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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Get the number of available CPU threads (cached for performance)
fn get_num_threads() -> usize {
    static NUM_THREADS: OnceLock<usize> = OnceLock::new();
    *NUM_THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    })
}

/// Calculate optimal threshold for parallel vs sequential processing
/// Returns true if parallel processing should be used
#[inline]
fn should_use_parallel(candidate_count: usize) -> bool {
    // Adaptive threshold based on CPU cores
    // Use sequential for small result sets to leverage cache locality
    // Use parallel for larger sets where parallelism overhead is worth it
    let num_threads = get_num_threads();

    // Minimum candidates per thread to justify parallelism overhead
    // (rayon spawn overhead is ~1-2µs, file read is ~10-100µs)
    let min_per_thread = 4;
    let parallel_threshold = num_threads * min_per_thread;

    candidate_count > parallel_threshold
}

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

/// Result for content-aware search
#[derive(Debug, Clone)]
pub struct ContentMatchResult {
    pub path: PathBuf,
    pub line_number: u32,
    pub line_content: String,
    pub match_start: usize,
    pub match_end: usize,
    pub context_before: Vec<(u32, String)>,
    pub context_after: Vec<(u32, String)>,
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

        // Verify candidates with limit for early termination
        let mut results = self.verify_candidates(&candidates, &plan, query.options.limit)?;

        // Also find files whose names match the search terms
        if let Some(ref verification) = plan.verification {
            let search_terms = Self::extract_search_terms(verification);
            if !search_terms.is_empty() {
                let filename_matches = self.find_filename_matches(&search_terms, query.options.limit)?;

                // Merge filename matches, avoiding duplicates
                let existing_paths: std::collections::HashSet<PathBuf> =
                    results.iter().map(|m| m.path.clone()).collect();
                for m in filename_matches {
                    if !existing_paths.contains(&m.path) {
                        results.push(m);
                    }
                }
            }
        }

        // Sort results
        self.sort_results(&mut results, query.options.sort);

        // Apply final limit (may have collected slightly more for better ranking)
        results.truncate(query.options.limit);

