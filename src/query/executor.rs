use crate::index::reader::IndexReader;
use crate::index::types::{DocId, Language, SearchMatch};
use crate::query::parser::{Query, SortOrder};
use crate::query::planner::{FilterStep, PlanStep, QueryPlan, VerificationStep};
use anyhow::Result;
use globset::Glob;
use regex::Regex;
use std::collections::HashSet;
use std::fs;

/// Query executor
pub struct QueryExecutor<'a> {
    reader: &'a IndexReader,
}

impl<'a> QueryExecutor<'a> {
    pub fn new(reader: &'a IndexReader) -> Self {
        Self { reader }
    }

    /// Execute a query and return matches
    pub fn execute(&self, query: &Query) -> Result<Vec<SearchMatch>> {
        let plan = QueryPlan::from_query(query);
        let candidates = self.execute_plan(&plan)?;

        // Verify candidates
        let verified = self.verify_candidates(&candidates, &plan)?;

        // Sort results
        let mut results = verified;
        self.sort_results(&mut results, query.options.sort);

        // Apply limit
        results.truncate(query.options.limit);

        Ok(results)
    }

    /// Execute the narrowing phase
    fn execute_plan(&self, plan: &QueryPlan) -> Result<HashSet<DocId>> {
        let mut candidates: Option<HashSet<DocId>> = None;
        let mut exclude_set: HashSet<DocId> = HashSet::new();

        for step in &plan.steps {
            match step {
                PlanStep::TrigramIntersect(trigrams) => {
                    // Get postings for each trigram and intersect
                    let mut trigram_sets: Vec<HashSet<DocId>> = trigrams
                        .iter()
                        .filter(|&&t| !self.reader.is_stop_gram(t))
                        .map(|&t| self.reader.get_trigram_docs(t).into_iter().collect())
                        .collect();

                    // Sort by size for efficient intersection
                    trigram_sets.sort_by_key(|s| s.len());

                    if let Some(first) = trigram_sets.first() {
                        let mut result = first.clone();
                        for set in trigram_sets.iter().skip(1) {
                            result.retain(|id| set.contains(id));
                        }

                        candidates = Some(match candidates {
                            Some(existing) => existing
                                .intersection(&result)
                                .copied()
                                .collect(),
                            None => result,
                        });
                    }
                }

                PlanStep::TokenLookup(token) => {
                    let docs: HashSet<DocId> =
                        self.reader.get_token_docs(token).into_iter().collect();

                    candidates = Some(match candidates {
                        Some(existing) => existing.intersection(&docs).copied().collect(),
                        None => docs,
                    });
                }

                PlanStep::Union(sub_plans) => {
                    let mut union: HashSet<DocId> = HashSet::new();
                    for sub_plan in sub_plans {
                        let sub_candidates = self.execute_plan(sub_plan)?;
                        union.extend(sub_candidates);
                    }

                    candidates = Some(match candidates {
                        Some(existing) => existing.intersection(&union).copied().collect(),
                        None => union,
                    });
                }

                PlanStep::Intersect(sub_plans) => {
                    let mut intersection: Option<HashSet<DocId>> = None;
                    for sub_plan in sub_plans {
                        let sub_candidates = self.execute_plan(sub_plan)?;
                        intersection = Some(match intersection {
                            Some(existing) => existing
                                .intersection(&sub_candidates)
                                .copied()
                                .collect(),
                            None => sub_candidates,
                        });
                    }

                    if let Some(int) = intersection {
                        candidates = Some(match candidates {
                            Some(existing) => existing.intersection(&int).copied().collect(),
                            None => int,
                        });
                    }
                }

                PlanStep::Exclude(sub_plan) => {
                    let excluded = self.execute_plan(sub_plan)?;
                    exclude_set.extend(excluded);
                }

                PlanStep::Filter(filter) => {
                    // Apply document filters
                    let filtered = self.apply_filter(filter, candidates.as_ref())?;
                    candidates = Some(filtered);
                }
            }
        }

        // Remove excluded documents
        if let Some(ref mut cands) = candidates {
            cands.retain(|id| !exclude_set.contains(id));
        }

        // If no narrowing steps, start with all valid documents
        Ok(candidates.unwrap_or_else(|| self.reader.valid_doc_ids().into_iter().collect()))
    }

