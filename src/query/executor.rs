use crate::index::reader::IndexReader;
use crate::index::types::{DocId, Language, SearchMatch};
use crate::query::parser::{Query, SortOrder};
use crate::query::planner::{FilterStep, PlanStep, QueryPlan, VerificationStep};
use crate::query::scorer::{ScoreContext, Scorer, ScoringWeights};
use anyhow::Result;
use globset::Glob;
use rayon::prelude::*;
use regex::Regex;
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Thread-safe regex cache for avoiding repeated compilation
/// Uses a simple LRU-like approach with a bounded size
struct RegexCache {
    cache: Mutex<HashMap<String, Arc<Regex>>>,
    max_size: usize,
}

impl RegexCache {
    fn new(max_size: usize) -> Self {
        Self {
            cache: Mutex::new(HashMap::with_capacity(max_size)),
            max_size,
        }
    }

    /// Get or compile a regex pattern
    fn get_or_compile(&self, pattern: &str) -> Option<Arc<Regex>> {
        // Check cache first
        {
            let cache = self.cache.lock().ok()?;
            if let Some(re) = cache.get(pattern) {
                return Some(Arc::clone(re));
            }
        }

        // Compile the regex
        let re = Regex::new(pattern).ok()?;
        let arc_re = Arc::new(re);

        // Try to cache it
        if let Ok(mut cache) = self.cache.lock() {
            // Evict oldest entries if cache is full
            if cache.len() >= self.max_size {
                // Simple eviction: remove a random entry
                if let Some(key) = cache.keys().next().cloned() {
                    cache.remove(&key);
                }
            }
            cache.insert(pattern.to_string(), Arc::clone(&arc_re));
        }

        Some(arc_re)
    }
}

/// Global regex cache for the executor
static REGEX_CACHE: std::sync::OnceLock<RegexCache> = std::sync::OnceLock::new();

