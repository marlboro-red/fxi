use super::regex_plan::regex_steps;
use crate::index::types::Trigram;
use crate::query::parser::{Query, QueryNode};

/// Query execution plan
#[derive(Debug)]
pub struct QueryPlan {
    pub steps: Vec<PlanStep>,
    pub verification: Option<VerificationStep>,
}

/// Individual plan step
#[derive(Debug)]
pub enum PlanStep {
    /// Fetch postings for trigrams and intersect
    TrigramIntersect(Vec<Trigram>),
    /// Fetch postings for a token
    #[allow(dead_code)] // Reserved for explicit whole-token semantics.
    TokenLookup(String),
    /// Union results from sub-plans
    Union(Vec<QueryPlan>),
    /// Single-word literal narrowing: token postings unioned with trigram
    /// postings for substring recall. The trigram side is best-effort — when
    /// its trigrams are all stop-grams, the intersection of the word's
    /// sub-token postings (e.g. `foo_bar` -> foo ∩ bar) is used instead of
    /// degrading the candidate set to the whole corpus.
    #[allow(dead_code)] // Not safe for arbitrary insensitive substrings.
    TokenOrTrigram {
        token: String,
        sub_tokens: Vec<String>,
        trigrams: Vec<Trigram>,
    },
    /// Intersect results from sub-plans
    #[allow(dead_code)]
    Intersect(Vec<QueryPlan>),
    /// Exclude results matching sub-plan
    Exclude(Box<QueryPlan>),
    /// Apply document filters
    Filter(FilterStep),
    /// Positional phrase resolution: check token adjacency from position index
    #[allow(dead_code)] // Requires explicit token-boundary semantics.
    PositionalPhrase(Vec<(String, u32)>),
}

/// Filter step for post-narrowing
#[derive(Debug)]
pub struct FilterStep {
    pub path_glob: Option<String>,
    pub filename: Option<String>,
    pub extension: Option<String>,
    pub language: Option<String>,
    pub size_min: Option<u64>,
    pub size_max: Option<u64>,
    pub mtime_min: Option<u64>,
    pub mtime_max: Option<u64>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
}

/// Verification step (run against candidate documents)
#[derive(Debug, Clone)]
pub enum VerificationStep {
    /// Literal substring match
    Literal(String),
    /// Literal with boost factor for scoring
    BoostedLiteral {
        text: String,
        boost: f32,
    },
    /// Exact phrase match (case-insensitive when -i is set)
    Phrase {
        text: String,
        case_insensitive: bool,
    },
    /// Regex match
    Regex(String),
    /// Proximity search: terms must appear within distance lines
    Near {
        terms: Vec<String>,
        distance: u32,
    },
    /// Compound verification
    And(Vec<VerificationStep>),
    Or(Vec<VerificationStep>),
    Not(Box<VerificationStep>),
}

impl QueryPlan {
    /// Create a query plan from a parsed query
    pub fn from_query(query: &Query) -> Self {
        let mut planner = QueryPlanner::new(query.options.case_insensitive);
        planner.plan(query)
    }
}

/// Query planner
struct QueryPlanner {
    steps: Vec<PlanStep>,
    /// Apply Unicode-insensitive regex semantics to explicit phrases/regexes.
    case_insensitive: bool,
}

impl QueryPlanner {
    fn new(case_insensitive: bool) -> Self {
        Self {
            steps: Vec::new(),
            case_insensitive,
        }
    }

    fn plan(&mut self, query: &Query) -> QueryPlan {
        // Plan the main query
        let (narrowing_steps, verification) = self.plan_node(&query.root);
        self.steps.extend(narrowing_steps);

        // Add filter step if we have any filters. Filters run after the
        // narrowing steps: per-document checks (glob matching, metadata
        // comparisons) are far more expensive than the index lookups above,
        // so they should only see the already-narrowed candidate set instead
        // of scanning every document in the index.
        if query.filters.path.is_some()
            || query.filters.filename.is_some()
            || query.filters.ext.is_some()
            || query.filters.lang.is_some()
            || query.filters.size_min.is_some()
            || query.filters.size_max.is_some()
            || query.filters.mtime_min.is_some()
            || query.filters.mtime_max.is_some()
            || query.filters.line_start.is_some()
            || query.filters.line_end.is_some()
        {
            self.steps.push(PlanStep::Filter(FilterStep {
                path_glob: query.filters.path.clone(),
                filename: query.filters.filename.clone(),
                extension: query.filters.ext.clone(),
                language: query.filters.lang.clone(),
                size_min: query.filters.size_min,
                size_max: query.filters.size_max,
                mtime_min: query.filters.mtime_min,
                mtime_max: query.filters.mtime_max,
                line_start: query.filters.line_start,
                line_end: query.filters.line_end,
            }));
        }

        QueryPlan {
            steps: self.steps.drain(..).collect(),
            verification,
        }
    }