        Ok(results)
    }

    /// Execute query returning content matches with line content and context
    pub fn execute_with_content(
        &self,
        query: &Query,
        context_before: u32,
        context_after: u32,
    ) -> Result<Vec<ContentMatchResult>> {
        let plan = QueryPlan::from_query(query);
        let candidates = self.execute_plan(&plan)?;

        let verification = match &plan.verification {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };

        // Extract line filter from plan (if present)
        let (line_start, line_end) = Self::extract_line_filter(&plan.steps);

        // Collect candidate doc_ids with their paths for processing
        let candidate_infos: Vec<(DocId, PathBuf, PathBuf)> = candidates
            .iter()
            .filter_map(|doc_id| {
                self.reader.get_document(doc_id).and_then(|doc| {
                    self.reader.get_full_path(doc).map(|full_path| {
                        let rel_path = self.reader.get_path(doc).cloned().unwrap_or_default();
                        (doc_id, full_path, rel_path)
                    })
                })
            })
            .collect();

        let candidate_count = candidate_infos.len();
        let use_cache = !should_use_parallel(candidate_count);

        let mut all_results = Vec::new();

        if use_cache {
            // Sequential processing with cache
            for (_doc_id, full_path, rel_path) in candidate_infos {
                let content = match self.reader.read_file_cached(&full_path) {
                    Some(c) => c,
                    None => continue,
                };

                let file_matches = Self::verify_content_static(&content, verification, 0);

                for (line_num, line_content, start, end) in file_matches {
                    // Apply line filter if specified
                    if let Some(min) = line_start {
                        if line_num < min {
                            continue;
                        }
                    }
                    if let Some(max) = line_end {
                        if line_num > max {
                            continue;
                        }
                    }

                    let (ctx_before, ctx_after) =
                        Self::extract_context_lines(&content, line_num, context_before, context_after);

                    all_results.push(ContentMatchResult {
                        path: rel_path.clone(),
                        line_number: line_num,
                        line_content,
                        match_start: start,
                        match_end: end,
                        context_before: ctx_before,
                        context_after: ctx_after,
                    });
                }
            }
        } else {
            // Parallel processing
            let results: Vec<Vec<ContentMatchResult>> = candidate_infos
                .into_par_iter()
                .filter_map(|(_doc_id, full_path, rel_path)| {
                    let content = fs::read_to_string(&full_path).ok()?;
                    let file_matches = Self::verify_content_static(&content, verification, 0);

                    if file_matches.is_empty() {
                        return None;
                    }

                    let mut matches = Vec::new();
                    for (line_num, line_content, start, end) in file_matches {
                        // Apply line filter if specified
                        if let Some(min) = line_start {
                            if line_num < min {
                                continue;
                            }
                        }
                        if let Some(max) = line_end {
                            if line_num > max {
                                continue;
                            }
                        }

                        let (ctx_before, ctx_after) =
                            Self::extract_context_lines(&content, line_num, context_before, context_after);

                        matches.push(ContentMatchResult {
                            path: rel_path.clone(),
                            line_number: line_num,
                            line_content,
                            match_start: start,
                            match_end: end,
                            context_before: ctx_before,
                            context_after: ctx_after,
                        });
                    }

                    if matches.is_empty() {
                        None
                    } else {
                        Some(matches)
                    }
                })
                .collect();

            for file_results in results {
                all_results.extend(file_results);
            }
        }

        // Sort by path and line number
        all_results.sort_by(|a, b| {
            match a.path.cmp(&b.path) {
                std::cmp::Ordering::Equal => a.line_number.cmp(&b.line_number),
                other => other,
            }
        });

        Ok(all_results)
    }

    /// Execute query returning only unique files that match (optimized for -l mode)
    /// This is much faster than execute_with_content for common patterns because it:
    /// 1. Stops scanning each file after finding the first match
    /// 2. Skips context extraction
    /// 3. Returns minimal data per file
    pub fn execute_files_only(&self, query: &Query, file_limit: usize) -> Result<Vec<PathBuf>> {
        let plan = QueryPlan::from_query(query);
        let candidates = self.execute_plan(&plan)?;

        let verification = match &plan.verification {
            Some(v) => v,
            None => {
                // No content verification - just return file paths up to limit
                let paths: Vec<PathBuf> = candidates
                    .iter()
                    .filter_map(|doc_id| {
                        self.reader.get_document(doc_id).and_then(|doc| {
                            self.reader.get_path(doc).cloned()
                        })
                    })
                    .take(if file_limit == 0 { usize::MAX } else { file_limit })
                    .collect();
                return Ok(paths);
            }
        };

        // Collect candidate doc_ids with their paths for processing
        let candidate_infos: Vec<(DocId, PathBuf, PathBuf)> = candidates
            .iter()
            .filter_map(|doc_id| {
                self.reader.get_document(doc_id).and_then(|doc| {
                    self.reader.get_full_path(doc).map(|full_path| {
                        let rel_path = self.reader.get_path(doc).cloned().unwrap_or_default();
                        (doc_id, full_path, rel_path)
                    })
                })
            })
            .collect();

        let candidate_count = candidate_infos.len();
        let effective_limit = if file_limit == 0 { usize::MAX } else { file_limit };

        // For files-only, we use parallel processing with early termination
        // Each file only needs to find ONE match to be included
        let match_count = AtomicUsize::new(0);

        let matching_files: Vec<PathBuf> = if !should_use_parallel(candidate_count) {
            // Sequential for small result sets
            let mut results = Vec::new();
            for (_doc_id, full_path, rel_path) in candidate_infos {
                if results.len() >= effective_limit {
                    break;
                }

                let content = match self.reader.read_file_cached(&full_path) {
                    Some(c) => c,
                    None => continue,
                };

                // Check if file has ANY match (fast path)
                if Self::has_match(&content, verification) {
                    results.push(rel_path);
                }
            }
            results
        } else {
            // Parallel processing with early termination
            candidate_infos
                .into_par_iter()
                .filter_map(|(_doc_id, full_path, rel_path)| {
                    // Early termination check
                    if match_count.load(Ordering::Relaxed) >= effective_limit {
                        return None;
                    }

                    // Read file content
                    let content = fs::read_to_string(&full_path).ok()?;

                    // Check if file has ANY match
                    if Self::has_match(&content, verification) {
                        match_count.fetch_add(1, Ordering::Relaxed);
                        Some(rel_path)
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Sort by path for consistent output
        let mut sorted = matching_files;
        sorted.sort();
        if sorted.len() > effective_limit {
            sorted.truncate(effective_limit);
        }

        Ok(sorted)
    }

    /// Fast check if content has ANY match (for files-only mode)
    /// Returns immediately on first match found
    fn has_match(content: &str, verification: &VerificationStep) -> bool {
        match verification {
            VerificationStep::Literal(text) => {
                // Case-insensitive search using memchr
                use memchr::memmem;
                let needle_lower = text.to_lowercase();
                let finder = memmem::Finder::new(needle_lower.as_bytes());

                for line in content.lines() {
                    if line.is_ascii() && text.is_ascii() {
                        // Fast ASCII path
                        if line.to_ascii_lowercase().contains(&needle_lower) {
                            return true;
                        }
                    } else {
                        let line_lower = line.to_lowercase();
                        if finder.find(line_lower.as_bytes()).is_some() {
                            return true;
                        }
                    }
                }
                false
            }
            VerificationStep::BoostedLiteral { text, .. } => {
                Self::has_match(content, &VerificationStep::Literal(text.clone()))
            }
            VerificationStep::Phrase(text) => {
                // Exact phrase match (case-sensitive)
                content.contains(text)
            }
            VerificationStep::Regex(pattern) => {
                if let Some(re) = get_regex_cache().get_or_compile(pattern) {
                    re.is_match(content)
                } else {
                    false
                }
            }
            VerificationStep::And(steps) => {
                steps.iter().all(|step| Self::has_match(content, step))
            }
            VerificationStep::Or(steps) => {
                steps.iter().any(|step| Self::has_match(content, step))
            }
            VerificationStep::Not(inner) => {
                !Self::has_match(content, inner)
            }
            VerificationStep::Near { terms, distance } => {
                // Simplified check for proximity - just verify all terms exist
                let content_lower = content.to_lowercase();
                let terms_exist = terms.iter().all(|t| content_lower.contains(&t.to_lowercase()));
                if !terms_exist {
                    return false;
                }
                // Full proximity check (reuse existing logic)
                !Self::find_proximity_matches_static(content, terms, *distance, 0).is_empty()
            }
        }
    }

    /// Extract context lines around a match
    fn extract_context_lines(
        content: &str,
        match_line: u32,
        before: u32,
        after: u32,
    ) -> (Vec<(u32, String)>, Vec<(u32, String)>) {
        let lines: Vec<&str> = content.lines().collect();
        let match_idx = (match_line - 1) as usize;

        let mut ctx_before = Vec::new();
        let mut ctx_after = Vec::new();

        // Extract lines before
        let start_idx = match_idx.saturating_sub(before as usize);
        for i in start_idx..match_idx {
            if let Some(line) = lines.get(i) {
                ctx_before.push(((i + 1) as u32, line.to_string()));
            }
        }

        // Extract lines after
        let end_idx = (match_idx + 1 + after as usize).min(lines.len());
        for i in (match_idx + 1)..end_idx {
            if let Some(line) = lines.get(i) {
                ctx_after.push(((i + 1) as u32, line.to_string()));
            }
        }

        (ctx_before, ctx_after)
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

        // For filename filter, store the pattern for case-insensitive matching
        let filename_pattern = filter.filename.clone();

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

                // Filename filter (case-insensitive)
                if let Some(ref pattern) = filename_pattern {
                    if let Some(path) = self.reader.get_path(doc) {
                        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        let filename_lower = filename.to_lowercase();
                        let pattern_lower = pattern.to_lowercase();

                        // Support glob patterns or substring matching
                        let matches = if pattern.contains('*') || pattern.contains('?') {
                            Glob::new(&pattern_lower)
                                .map(|g| g.compile_matcher().is_match(&filename_lower))
                                .unwrap_or(false)
                        } else {
                            filename_lower.contains(&pattern_lower)
                        };

                        if !matches {
                            continue;
                        }
                    } else {
                        continue;
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

    /// Create matches for file-only queries (no content search term)
    fn create_file_matches(
        &self,
        candidates: &RoaringBitmap,
        limit: usize,
    ) -> Result<Vec<SearchMatch>> {
        let mut matches = Vec::with_capacity(limit.min(candidates.len() as usize));

        for doc_id in candidates.iter().take(limit) {
            if let Some(doc) = self.reader.get_document(doc_id) {
                if let Some(path) = self.reader.get_path(doc) {
                    matches.push(SearchMatch {
                        doc_id,
                        path: path.clone(),
                        line_number: 1,
                        score: 1.0,
                    });
                }
            }
        }

        Ok(matches)
    }

    /// Find files whose names contain any of the search terms
    fn find_filename_matches(
        &self,
        search_terms: &[String],
        limit: usize,
    ) -> Result<Vec<SearchMatch>> {
        let mut matches = Vec::new();
        let valid_docs = self.reader.valid_doc_ids();

        // Convert search terms to lowercase for case-insensitive matching
        let terms_lower: Vec<String> = search_terms.iter().map(|t| t.to_lowercase()).collect();

        for doc_id in valid_docs.iter() {
            if matches.len() >= limit {
                break;
            }

            if let Some(doc) = self.reader.get_document(doc_id) {
                if let Some(path) = self.reader.get_path(doc) {
                    let filename = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_lowercase();

                    // Check if filename contains any search term
                    let matches_term = terms_lower.iter().any(|term| filename.contains(term));

                    if matches_term {
                        matches.push(SearchMatch {
                            doc_id,
                            path: path.clone(),
                            line_number: 1,
                            score: 2.0, // Boost filename matches
                        });
                    }
                }
            }
        }

        Ok(matches)
    }

    /// Verify candidates against actual file content using parallel processing
    /// with early termination once limit is reached
    fn verify_candidates(
        &self,
        candidates: &RoaringBitmap,
        plan: &QueryPlan,
        limit: usize,
    ) -> Result<Vec<SearchMatch>> {
        let verification = match &plan.verification {
            Some(v) => v,
            None => {
                // No content verification needed - return file-only matches
                // This happens when using filters without a search term (e.g., "file:foo")
                return self.create_file_matches(candidates, limit);
            }
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
        // Adaptive threshold based on CPU cores
        let candidate_count = candidate_infos.len();
        let use_cache = !should_use_parallel(candidate_count);

        // Track match count for early termination
        // Collect 1.5x limit for better ranking (was 2x, reduced for faster termination)
        let target_matches = limit + (limit / 2);
        let match_count = AtomicUsize::new(0);

        let all_matches: Vec<(DocId, PathBuf, u64, Vec<(u32, String, usize, usize)>)> = if use_cache {
            // Small result set: use cached reads (sequential to leverage cache)
            let mut results = Vec::with_capacity(candidate_count.min(target_matches));
            let mut total_matches = 0;

            for (doc_id, full_path, rel_path, mtime) in candidate_infos {
                // Early termination check
                if total_matches >= target_matches {
                    break;
                }

                // Use cached file reading
                let content = match self.reader.read_file_cached(&full_path) {
                    Some(c) => c,
                    None => continue,
                };

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

                if !file_matches.is_empty() {
                    total_matches += file_matches.len();
                    results.push((doc_id, rel_path, mtime, file_matches));
                }
            }
            results
        } else {
            // Large result set: use parallel uncached reads with early termination
            // Use with_min_len to ensure good work stealing granularity
            candidate_infos
                .into_par_iter()
                .with_min_len(4) // Ensure chunks aren't too small
                .filter_map(|(doc_id, full_path, rel_path, mtime)| {
                    // Early termination check - skip if we have enough matches
                    // Use Relaxed ordering for maximum performance (occasional over-collection is OK)
                    if match_count.load(Ordering::Relaxed) >= target_matches {
                        return None;
                    }

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
                        // Update match count for early termination
                        match_count.fetch_add(file_matches.len(), Ordering::Relaxed);
                        Some((doc_id, rel_path, mtime, file_matches))
                    }
                })
                .collect()
        };

        // Build final results with scoring
        // Pre-allocate based on expected match count (typically 1-3 matches per file)
        let estimated_total = all_matches.len() * 2;
        let mut matches = Vec::with_capacity(estimated_total.min(limit * 2));

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
            for (line_num, _line_content, _start, _end) in file_matches {
                matches.push(SearchMatch {
                    doc_id,
                    path: path.clone(),
                    line_number: line_num,
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
    /// Optimized to minimize allocations and leverage pre-computed lowercase lines
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

        // Collect lines with their lowercase versions ONCE (avoid repeated to_lowercase)
        let lines: Vec<(&str, String)> = content
            .lines()
            .map(|line| (line, line.to_lowercase()))
            .collect();

        // For each term, collect line numbers where it appears
        let mut term_lines: Vec<Vec<u32>> = Vec::with_capacity(terms.len());

        for term_lower in &terms_lower {
            let mut lines_with_term = Vec::new();

            for (line_num, (_original, line_lower)) in lines.iter().enumerate() {
                if line_lower.contains(term_lower.as_str()) {
                    lines_with_term.push((line_num + 1) as u32);
                }
            }

            if lines_with_term.is_empty() {
                // One of the terms doesn't exist in the file - early exit
                return Vec::new();
            }

            term_lines.push(lines_with_term);
        }

        // Find line combinations where all terms are within distance
        let mut matches = Vec::new();

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
                if let Some((original_line, line_lower)) = lines.get(line_idx) {
                    if let Some(pos) = line_lower.find(&terms_lower[0]) {
                        matches.push((
                            first_line,
                            original_line.to_string(),
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
    /// Optimized single-pass iteration for both case-sensitive and case-insensitive modes
    fn find_literal_matches_static(
        content: &str,
        needle: &str,
        case_sensitive: bool,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        use memchr::memmem;

        let mut matches = Vec::new();

        if case_sensitive {
            // Case-sensitive: search directly on original bytes
            let finder = memmem::Finder::new(needle.as_bytes());
            for (line_num, line) in content.lines().enumerate() {
                if let Some(pos) = finder.find(line.as_bytes()) {
                    matches.push((
                        (line_num + 1) as u32,
                        line.to_string(),
                        pos,
                        pos + needle.len(),
                    ));
                }
            }
        } else {
            // Case-insensitive: pre-lowercase needle once, lowercase each line
            let needle_lower = needle.to_lowercase();
            let finder = memmem::Finder::new(needle_lower.as_bytes());

            for (line_num, line) in content.lines().enumerate() {
                // Fast path: check if line could possibly match using ASCII comparison first
                // This avoids expensive to_lowercase() for lines that can't match
                let line_bytes = line.as_bytes();

                // Quick rejection: if line is shorter than needle, skip
                if line_bytes.len() < needle_lower.len() {
                    continue;
                }

                // Try ASCII lowercase comparison first (common case, avoids allocation)
                let mut found_ascii = false;
                if line.is_ascii() && needle.is_ascii() {
                    // Fast ASCII-only path: compare in-place without allocation
                    let needle_bytes = needle_lower.as_bytes();
                    for i in 0..=(line_bytes.len() - needle_bytes.len()) {
                        let mut matched = true;
                        for j in 0..needle_bytes.len() {
                            let line_char = line_bytes[i + j].to_ascii_lowercase();
                            if line_char != needle_bytes[j] {
                                matched = false;
                                break;
                            }
                        }
                        if matched {
                            matches.push((
                                (line_num + 1) as u32,
                                line.to_string(),
                                i,
                                i + needle.len(),
                            ));
                            found_ascii = true;
                            break; // Only report first match per line
                        }
                    }
                }

                // Fallback to full Unicode lowercase for non-ASCII
                if !found_ascii && (!line.is_ascii() || !needle.is_ascii()) {
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