    /// Apply document filters
    fn apply_filter(
        &self,
        filter: &FilterStep,
        candidates: Option<&HashSet<DocId>>,
    ) -> Result<HashSet<DocId>> {
        let docs = if let Some(cands) = candidates {
            cands.iter().copied().collect::<Vec<_>>()
        } else {
            self.reader.valid_doc_ids()
        };

        let path_matcher = filter.path_glob.as_ref().map(|g| {
            Glob::new(g)
                .unwrap_or_else(|_| Glob::new("*").unwrap())
                .compile_matcher()
        });

        let mut result = HashSet::new();

        for doc_id in docs {
            if let Some(doc) = self.reader.get_document(doc_id) {
                // Skip stale/tombstone
                if !doc.is_valid() {
                    continue;
                }

                // Path filter
                if let Some(ref matcher) = path_matcher {
                    if let Some(path) = self.reader.get_path(doc) {
                        if !matcher.is_match(path) {
                            continue;
                        }
                    }
                }

                // Extension filter
                if let Some(ref ext) = filter.extension {
                    if let Some(path) = self.reader.get_path(doc) {
                        let file_ext = path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("");
                        if !file_ext.eq_ignore_ascii_case(ext) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }

                // Language filter
                if let Some(ref lang) = filter.language {
                    let lang_enum = parse_language(lang);
                    if doc.language != lang_enum {
                        continue;
                    }
                }

                // Size filters
                if let Some(min) = filter.size_min {
                    if doc.size < min {
                        continue;
                    }
                }
                if let Some(max) = filter.size_max {
                    if doc.size > max {
                        continue;
                    }
                }

                result.insert(doc_id);
            }
        }

        Ok(result)
    }

    /// Verify candidates against actual file content
    fn verify_candidates(
        &self,
        candidates: &HashSet<DocId>,
        plan: &QueryPlan,
    ) -> Result<Vec<SearchMatch>> {
        let mut matches = Vec::new();

        let verification = match &plan.verification {
            Some(v) => v,
            None => return Ok(matches),
        };

        for &doc_id in candidates {
            if let Some(doc) = self.reader.get_document(doc_id) {
                if let Some(full_path) = self.reader.get_full_path(doc) {
                    // Read file content
                    let content = match fs::read_to_string(&full_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    // Find matches
                    let file_matches = self.verify_content(&content, verification, doc_id);

                    for (line_num, line_content, start, end) in file_matches {
                        let path = self.reader.get_path(doc).cloned().unwrap_or_default();

                        matches.push(SearchMatch {
                            doc_id,
                            path,
                            line_number: line_num,
                            line_content,
                            match_start: start,
                            match_end: end,
                            score: 1.0,
                        });
                    }
                }
            }
        }

        Ok(matches)
    }

    /// Verify content against a verification step
    fn verify_content(
        &self,
        content: &str,
        verification: &VerificationStep,
        doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        match verification {
            VerificationStep::Literal(text) => {
                self.find_literal_matches(content, text, false, doc_id)
            }
            VerificationStep::Phrase(text) => {
                self.find_literal_matches(content, text, true, doc_id)
            }
            VerificationStep::Regex(pattern) => {
                if let Ok(re) = Regex::new(pattern) {
                    self.find_regex_matches(content, &re, doc_id)
                } else {
                    Vec::new()
                }
            }
            VerificationStep::And(steps) => {
                // All must have at least one match
                let mut all_matches: Option<Vec<(u32, String, usize, usize)>> = None;

                for step in steps {
                    let step_matches = self.verify_content(content, step, doc_id);
                    if step_matches.is_empty() {
                        return Vec::new();
                    }

                    all_matches = Some(match all_matches {
                        Some(mut existing) => {
                            existing.extend(step_matches);
                            existing
                        }
                        None => step_matches,
                    });
                }

                all_matches.unwrap_or_default()
            }
            VerificationStep::Or(steps) => {
                let mut all_matches = Vec::new();
                for step in steps {
                    all_matches.extend(self.verify_content(content, step, doc_id));
                }
                all_matches
            }
            VerificationStep::Not(inner) => {
                let inner_matches = self.verify_content(content, inner, doc_id);
                if inner_matches.is_empty() {
                    // Return a "match" indicating the file doesn't contain the pattern
                    vec![(1, content.lines().next().unwrap_or("").to_string(), 0, 0)]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Find literal string matches
    fn find_literal_matches(
        &self,
        content: &str,
        needle: &str,
        case_sensitive: bool,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        let mut matches = Vec::new();

        let (search_content, search_needle);
        if case_sensitive {
            search_content = content.to_string();
            search_needle = needle.to_string();
        } else {
            search_content = content.to_lowercase();
            search_needle = needle.to_lowercase();
        }

        for (line_num, line) in content.lines().enumerate() {
            let search_line = if case_sensitive {
                line.to_string()
            } else {
                line.to_lowercase()
            };

            if let Some(pos) = search_line.find(&search_needle) {
                matches.push((
                    (line_num + 1) as u32,
                    line.to_string(),
                    pos,
                    pos + needle.len(),
                ));
            }
        }

        matches
    }

    /// Find regex matches
    fn find_regex_matches(
        &self,
        content: &str,
        regex: &Regex,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        let mut matches = Vec::new();

        for (line_num, line) in content.lines().enumerate() {
            if let Some(m) = regex.find(line) {
                matches.push((
                    (line_num + 1) as u32,
                    line.to_string(),
                    m.start(),
                    m.end(),
                ));
            }
        }

        matches
    }

    /// Sort results by the specified order
    fn sort_results(&self, results: &mut [SearchMatch], order: SortOrder) {
        match order {
            SortOrder::Score => {
                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortOrder::Recency => {
                results.sort_by(|a, b| {
                    let mtime_a = self
                        .reader
                        .get_document(a.doc_id)
                        .map(|d| d.mtime)
                        .unwrap_or(0);
                    let mtime_b = self
                        .reader
                        .get_document(b.doc_id)
                        .map(|d| d.mtime)
                        .unwrap_or(0);
                    mtime_b.cmp(&mtime_a)
                });
            }
            SortOrder::Path => {
                results.sort_by(|a, b| a.path.cmp(&b.path));
            }
        }
    }
}

/// Parse language string to enum
fn parse_language(lang: &str) -> Language {
    match lang.to_lowercase().as_str() {
        "rust" | "rs" => Language::Rust,
        "python" | "py" => Language::Python,
        "javascript" | "js" => Language::JavaScript,
        "typescript" | "ts" => Language::TypeScript,
        "go" => Language::Go,
        "c" => Language::C,
        "cpp" | "c++" => Language::Cpp,
        "java" => Language::Java,
        "ruby" | "rb" => Language::Ruby,
        "shell" | "sh" | "bash" => Language::Shell,
        _ => Language::Unknown,
    }
}
