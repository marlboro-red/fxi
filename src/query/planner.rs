use crate::index::types::Trigram;
use crate::query::parser::{Query, QueryNode};
use crate::utils::query_trigrams;

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
    TokenLookup(String),
    /// Union results from sub-plans
    Union(Vec<QueryPlan>),
    /// Intersect results from sub-plans
    #[allow(dead_code)]
    Intersect(Vec<QueryPlan>),
    /// Exclude results matching sub-plan
    Exclude(Box<QueryPlan>),
    /// Apply document filters
    Filter(FilterStep),
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
    BoostedLiteral { text: String, boost: f32 },
    /// Exact phrase match
    Phrase(String),
    /// Regex match
    Regex(String),
    /// Proximity search: terms must appear within distance lines
    Near { terms: Vec<String>, distance: u32 },
    /// Compound verification
    And(Vec<VerificationStep>),
    Or(Vec<VerificationStep>),
    Not(Box<VerificationStep>),
}

impl QueryPlan {
    /// Create a query plan from a parsed query
    pub fn from_query(query: &Query) -> Self {
        let mut planner = QueryPlanner::new();
        planner.plan(query)
    }
}

/// Query planner
struct QueryPlanner {
    steps: Vec<PlanStep>,
}

impl QueryPlanner {
    fn new() -> Self {
        Self { steps: Vec::new() }
    }

    fn plan(&mut self, query: &Query) -> QueryPlan {
        // Add filter step if we have any filters
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

        // Plan the main query
        let (narrowing_steps, verification) = self.plan_node(&query.root);
        self.steps.extend(narrowing_steps);

        QueryPlan {
            steps: self.steps.drain(..).collect(),
            verification,
        }
    }

    fn plan_node(&mut self, node: &QueryNode) -> (Vec<PlanStep>, Option<VerificationStep>) {
        match node {
            QueryNode::Empty => (Vec::new(), None),

            QueryNode::Literal(text) => {
                let trigrams = query_trigrams(text);

                if trigrams.is_empty() {
                    // Short query, use token index
                    let tokens: Vec<_> = text
                        .split_whitespace()
                        .filter(|t| t.len() >= 2)
                        .map(|t| t.to_lowercase())
                        .collect();

                    if tokens.is_empty() {
                        return (Vec::new(), Some(VerificationStep::Literal(text.clone())));
                    }

                    let steps: Vec<_> = tokens
                        .into_iter()
                        .map(PlanStep::TokenLookup)
                        .collect();

                    (steps, Some(VerificationStep::Literal(text.clone())))
                } else {
                    // Use trigram narrowing
                    (
                        vec![PlanStep::TrigramIntersect(trigrams)],
                        Some(VerificationStep::Literal(text.clone())),
                    )
                }
            }

            QueryNode::BoostedLiteral { text, boost } => {
                let trigrams = query_trigrams(text);

                if trigrams.is_empty() {
                    let tokens: Vec<_> = text
                        .split_whitespace()
                        .filter(|t| t.len() >= 2)
                        .map(|t| t.to_lowercase())
                        .collect();

                    if tokens.is_empty() {
                        return (
                            Vec::new(),
                            Some(VerificationStep::BoostedLiteral {
                                text: text.clone(),
                                boost: *boost,
                            }),
                        );
                    }

                    let steps: Vec<_> = tokens
                        .into_iter()
                        .map(PlanStep::TokenLookup)
                        .collect();

                    (
                        steps,
                        Some(VerificationStep::BoostedLiteral {
                            text: text.clone(),
                            boost: *boost,
                        }),
                    )
                } else {
                    (
                        vec![PlanStep::TrigramIntersect(trigrams)],
                        Some(VerificationStep::BoostedLiteral {
                            text: text.clone(),
                            boost: *boost,
                        }),
                    )
                }
            }

            QueryNode::Near { terms, distance } => {
                // For proximity search, narrow using trigrams from all terms
                let mut all_trigrams = Vec::new();
                for term in terms {
                    all_trigrams.extend(query_trigrams(term));
                }

                let steps = if all_trigrams.is_empty() {
                    // Use token lookups for short terms
                    terms
                        .iter()
                        .filter(|t| t.len() >= 2)
                        .map(|t| PlanStep::TokenLookup(t.to_lowercase()))
                        .collect()
                } else {
                    vec![PlanStep::TrigramIntersect(all_trigrams)]
                };

                (
                    steps,
                    Some(VerificationStep::Near {
                        terms: terms.clone(),
                        distance: *distance,
                    }),
                )
            }

            QueryNode::Phrase(text) => {
                let trigrams = query_trigrams(text);

                if trigrams.is_empty() {
                    (Vec::new(), Some(VerificationStep::Phrase(text.clone())))
                } else {
                    (
                        vec![PlanStep::TrigramIntersect(trigrams)],
                        Some(VerificationStep::Phrase(text.clone())),
                    )
                }
            }

            QueryNode::Regex(pattern) => {
                // Try to extract literal prefix for narrowing
                let literal_prefix = extract_regex_prefix(pattern);

                let steps = if let Some(prefix) = literal_prefix {
                    let trigrams = query_trigrams(&prefix);
                    if !trigrams.is_empty() {
                        vec![PlanStep::TrigramIntersect(trigrams)]
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };

                (steps, Some(VerificationStep::Regex(pattern.clone())))
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
                    if !steps.is_empty() {
                        sub_plans.push(QueryPlan {
                            steps,
                            verification: verification.clone(),
                        });
                    }
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

/// Extract literal prefix from regex for narrowing
fn extract_regex_prefix(pattern: &str) -> Option<String> {
    let mut prefix = String::new();
    let mut chars = pattern.chars().peekable();

    // Skip leading ^
    if chars.peek() == Some(&'^') {
        chars.next();
    }

    while let Some(ch) = chars.next() {
        match ch {
            // Escape sequences
            '\\' => {
                if let Some(escaped) = chars.next() {
                    match escaped {
                        'n' => prefix.push('\n'),
                        't' => prefix.push('\t'),
                        'r' => prefix.push('\r'),
                        c if c.is_ascii_alphanumeric() => {
                            // Special regex escape, stop
                            break;
                        }
                        c => prefix.push(c),
                    }
                } else {
                    break;
                }
            }
            // Regex metacharacters - stop here
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '$' => break,
            // Regular character
            c => prefix.push(c),
        }
    }

    if prefix.len() >= 3 {
        Some(prefix)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_regex_prefix() {
        assert_eq!(
            extract_regex_prefix("hello.*world"),
            Some("hello".to_string())
        );
        assert_eq!(extract_regex_prefix("^foo"), Some("foo".to_string()));
        assert_eq!(extract_regex_prefix("ab"), None); // Too short
    }
}