    #[allow(clippy::only_used_in_recursion)]
    fn plan_node(&mut self, node: &QueryNode) -> (Vec<PlanStep>, Option<VerificationStep>) {
        match node {
            QueryNode::Empty => (Vec::new(), None),

            QueryNode::Literal(text) => (
                literal_steps(text, true),
                Some(VerificationStep::Literal(text.clone())),
            ),
            QueryNode::BoostedLiteral { text, boost } => (
                literal_steps(text, true),
                Some(VerificationStep::BoostedLiteral {
                    text: text.clone(),
                    boost: *boost,
                }),
            ),
            QueryNode::Near { terms, distance } => (
                terms
                    .iter()
                    .flat_map(|term| literal_steps(term, true))
                    .collect(),
                Some(VerificationStep::Near {
                    terms: terms.clone(),
                    distance: *distance,
                }),
            ),
            QueryNode::Phrase(text) => (
                literal_steps(text, self.case_insensitive),
                Some(VerificationStep::Phrase {
                    text: text.clone(),
                    case_insensitive: self.case_insensitive,
                }),
            ),
            QueryNode::Regex(pattern) => {
                let pattern = if self.case_insensitive {
                    format!("(?i){pattern}")
                } else {
                    pattern.clone()
                };
                (
                    regex_steps(&pattern),
                    Some(VerificationStep::Regex(pattern)),
                )
            }

            QueryNode::And(nodes) => {
                let mut all_steps = Vec::new();
                let mut verifications = Vec::new();

                for node in nodes {
                    let (steps, verification) = self.plan_node(node);
                    all_steps.extend(steps);
                    if let Some(v) = verification {
                        verifications.push(v);
                    }
                }

                let verification = if verifications.len() == 1 {
                    verifications.pop()
                } else if verifications.is_empty() {
                    None
                } else {
                    Some(VerificationStep::And(verifications))
                };

                (all_steps, verification)
            }

            QueryNode::Or(nodes) => {
                let mut sub_plans = Vec::new();
                let mut verifications = Vec::new();

                for node in nodes {
                    let (steps, verification) = self.plan_node(node);
                    // An empty plan means all documents, not no documents.
                    // Keeping it in the union preserves matches from short
                    // literals and regexes that cannot use the index.
                    sub_plans.push(QueryPlan {
                        steps,
                        verification: verification.clone(),
                    });
                    if let Some(v) = verification {
                        verifications.push(v);
                    }
                }

                let verification = if verifications.len() == 1 {
                    verifications.pop()
                } else if verifications.is_empty() {
                    None
                } else {
                    Some(VerificationStep::Or(verifications))
                };

                if sub_plans.is_empty() {
                    (Vec::new(), verification)
                } else {
                    (vec![PlanStep::Union(sub_plans)], verification)
                }
            }

            QueryNode::Not(inner) => {
                let (steps, verification) = self.plan_node(inner);
                let exclude_plan = QueryPlan {
                    steps,
                    verification: verification.clone(),
                };

                let verify = verification.map(|v| VerificationStep::Not(Box::new(v)));

                (vec![PlanStep::Exclude(Box::new(exclude_plan))], verify)
            }
        }
    }
}

fn literal_steps(text: &str, insensitive: bool) -> Vec<PlanStep> {
    let escaped = regex::escape(text);
    regex_steps(&if insensitive {
        format!("(?i:{escaped})")
    } else {
        escaped
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_non_prefix_and_insensitive_constraints() {
        for pattern in [
            ".*needle",
            "needle|other",
            "(?i)serverassert",
            "[aA]bc",
            "foo.*bar",
        ] {
            assert!(!regex_steps(pattern).is_empty(), "{pattern}");
        }
        for pattern in ["x|needle", "(?:needle)?", ".*", "[a-z]", "a?"] {
            assert!(regex_steps(pattern).is_empty(), "{pattern}");
        }
    }

    #[test]
    fn insensitive_phrases_keep_unicode_verification() {
        let mut query = crate::query::parser::parse_query("\"static void\"");
        query.options.case_insensitive = true;
        let plan = QueryPlan::from_query(&query);
        assert!(!plan.steps.is_empty());
        assert!(matches!(
            plan.verification,
            Some(VerificationStep::Phrase {
                case_insensitive: true,
                ..
            })
        ));
    }
}
