use crate::index::reader::IndexReader;
use crate::index::types::{DocId, Language, SearchMatch};
use crate::query::parser::{Query, SortOrder};
use crate::query::planner::{FilterStep, PlanStep, QueryPlan, VerificationStep};
use crate::query::scorer::{ScoreContext, Scorer, ScoringWeights};
use anyhow::Result;
use globset::Glob;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::fs;

/// Query executor
pub struct QueryExecutor<'a> {
    reader: &'a IndexReader,
    scorer: Scorer,
}

impl<'a> QueryExecutor<'a> {
    pub fn new(reader: &'a IndexReader) -> Self {
        Self {
            reader,
            scorer: Scorer::with_defaults(),
        }
    }

    /// Create executor with custom scoring weights
    #[allow(dead_code)]
    pub fn with_scoring_weights(reader: &'a IndexReader, weights: ScoringWeights) -> Self {
        Self {
            reader,
            scorer: Scorer::new(weights),
        }
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

                // Modification time filters
                if let Some(min) = filter.mtime_min {
                    if doc.mtime < min {
                        continue;
                    }
                }
                if let Some(max) = filter.mtime_max {
                    if doc.mtime > max {
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

        // Extract search terms for filename matching
        let search_terms = Self::extract_search_terms(verification);

        // Extract boost factor from verification steps
        let boost = Self::extract_boost(verification);

        // Extract line filter from plan (if present)
        let (line_start, line_end) = Self::extract_line_filter(&plan.steps);

        // First pass: collect all matches grouped by doc_id
        let mut doc_matches: HashMap<DocId, Vec<(u32, String, usize, usize)>> = HashMap::new();

        for &doc_id in candidates {
            if let Some(doc) = self.reader.get_document(doc_id) {
                if let Some(full_path) = self.reader.get_full_path(doc) {
                    // Read file content
                    let content = match fs::read_to_string(&full_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    // Find matches
                    let mut file_matches = self.verify_content(&content, verification, doc_id);

                    // Apply line filter if specified
                    if line_start.is_some() || line_end.is_some() {
                        file_matches.retain(|(line_num, _, _, _)| {
                            let above_min = line_start.map(|min| *line_num >= min).unwrap_or(true);
                            let below_max = line_end.map(|max| *line_num <= max).unwrap_or(true);
                            above_min && below_max
                        });
                    }

                    if !file_matches.is_empty() {
                        doc_matches.insert(doc_id, file_matches);
                    }
                }
            }
        }

        // Second pass: calculate scores and build results
        for (doc_id, file_matches) in doc_matches {
            if let Some(doc) = self.reader.get_document(doc_id) {
                let path = self.reader.get_path(doc).cloned().unwrap_or_default();

                // Build score context
                let filename_match = search_terms
                    .iter()
                    .any(|term| Scorer::term_in_filename(&path, term));

                let score_ctx = ScoreContext {
                    match_count: file_matches.len(),
                    filename_match,
                    depth: Scorer::path_depth(&path),
                    mtime: doc.mtime,
                    boost,
                };

                let score = self.scorer.calculate_score(&score_ctx);

                // Create matches with calculated score
                for (line_num, line_content, start, end) in file_matches {
                    matches.push(SearchMatch {
                        doc_id,
                        path: path.clone(),
                        line_number: line_num,
                        line_content,
                        match_start: start,
                        match_end: end,
                        score,
                    });
                }
            }
        }

        Ok(matches)
    }

    /// Extract boost factor from verification steps
    fn extract_boost(verification: &VerificationStep) -> f32 {
        match verification {
            VerificationStep::BoostedLiteral { boost, .. } => *boost,
            VerificationStep::And(steps) | VerificationStep::Or(steps) => {
                // Return the maximum boost from all steps
                steps
                    .iter()
                    .map(Self::extract_boost)
                    .fold(1.0_f32, |a, b| a.max(b))
            }
            _ => 1.0,
        }
    }

    /// Extract line filter from plan steps
    fn extract_line_filter(steps: &[PlanStep]) -> (Option<u32>, Option<u32>) {
        for step in steps {
            if let PlanStep::Filter(filter) = step {
                if filter.line_start.is_some() || filter.line_end.is_some() {
                    return (filter.line_start, filter.line_end);
                }
            }
        }
        (None, None)
    }

    /// Extract search terms from verification step for filename matching
    fn extract_search_terms(verification: &VerificationStep) -> Vec<String> {
        let mut terms = Vec::new();
        Self::collect_terms(verification, &mut terms);
        terms
    }

    /// Recursively collect terms from verification steps
    fn collect_terms(verification: &VerificationStep, terms: &mut Vec<String>) {
        match verification {
            VerificationStep::Literal(text) | VerificationStep::Phrase(text) => {
                // Split into words and collect meaningful terms
                for word in text.split_whitespace() {
                    if word.len() >= 2 {
                        terms.push(word.to_string());
                    }
                }
            }
            VerificationStep::BoostedLiteral { text, .. } => {
                // Same as literal
                for word in text.split_whitespace() {
                    if word.len() >= 2 {
                        terms.push(word.to_string());
                    }
                }
            }
            VerificationStep::Regex(pattern) => {
                // Try to extract literal parts from regex
                let literal = pattern
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>();
                if literal.len() >= 2 {
                    terms.push(literal);
                }
            }
            VerificationStep::Near { terms: near_terms, .. } => {
                // Include all proximity search terms
                for term in near_terms {
                    if term.len() >= 2 {
                        terms.push(term.clone());
                    }
                }
            }
            VerificationStep::And(steps) | VerificationStep::Or(steps) => {
                for step in steps {
                    Self::collect_terms(step, terms);
                }
            }
            VerificationStep::Not(_) => {
                // Don't include negated terms in filename matching
            }
        }
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
            VerificationStep::BoostedLiteral { text, boost: _ } => {
                // Boosted literal: same matching as regular literal
                // The boost is applied during scoring, not matching
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
            VerificationStep::Near { terms, distance } => {
                self.find_proximity_matches(content, terms, *distance, doc_id)
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

    /// Find proximity matches: all terms must appear within distance lines of each other
    fn find_proximity_matches(
        &self,
        content: &str,
        terms: &[String],
        distance: u32,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        if terms.is_empty() {
            return Vec::new();
        }

        // Collect line numbers for each term
        let mut term_lines: Vec<Vec<u32>> = Vec::with_capacity(terms.len());

        for term in terms {
            let term_lower = term.to_lowercase();
            let mut lines_with_term = Vec::new();

            for (line_num, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&term_lower) {
                    lines_with_term.push((line_num + 1) as u32);
                }
            }

            if lines_with_term.is_empty() {
                // One of the terms doesn't exist in the file
                return Vec::new();
            }

            term_lines.push(lines_with_term);
        }

        // Find line combinations where all terms are within distance
        let mut matches = Vec::new();
        let lines: Vec<&str> = content.lines().collect();

        // Start with lines containing the first term
        for &first_line in &term_lines[0] {
            let mut all_within_distance = true;

            // Check if all other terms have a match within distance
            for other_term_lines in term_lines.iter().skip(1) {
                let has_nearby = other_term_lines.iter().any(|&other_line| {
                    let diff = if first_line > other_line {
                        first_line - other_line
                    } else {
                        other_line - first_line
                    };
                    diff <= distance
                });

                if !has_nearby {
                    all_within_distance = false;
                    break;
                }
            }

            if all_within_distance {
                // Found a valid proximity match - return the first term's match
                let line_idx = (first_line - 1) as usize;
                if let Some(line_content) = lines.get(line_idx) {
                    let term_lower = terms[0].to_lowercase();
                    if let Some(pos) = line_content.to_lowercase().find(&term_lower) {
                        matches.push((
                            first_line,
                            line_content.to_string(),
                            pos,
                            pos + terms[0].len(),
                        ));
                    }
                }
            }
        }

        matches
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

        let search_needle = if case_sensitive {
            needle.to_string()
        } else {
            needle.to_lowercase()
        };

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