fn get_regex_cache() -> &'static RegexCache {
    REGEX_CACHE.get_or_init(|| RegexCache::new(64))
}

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

    /// Execute the narrowing phase using RoaringBitmap for efficient set operations
    fn execute_plan(&self, plan: &QueryPlan) -> Result<RoaringBitmap> {
        let mut candidates: Option<RoaringBitmap> = None;
        let mut exclude_set = RoaringBitmap::new();

        for step in &plan.steps {
            match step {
                PlanStep::TrigramIntersect(trigrams) => {
                    // Filter out stop-grams
                    let filtered_trigrams: Vec<_> = trigrams
                        .iter()
                        .filter(|&&t| !self.reader.is_stop_gram(t))
                        .copied()
                        .collect();

                    if !filtered_trigrams.is_empty() {
                        // Use bloom filter optimized path for multi-trigram queries
                        // This skips segments that definitely don't contain all trigrams
                        let result = self.reader.get_trigram_docs_with_bloom(&filtered_trigrams);

                        candidates = Some(match candidates {
                            Some(existing) => existing & result,
                            None => result,
                        });
                    }
                }

                PlanStep::TokenLookup(token) => {
                    let docs = self.reader.get_token_docs(token);

                    candidates = Some(match candidates {
                        Some(existing) => existing & docs,
                        None => docs,
                    });
                }

                PlanStep::Union(sub_plans) => {
                    let mut union = RoaringBitmap::new();
                    for sub_plan in sub_plans {
                        let sub_candidates = self.execute_plan(sub_plan)?;
                        union |= sub_candidates;
                    }

                    candidates = Some(match candidates {
                        Some(existing) => existing & union,
                        None => union,
                    });
                }

                PlanStep::Intersect(sub_plans) => {
                    let mut intersection: Option<RoaringBitmap> = None;
                    for sub_plan in sub_plans {
                        let sub_candidates = self.execute_plan(sub_plan)?;
                        intersection = Some(match intersection {
                            Some(existing) => existing & sub_candidates,
                            None => sub_candidates,
                        });
                    }

                    if let Some(int) = intersection {
                        candidates = Some(match candidates {
                            Some(existing) => existing & int,
                            None => int,
                        });
                    }
                }

                PlanStep::Exclude(sub_plan) => {
                    let excluded = self.execute_plan(sub_plan)?;
                    exclude_set |= excluded;
                }

                PlanStep::Filter(filter) => {
                    // Apply document filters
                    let filtered = self.apply_filter(filter, candidates.as_ref())?;
                    candidates = Some(filtered);
                }
            }
        }

        // Remove excluded documents using bitmap difference
        if let Some(ref mut cands) = candidates {
            *cands -= exclude_set;
        }

        // If no narrowing steps, start with all valid documents
        Ok(candidates.unwrap_or_else(|| self.reader.valid_doc_ids()))
    }

    /// Apply document filters using RoaringBitmap
    fn apply_filter(
        &self,
        filter: &FilterStep,
        candidates: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap> {
        // Get the set of doc_ids to filter
        let owned_valid_docs;
        let doc_ids: Vec<u32> = if let Some(cands) = candidates {
            cands.iter().collect()
        } else {
            owned_valid_docs = self.reader.valid_doc_ids();
            owned_valid_docs.iter().collect()
        };

        let path_matcher = filter.path_glob.as_ref().map(|g| {
            Glob::new(g)
                .unwrap_or_else(|_| Glob::new("*").unwrap())
                .compile_matcher()
        });

        let mut result = RoaringBitmap::new();

        for doc_id in doc_ids {
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

    /// Verify candidates against actual file content using parallel processing
    fn verify_candidates(
        &self,
        candidates: &RoaringBitmap,
        plan: &QueryPlan,
    ) -> Result<Vec<SearchMatch>> {
        let verification = match &plan.verification {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };

        // Extract search terms for filename matching
        let search_terms = Arc::new(Self::extract_search_terms(verification));

        // Extract boost factor from verification steps
        let boost = Self::extract_boost(verification);

        // Extract line filter from plan (if present)
        let (line_start, line_end) = Self::extract_line_filter(&plan.steps);

        // Collect candidate doc_ids with their paths for parallel processing
        let candidate_infos: Vec<(DocId, PathBuf, PathBuf, u64)> = candidates
            .iter()
            .filter_map(|doc_id| {
                self.reader.get_document(doc_id).and_then(|doc| {
                    self.reader.get_full_path(doc).map(|full_path| {
                        let rel_path = self.reader.get_path(doc).cloned().unwrap_or_default();
                        (doc_id, full_path, rel_path, doc.mtime)
                    })
                })
            })
            .collect();

        // Process files - use cache for small result sets, parallel for large
        let candidate_count = candidate_infos.len();
        let use_cache = candidate_count <= 16; // Use cache for small result sets

        let all_matches: Vec<(DocId, PathBuf, u64, Vec<(u32, String, usize, usize)>)> = if use_cache {
            // Small result set: use cached reads (sequential to leverage cache)
            candidate_infos
                .into_iter()
                .filter_map(|(doc_id, full_path, rel_path, mtime)| {
                    // Use cached file reading
                    let content = self.reader.read_file_cached(&full_path)?;

                    // Find matches
                    let mut file_matches =
                        Self::verify_content_static(&content, verification, doc_id);

                    // Apply line filter if specified
                    if line_start.is_some() || line_end.is_some() {
                        file_matches.retain(|(line_num, _, _, _)| {
                            let above_min = line_start.map(|min| *line_num >= min).unwrap_or(true);
                            let below_max = line_end.map(|max| *line_num <= max).unwrap_or(true);
                            above_min && below_max
                        });
                    }

                    if file_matches.is_empty() {
                        None
                    } else {
                        Some((doc_id, rel_path, mtime, file_matches))
                    }
                })
                .collect()
        } else {
            // Large result set: use parallel uncached reads
            candidate_infos
                .into_par_iter()
                .filter_map(|(doc_id, full_path, rel_path, mtime)| {
                    // Read file content (no cache to avoid lock contention)
                    let content = fs::read_to_string(&full_path).ok()?;

                    // Find matches
                    let mut file_matches =
                        Self::verify_content_static(&content, verification, doc_id);

                    // Apply line filter if specified
                    if line_start.is_some() || line_end.is_some() {
                        file_matches.retain(|(line_num, _, _, _)| {
                            let above_min = line_start.map(|min| *line_num >= min).unwrap_or(true);
                            let below_max = line_end.map(|max| *line_num <= max).unwrap_or(true);
                            above_min && below_max
                        });
                    }

                    if file_matches.is_empty() {
                        None
                    } else {
                        Some((doc_id, rel_path, mtime, file_matches))
                    }
                })
                .collect()
        };

        // Build final results with scoring
        let mut matches = Vec::new();
        for (doc_id, path, mtime, file_matches) in all_matches {
            // Build score context
            let filename_match = search_terms
                .iter()
                .any(|term| Scorer::term_in_filename(&path, term));

            let score_ctx = ScoreContext {
                match_count: file_matches.len(),
                filename_match,
                depth: Scorer::path_depth(&path),
                mtime,
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

    /// Verify content against a verification step (static version for parallel processing)
    fn verify_content_static(
        content: &str,
        verification: &VerificationStep,
        doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        match verification {
            VerificationStep::Literal(text) => {
                Self::find_literal_matches_static(content, text, false, doc_id)
            }
            VerificationStep::BoostedLiteral { text, boost: _ } => {
                // Boosted literal: same matching as regular literal
                // The boost is applied during scoring, not matching
                Self::find_literal_matches_static(content, text, false, doc_id)
            }
            VerificationStep::Phrase(text) => {
                Self::find_literal_matches_static(content, text, true, doc_id)
            }
            VerificationStep::Regex(pattern) => {
                // Use cached regex compilation for performance
                if let Some(re) = get_regex_cache().get_or_compile(pattern) {
                    Self::find_regex_matches_static(content, &re, doc_id)
                } else {
                    Vec::new()
                }
            }
            VerificationStep::Near { terms, distance } => {
                Self::find_proximity_matches_static(content, terms, *distance, doc_id)
            }
            VerificationStep::And(steps) => {
                // All must have at least one match
                let mut all_matches: Option<Vec<(u32, String, usize, usize)>> = None;

                for step in steps {
                    let step_matches = Self::verify_content_static(content, step, doc_id);
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
                    all_matches.extend(Self::verify_content_static(content, step, doc_id));
                }
                all_matches
            }
            VerificationStep::Not(inner) => {
                let inner_matches = Self::verify_content_static(content, inner, doc_id);
                if inner_matches.is_empty() {
                    // Return a "match" indicating the file doesn't contain the pattern
                    vec![(1, content.lines().next().unwrap_or("").to_string(), 0, 0)]
                } else {
                    Vec::new()
                }
            }
        }
    }


    /// Find proximity matches: all terms must appear within distance lines of each other (static)
    fn find_proximity_matches_static(
        content: &str,
        terms: &[String],
        distance: u32,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        if terms.is_empty() {
            return Vec::new();
        }

        // Pre-lowercase all terms once
        let terms_lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();

        // Collect line numbers for each term
        let mut term_lines: Vec<Vec<u32>> = Vec::with_capacity(terms.len());

        for term_lower in &terms_lower {
            let mut lines_with_term = Vec::new();

            for (line_num, line) in content.lines().enumerate() {
                // Use ASCII case-insensitive comparison when possible
                let line_lower = line.to_lowercase();
                if line_lower.contains(term_lower) {
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
                    let line_lower = line_content.to_lowercase();
                    if let Some(pos) = line_lower.find(&terms_lower[0]) {
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


    /// Find literal string matches using memchr for fast search (static)
    fn find_literal_matches_static(
        content: &str,
        needle: &str,
        case_sensitive: bool,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        use memchr::memmem;

        let mut matches = Vec::new();

        // Pre-lowercase the needle once for case-insensitive search
        let search_needle = if case_sensitive {
            needle.to_string()
        } else {
            needle.to_lowercase()
        };
        let needle_bytes = search_needle.as_bytes();
        let finder = memmem::Finder::new(needle_bytes);

        for (line_num, line) in content.lines().enumerate() {
            let search_bytes = if case_sensitive {
                // Use line bytes directly for case-sensitive search
                line.as_bytes()
            } else {
                // We need to lowercase the line for case-insensitive search
                // This is unavoidable for non-ASCII text
                continue; // Will be handled below
            };

            if case_sensitive {
                if let Some(pos) = finder.find(search_bytes) {
                    matches.push((
                        (line_num + 1) as u32,
                        line.to_string(),
                        pos,
                        pos + needle.len(),
                    ));
                }
            }
        }

        // Handle case-insensitive search separately (requires lowercasing)
        if !case_sensitive {
            for (line_num, line) in content.lines().enumerate() {
                let line_lower = line.to_lowercase();
                if let Some(pos) = finder.find(line_lower.as_bytes()) {
                    matches.push((
                        (line_num + 1) as u32,
                        line.to_string(),
                        pos,
                        pos + needle.len(),
                    ));
                }
            }
        }

        matches
    }


    /// Find regex matches (static)
    fn find_regex_matches_static(
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
