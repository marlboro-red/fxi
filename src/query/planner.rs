use crate::index::types::Trigram;
use crate::query::parser::{Query, QueryNode};
use crate::utils::{query_trigrams, tokenize_query, tokenize_query_with_positions};

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
    /// Single-word literal narrowing: token postings unioned with trigram
    /// postings for substring recall. The trigram side is best-effort — when
    /// its trigrams are all stop-grams, the intersection of the word's
    /// sub-token postings (e.g. `foo_bar` -> foo ∩ bar) is used instead of
    /// degrading the candidate set to the whole corpus.
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
    /// -i: trigram narrowing is case-sensitive, so case-insensitive queries
    /// must narrow through the token/positional indexes (stored lowercased)
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

            QueryNode::Literal(text) => {
                // For single-word queries, use BOTH token lookup (fast exact match)
                // AND trigram search (substring match like ripgrep).
                // This ensures we find both exact tokens AND substrings.
                let is_single_word = !text.contains(char::is_whitespace) && text.len() >= 2;

                if is_single_word {
                    let trigrams = query_trigrams(text);

                    let steps = if !trigrams.is_empty() {
                        // Token lookup for fast exact match, trigrams for
                        // substring recall (best-effort under stop-grams)
                        vec![PlanStep::TokenOrTrigram {
                            token: text.to_lowercase(),
                            sub_tokens: tokenize_query(text),
                            trigrams,
                        }]
                    } else {
                        vec![PlanStep::TokenLookup(text.to_lowercase())]
                    };

                    (steps, Some(VerificationStep::Literal(text.clone())))
                } else {
                    // Under -i, skip case-sensitive trigram narrowing and use
                    // the lowercased token index instead
                    let trigrams = if self.case_insensitive {
                        Vec::new()
                    } else {
                        query_trigrams(text)
                    };

                    if trigrams.is_empty() {
                        // Short query or multiple short words, use token index
                        let tokens: Vec<_> = text
                            .split_whitespace()
                            .filter(|t| t.len() >= 2)
                            .map(|t| t.to_lowercase())
                            .collect();

                        if tokens.is_empty() {
                            return (Vec::new(), Some(VerificationStep::Literal(text.clone())));
                        }

                        let steps: Vec<_> = tokens.into_iter().map(PlanStep::TokenLookup).collect();

                        (steps, Some(VerificationStep::Literal(text.clone())))
                    } else {
                        // Multi-word query: use trigram narrowing
                        (
                            vec![PlanStep::TrigramIntersect(trigrams)],
                            Some(VerificationStep::Literal(text.clone())),
                        )
                    }
                }
            }

            QueryNode::BoostedLiteral { text, boost } => {
                // For single-word queries, use BOTH token lookup (fast exact match)
                // AND trigram search (substring match), same strategy as Literal
                let is_single_word = !text.contains(char::is_whitespace) && text.len() >= 2;

                if is_single_word {
                    let trigrams = query_trigrams(text);

                    let steps = if !trigrams.is_empty() {
                        // Token lookup for fast exact match, trigrams for
                        // substring recall (best-effort under stop-grams)
                        vec![PlanStep::TokenOrTrigram {
                            token: text.to_lowercase(),
                            sub_tokens: tokenize_query(text),
                            trigrams,
                        }]
                    } else {
                        vec![PlanStep::TokenLookup(text.to_lowercase())]
                    };

                    (
                        steps,
                        Some(VerificationStep::BoostedLiteral {
                            text: text.clone(),
                            boost: *boost,
                        }),
                    )
                } else {
                    // Under -i, skip case-sensitive trigram narrowing and use
                    // the lowercased token index instead
                    let trigrams = if self.case_insensitive {
                        Vec::new()
                    } else {
                        query_trigrams(text)
                    };

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

                        let steps: Vec<_> = tokens.into_iter().map(PlanStep::TokenLookup).collect();

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
            }

            QueryNode::Near { terms, distance } => {
                // For proximity search, narrow using trigrams from all terms.
                // Under -i, trigrams (case-sensitive) would miss other-case
                // docs, so fall through to token lookups (stored lowercased).
                let mut all_trigrams = Vec::new();
                if !self.case_insensitive {
                    for term in terms {
                        all_trigrams.extend(query_trigrams(term));
                    }
                    all_trigrams.sort_unstable();
                    all_trigrams.dedup();
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
                let phrase_tokens = tokenize_query_with_positions(text);

                let mut steps = Vec::new();

                if self.case_insensitive {
                    // Trigrams are case-sensitive, so narrow through the
                    // lowercased token/positional indexes instead
                    if phrase_tokens.len() >= 2 {
                        steps.push(PlanStep::PositionalPhrase(phrase_tokens));
                    } else if let Some((token, _)) = phrase_tokens.first() {
                        // Union with exact-case trigrams: they can't see
                        // other-case docs but still add substring matches the
                        // token index misses (e.g. the phrase inside a larger
                        // identifier), same as single-word Literal narrowing
                        let trigrams = query_trigrams(text);
                        if trigrams.is_empty() {
                            steps.push(PlanStep::TokenLookup(token.clone()));
                        } else {
                            steps.push(PlanStep::TokenOrTrigram {
                                token: token.clone(),
                                sub_tokens: tokenize_query(text),
                                trigrams,
                            });
                        }
                    }
                } else {
                    let trigrams = query_trigrams(text);
                    if !trigrams.is_empty() {
                        steps.push(PlanStep::TrigramIntersect(trigrams));
                    }

                    // Add positional phrase step if we have at least 2 tokens
                    if phrase_tokens.len() >= 2 {
                        steps.push(PlanStep::PositionalPhrase(phrase_tokens));
                    }
                }

                (
                    steps,
                    Some(VerificationStep::Phrase {
                        text: text.clone(),
                        case_insensitive: self.case_insensitive,
                    }),
                )
            }

            QueryNode::Regex(pattern) => {
                if self.case_insensitive {
                    // Case-sensitive trigram narrowing would miss other-case
                    // matches, so verify across all docs with (?i) applied
                    let ci_pattern = if pattern.starts_with("(?i)") {
                        pattern.clone()
                    } else {
                        format!("(?i){}", pattern)
                    };
                    return (Vec::new(), Some(VerificationStep::Regex(ci_pattern)));
                }

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

    fn plan_ci(input: &str) -> QueryPlan {
        let mut query = crate::query::parser::parse_query(input);
        query.options.case_insensitive = true;
        QueryPlan::from_query(&query)
    }

    #[test]
    fn test_ci_phrase_skips_trigram_narrowing() {
        // Trigrams are case-sensitive; a CI phrase must narrow through the
        // lowercased positional index instead
        let plan = plan_ci("\"static void\"");
        assert!(
            !plan
                .steps
                .iter()
                .any(|s| matches!(s, PlanStep::TrigramIntersect(_))),
            "CI phrase must not use case-sensitive trigram narrowing"
        );
        assert!(
            plan.steps
                .iter()
                .any(|s| matches!(s, PlanStep::PositionalPhrase(_))),
            "CI phrase should narrow via positional index"
        );
        match plan.verification {
            Some(VerificationStep::Phrase {
                case_insensitive, ..
            }) => assert!(case_insensitive),
            other => panic!("expected CI phrase verification, got {:?}", other),
        }
    }

    #[test]
    fn test_ci_single_token_phrase_uses_token_lookup() {
        // Narrowing is a union of the lowercased token index and exact-case
        // trigrams (the latter for substring recall inside identifiers)
        let plan = plan_ci("\"deadlock\"");
        let has_token_lookup = plan.steps.iter().any(|s| match s {
            PlanStep::TokenLookup(t) => t == "deadlock",
            PlanStep::TokenOrTrigram { token, .. } => token == "deadlock",
            _ => false,
        });
        assert!(
            has_token_lookup,
            "single-token CI phrase should narrow via token index, got {:?}",
            plan.steps
        );
    }

    #[test]
    fn test_cs_phrase_keeps_trigram_narrowing() {
        let query = crate::query::parser::parse_query("\"static void\"");
        let plan = QueryPlan::from_query(&query);
        assert!(plan
            .steps
            .iter()
            .any(|s| matches!(s, PlanStep::TrigramIntersect(_))));
        match plan.verification {
            Some(VerificationStep::Phrase {
                case_insensitive, ..
            }) => assert!(!case_insensitive),
            other => panic!("expected CS phrase verification, got {:?}", other),
        }
    }

    #[test]
    fn test_ci_regex_gets_case_flag() {
        let plan = plan_ci("re:/spin_lock\\(&\\w+/");
        match plan.verification {
            Some(VerificationStep::Regex(p)) => {
                assert!(p.starts_with("(?i)"), "CI regex should carry (?i): {p}")
            }
            other => panic!("expected regex verification, got {:?}", other),
        }
        assert!(
            plan.steps.is_empty(),
            "CI regex must not narrow with case-sensitive trigrams"
        );
    }
}
