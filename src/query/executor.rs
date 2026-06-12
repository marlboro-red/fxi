use crate::index::reader::{FileContent, IndexReader};
use crate::index::types::{DocId, Language, SearchMatch};
use crate::query::parser::{Query, SortOrder};
use crate::query::planner::{FilterStep, PlanStep, QueryPlan, VerificationStep};
use crate::query::scorer::{ScoreContext, Scorer, ScoringWeights};
use anyhow::Result;
use globset::Glob;
use memmap2::Mmap;
use rayon::prelude::*;
use regex::Regex;
use roaring::RoaringBitmap;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

/// Context lines before/after a match: Vec<(line_number, line_content)>
type ContextLines = Vec<(u32, String)>;

/// A single match within a file: (line_number, line_content, match_start, match_end)
type FileMatch = (u32, String, usize, usize);

/// Collected file matches with metadata: (doc_id, full_path, rel_path, mtime, matches)
type FileMatchResult = (DocId, PathBuf, PathBuf, u64, Vec<FileMatch>);

/// Precompiled filename filter matcher to avoid per-document recompilation.
enum FilenameMatcher {
    Exact(String),
    Glob(globset::GlobMatcher),
}

impl FilenameMatcher {
    #[inline]
    fn is_match(&self, filename_lower: &str) -> bool {
        match self {
            Self::Exact(exact) => filename_lower == exact,
            Self::Glob(glob) => glob.is_match(filename_lower),
        }
    }
}

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

/// Minimum file size to use memory mapping (smaller files are faster with regular read)
const MMAP_THRESHOLD: u64 = 4096;

/// Read file content using memory mapping for large files, regular read for small files.
/// Memory-mapped content is borrowed directly (zero-copy); the OS handles caching.
/// Returns None if the file cannot be read or contains invalid UTF-8.
#[inline]
fn read_file_mmap(path: &Path) -> Option<FileContent> {
    let file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let size = metadata.len();

    if size == 0 {
        return Some(FileContent::Owned(String::new()));
    }

    if size < MMAP_THRESHOLD {
        // Small file: regular read is faster (avoids mmap syscall overhead)
        let content = fs::read_to_string(path).ok()?;
        Some(FileContent::Owned(content))
    } else {
        // Large file: use memory mapping
        let mmap = unsafe { Mmap::map(&file).ok()? };

        // Validate UTF-8 once; deref borrows the mapped pages directly
        std::str::from_utf8(&mmap).ok()?;
        Some(FileContent::Mapped(mmap))
    }
}

/// Thread-safe regex cache for avoiding repeated compilation.
/// Uses RwLock to allow concurrent reads (cache hits are common in parallel workloads).
struct RegexCache {
    cache: RwLock<HashMap<String, Arc<Regex>>>,
    max_size: usize,
}

impl RegexCache {
    fn new(max_size: usize) -> Self {
        Self {
            cache: RwLock::new(HashMap::with_capacity(max_size)),
            max_size,
        }
    }

    /// Get or compile a regex pattern.
    /// Uses read lock for cache lookups (allows concurrent readers),
    /// only acquires write lock when inserting new patterns.
    fn get_or_compile(&self, pattern: &str) -> Option<Arc<Regex>> {
        // Fast path: check cache with read lock (allows concurrent readers)
        {
            let cache = self.cache.read().ok()?;
            if let Some(re) = cache.get(pattern) {
                return Some(Arc::clone(re));
            }
        }

        // Compile the regex (outside of any lock)
        let re = Regex::new(pattern).ok()?;
        let arc_re = Arc::new(re);

        // Slow path: insert with write lock
        if let Ok(mut cache) = self.cache.write() {
            // Double-check: another thread may have inserted while we were compiling
            if let Some(existing) = cache.get(pattern) {
                return Some(Arc::clone(existing));
            }

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

        let limit = query.options.limit;
        // Collect 1.5x limit for better ranking (early termination)
        let target = if limit > 0 {
            Some(limit + (limit / 2))
        } else {
            None
        };
        let all_matches = self.find_verified_matches(&candidates, &plan, target)?;

        // Build results with scoring
        let verification = plan.verification.as_ref();
        let search_terms_lower = verification
            .map(Self::extract_search_terms)
            .unwrap_or_default();
        let boost = verification.map(Self::extract_boost).unwrap_or(1.0);

        let estimated_total = all_matches.len() * 2;
        let mut results = Vec::with_capacity(estimated_total.min(if limit > 0 {
            limit * 2
        } else {
            estimated_total
        }));

        for (doc_id, _full_path, path, mtime, file_matches) in &all_matches {
            if file_matches.is_empty() {
                // File-only query (no verification) — emit one match per file
                results.push(SearchMatch {
                    doc_id: *doc_id,
                    path: path.clone(),
                    line_number: 1,
                    score: 1.0,
                });
                continue;
            }

            let filename_match = Self::filename_matches_terms(path, &search_terms_lower);
            let score_ctx = ScoreContext {
                match_count: file_matches.len(),
                filename_match,
                depth: Scorer::path_depth(path),
                mtime: *mtime,
                boost,
            };
            let score = self.scorer.calculate_score(&score_ctx);

            for (line_num, _line_content, _start, _end) in file_matches {
                results.push(SearchMatch {
                    doc_id: *doc_id,
                    path: path.clone(),
                    line_number: *line_num,
                    score,
                });
            }
        }

        // Also find files whose names match the search terms
        if !search_terms_lower.is_empty() {
            let filename_matches = self.find_filename_matches(&search_terms_lower, limit)?;

            // Merge filename matches, avoiding duplicates (dedup by borrowed
            // path — no PathBuf clones)
            let to_add: Vec<SearchMatch> = {
                let existing_paths: std::collections::HashSet<&Path> =
                    results.iter().map(|m| m.path.as_path()).collect();
                filename_matches
                    .into_iter()
                    .filter(|m| !existing_paths.contains(m.path.as_path()))
                    .collect()
            };
            results.extend(to_add);
        }

        // Sort results
        self.sort_results(&mut results, query.options.sort);

        // Apply final limit (may have collected slightly more for better ranking)
        // Skip truncation when limit == 0 (unlimited)
        if limit > 0 {
            results.truncate(limit);
        }

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

        let verified = self.find_verified_matches(&candidates, &plan, None)?;

        let mut all_results = Vec::new();

        for (_doc_id, full_path, rel_path, _mtime, file_matches) in verified {
            if file_matches.is_empty() {
                // File-only query (no verification) — emit one match per file
                all_results.push(ContentMatchResult {
                    path: rel_path,
                    line_number: 1,
                    line_content: String::new(),
                    match_start: 0,
                    match_end: 0,
                    context_before: vec![],
                    context_after: vec![],
                });
                continue;
            }

            // Re-read file for context extraction (hits file cache for sequential,
            // re-reads for parallel — but context extraction is a post-processing step)
            let content = self
                .reader
                .read_file_cached(&full_path)
                .or_else(|| read_file_mmap(&full_path));

            // Split into lines once per file, not once per match
            let lines: Option<Vec<&str>> = match &content {
                Some(c) if context_before > 0 || context_after > 0 => Some(c.lines().collect()),
                _ => None,
            };

            for (line_num, line_content, start, end) in file_matches {
                let (ctx_before, ctx_after) = match &lines {
                    Some(lines) => Self::extract_context_from_lines(
                        lines,
                        line_num,
                        context_before,
                        context_after,
                    ),
                    None => (Vec::new(), Vec::new()),
                };

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

        // Sort by path and line number
        all_results.sort_by(|a, b| match a.path.cmp(&b.path) {
            std::cmp::Ordering::Equal => a.line_number.cmp(&b.line_number),
            other => other,
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
                        self.reader
                            .get_document(doc_id)
                            .and_then(|doc| self.reader.get_path(doc).cloned())
                    })
                    .take(if file_limit == 0 {
                        usize::MAX
                    } else {
                        file_limit
                    })
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
        let effective_limit = if file_limit == 0 {
            usize::MAX
        } else {
            file_limit
        };

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
            // Parallel processing with early termination and memory-mapped I/O
            candidate_infos
                .into_par_iter()
                .filter_map(|(_doc_id, full_path, rel_path)| {
                    // Early termination check
                    if match_count.load(Ordering::Relaxed) >= effective_limit {
                        return None;
                    }

                    // Read file content using mmap for large files
                    let content = read_file_mmap(&full_path)?;

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
            VerificationStep::Literal(text) => Self::has_literal_match(content, text),
            VerificationStep::BoostedLiteral { text, .. } => Self::has_literal_match(content, text),
            VerificationStep::Phrase {
                text,
                case_insensitive,
            } => {
                if *case_insensitive {
                    Self::has_literal_match(content, text)
                } else {
                    // Exact phrase match (case-sensitive)
                    content.contains(text)
                }
            }
            VerificationStep::Regex(pattern) => {
                if let Some(re) = get_regex_cache().get_or_compile(pattern) {
                    re.is_match(content)
                } else {
                    false
                }
            }
            VerificationStep::And(steps) => steps.iter().all(|step| Self::has_match(content, step)),
            VerificationStep::Or(steps) => steps.iter().any(|step| Self::has_match(content, step)),
            VerificationStep::Not(inner) => !Self::has_match(content, inner),
            VerificationStep::Near { terms, distance } => {
                // find_proximity_matches_static early-exits as soon as any
                // term is missing, so no separate existence pre-check needed
                !Self::find_proximity_matches_static(content, terms, *distance, 0).is_empty()
            }
        }
    }

    /// Fast literal presence check for files-only mode.
    #[inline]
    fn has_literal_match(content: &str, text: &str) -> bool {
        use memchr::memmem;

        let needle_lower = text.to_lowercase();
        let finder = memmem::Finder::new(needle_lower.as_bytes());

        if content.is_ascii()
            && text.is_ascii()
            && !needle_lower.contains('\n')
            && !needle_lower.contains('\r')
        {
            // Lowercase once and run a single SIMD search over the whole
            // content instead of allocating a lowercased String per line
            return finder
                .find(content.to_ascii_lowercase().as_bytes())
                .is_some();
        }

        for line in content.lines() {
            let line_lower = line.to_lowercase();
            if finder.find(line_lower.as_bytes()).is_some() {
                return true;
            }
        }

        false
    }

    /// Extract context lines around a match.
    ///
    /// The caller splits the file into lines once (`lines`); each match then
    /// slices its context window directly instead of re-scanning the file
    /// from line 0 per match.
    fn extract_context_from_lines(
        lines: &[&str],
        match_line: u32,
        before: u32,
        after: u32,
    ) -> (ContextLines, ContextLines) {
        let match_idx = (match_line - 1) as usize;
        let start_line = match_idx.saturating_sub(before as usize);

        let mut ctx_before = Vec::with_capacity(before as usize);
        for i in start_line..match_idx {
            if let Some(&line) = lines.get(i) {
                ctx_before.push(((i + 1) as u32, line.to_string()));
            }
        }

        let mut ctx_after = Vec::with_capacity(after as usize);
        for i in (match_idx + 1)..(match_idx + 1 + after as usize) {
            if let Some(&line) = lines.get(i) {
                ctx_after.push(((i + 1) as u32, line.to_string()));
            }
        }

        (ctx_before, ctx_after)
    }

    /// Execute the narrowing phase using RoaringBitmap for efficient set operations
    fn execute_plan(&self, plan: &QueryPlan) -> Result<RoaringBitmap> {
        let mut candidates: Option<RoaringBitmap> = None;
        let mut exclude_plans: Vec<&QueryPlan> = Vec::new();

        for step in &plan.steps {
            match step {
                PlanStep::TrigramIntersect(trigrams) => {
                    // Filter out stop-grams
                    let mut filtered_trigrams: Vec<_> = trigrams
                        .iter()
                        .filter(|&&t| !self.reader.is_stop_gram(t))
                        .copied()
                        .collect();
                    filtered_trigrams.sort_unstable();
                    filtered_trigrams.dedup();

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

                PlanStep::TokenOrTrigram {
                    token,
                    sub_tokens,
                    trigrams,
                } => {
                    let mut docs = self.reader.get_token_docs(token);

                    // Trigram side is a best-effort substring-recall
                    // supplement: when all its trigrams are stop-grams it is
                    // dropped, instead of degrading to the whole corpus the
                    // way an unnarrowable sub-plan would
                    let filtered: Vec<_> = trigrams
                        .iter()
                        .filter(|&&t| !self.reader.is_stop_gram(t))
                        .copied()
                        .collect();
                    if !filtered.is_empty() {
                        docs |= self.reader.get_trigram_docs_with_bloom(&filtered);
                    } else {
                        // No usable trigrams. Substring recall comes from the
                        // token dictionary instead: any alphanumeric substring
                        // lies inside a single token ("println" in
                        // "eprintln"), so scan the dictionary for containing
                        // tokens...
                        docs |= self.reader.get_token_docs_containing(token);

                        // ...and compound identifiers (foo_bar) are indexed
                        // as their parts, so intersect the sub-token postings
                        if sub_tokens.len() >= 2 {
                            let mut sub_docs: Option<RoaringBitmap> = None;
                            for sub in sub_tokens {
                                let d = self.reader.get_token_docs(sub);
                                sub_docs = Some(match sub_docs {
                                    Some(existing) => existing & d,
                                    None => d,
                                });
                            }
                            if let Some(sub_docs) = sub_docs {
                                docs |= sub_docs;
                            }
                        }
                    }

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
                    // Deferred: exclusion is resolved after all narrowing steps,
                    // so its (expensive) content verification only runs against
                    // docs that are actually in the final candidate set.
                    exclude_plans.push(sub_plan);
                }

                PlanStep::PositionalPhrase(phrase_tokens) => {
                    // Use positional index to resolve phrase adjacency,
                    // decoding positions only for already-narrowed candidates
                    if let Some(positional_docs) = self
                        .reader
                        .resolve_phrase_positional(phrase_tokens, candidates.as_ref())
                    {
                        candidates = Some(match candidates {
                            Some(existing) => existing & positional_docs,
                            None => positional_docs,
                        });
                    }
                    // If None (no positional data), skip — fall back to content verification
                }

                PlanStep::Filter(filter) => {
                    // Apply document filters
                    let filtered = self.apply_filter(filter, candidates.as_ref())?;
                    candidates = Some(filtered);
                }
            }

            // Once candidates are empty, later intersections/unions in this plan
            // cannot produce new matches, so exit early.
            if candidates.as_ref().is_some_and(|c| c.is_empty()) {
                break;
            }
        }

        // Resolve exclusions against the narrowed candidate set. Only docs
        // that are both candidates and trigram matches for the negated term
        // need content verification (to avoid trigram false positives) —
        // not every doc that merely shares trigrams with the negated term.
        if let Some(ref mut cands) = candidates {
            for sub_plan in exclude_plans {
                if cands.is_empty() {
                    break;
                }
                let excluded = self.execute_plan(sub_plan)?;

                if let Some(ref verification) = sub_plan.verification {
                    let to_check = excluded & &*cands;
                    let confirmed = self.verify_excluded_docs(to_check, verification);
                    *cands -= confirmed;
                } else {
                    // No verification available, fall back to trigram-only exclusion
                    *cands -= excluded;
                }
            }
        }

        // If no narrowing steps, start with all valid documents
        Ok(candidates.unwrap_or_else(|| self.reader.valid_doc_ids().clone()))
    }

    /// Verify which of the candidate docs actually contain the excluded term.
    /// Uses early-exit matching (any match disqualifies) and goes parallel for
    /// larger sets.
    fn verify_excluded_docs(
        &self,
        to_check: RoaringBitmap,
        verification: &VerificationStep,
    ) -> RoaringBitmap {
        if !should_use_parallel(to_check.len() as usize) {
            let mut confirmed = RoaringBitmap::new();
            for doc_id in to_check.iter() {
                if let Some(doc) = self.reader.get_document(doc_id)
                    && let Some(full_path) = self.reader.get_full_path(doc)
                    && let Some(content) = self.reader.read_file_cached(&full_path)
                    && Self::has_match(&content, verification)
                {
                    confirmed.insert(doc_id);
                }
            }
            confirmed
        } else {
            let doc_ids: Vec<u32> = to_check.iter().collect();
            doc_ids
                .into_par_iter()
                .filter(|&doc_id| {
                    self.reader
                        .get_document(doc_id)
                        .and_then(|doc| self.reader.get_full_path(doc))
                        .and_then(|full_path| read_file_mmap(&full_path))
                        .map(|content| Self::has_match(&content, verification))
                        .unwrap_or(false)
                })
                .collect::<Vec<u32>>()
                .into_iter()
                .collect()
        }
    }

    /// Apply document filters using RoaringBitmap
    fn apply_filter(
        &self,
        filter: &FilterStep,
        candidates: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap> {
        let path_matcher = filter.path_glob.as_ref().map(|g| {
            globset::GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .unwrap_or_else(|_| Glob::new("*").unwrap())
                .compile_matcher()
        });

        let filename_matcher = filter.filename.as_ref().map(|pattern| {
            let pattern_lower = pattern.to_lowercase();
            if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
                match Glob::new(&pattern_lower) {
                    Ok(glob) => FilenameMatcher::Glob(glob.compile_matcher()),
                    Err(_) => FilenameMatcher::Exact(pattern_lower),
                }
            } else {
                FilenameMatcher::Exact(pattern_lower)
            }
        });

        let language_filter = filter.language.as_deref().map(parse_language);
        let needs_path =
            path_matcher.is_some() || filename_matcher.is_some() || filter.extension.is_some();

        let mut result = RoaringBitmap::new();

        let mut check_doc = |doc_id: u32| {
            if let Some(doc) = self.reader.get_document(doc_id) {
                // Skip stale/tombstone
                if !doc.is_valid() {
                    return;
                }

                let path = if needs_path {
                    self.reader.get_path(doc)
                } else {
                    None
                };

                // Path filter
                if let Some(ref matcher) = path_matcher
                    && !path.map(|p| matcher.is_match(p)).unwrap_or(false)
                {
                    return;
                }

                // Filename filter (case-insensitive, exact match unless glob pattern)
                if let Some(ref matcher) = filename_matcher {
                    let filename_lower = path
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_lowercase());

                    if !filename_lower
                        .as_deref()
                        .map(|name| matcher.is_match(name))
                        .unwrap_or(false)
                    {
                        return;
                    }
                }

                // Extension filter
                if let Some(ref ext) = filter.extension {
                    if let Some(path) = path {
                        let file_ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        if !file_ext.eq_ignore_ascii_case(ext) {
                            return;
                        }
                    } else {
                        return;
                    }
                }

                // Language filter
                if language_filter
                    .map(|lang| doc.language != lang)
                    .unwrap_or(false)
                {
                    return;
                }

                // Size filters
                if let Some(min) = filter.size_min
                    && doc.size < min
                {
                    return;
                }
                if let Some(max) = filter.size_max
                    && doc.size > max
                {
                    return;
                }

                // Modification time filters
                if let Some(min) = filter.mtime_min
                    && doc.mtime < min
                {
                    return;
                }
                if let Some(max) = filter.mtime_max
                    && doc.mtime > max
                {
                    return;
                }

                result.insert(doc_id);
            }
        };

        if let Some(cands) = candidates {
            for doc_id in cands.iter() {
                check_doc(doc_id);
            }
        } else {
            let valid_docs = self.reader.valid_doc_ids();
            for doc_id in valid_docs.iter() {
                check_doc(doc_id);
            }
        }

        Ok(result)
    }

    /// Case-insensitive substring check for short ASCII haystacks (filenames)
    /// that avoids allocating a lowercased copy per call.
    #[inline]
    fn contains_ignore_ascii_case(haystack: &str, needle_lower: &str) -> bool {
        let h = haystack.as_bytes();
        let n = needle_lower.as_bytes();
        if n.is_empty() {
            return true;
        }
        if h.len() < n.len() {
            return false;
        }
        h.windows(n.len())
            .any(|w| w.iter().zip(n).all(|(&a, &b)| a.to_ascii_lowercase() == b))
    }

    /// Check if filename contains any search term (all terms must already be lowercase).
    /// This runs once per document in the index on scored searches, so the
    /// ASCII path must not allocate.
    #[inline]
    fn filename_matches_terms(path: &Path, terms_lower: &[String]) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .map(|filename| {
                if filename.is_ascii() {
                    terms_lower
                        .iter()
                        .any(|term| Self::contains_ignore_ascii_case(filename, term))
                } else {
                    let filename_lower = filename.to_lowercase();
                    terms_lower.iter().any(|term| filename_lower.contains(term))
                }
            })
            .unwrap_or(false)
    }

    /// Find files whose names contain any of the search terms
    fn find_filename_matches(
        &self,
        search_terms_lower: &[String],
        limit: usize,
    ) -> Result<Vec<SearchMatch>> {
        let mut matches = Vec::new();
        let valid_docs = self.reader.valid_doc_ids();

        for doc_id in valid_docs.iter() {
            if matches.len() >= limit {
                break;
            }

            if let Some(doc) = self.reader.get_document(doc_id)
                && let Some(path) = self.reader.get_path(doc)
                && Self::filename_matches_terms(path, search_terms_lower)
            {
                matches.push(SearchMatch {
                    doc_id,
                    path: path.clone(),
                    line_number: 1,
                    score: 2.0, // Boost filename matches
                });
            }
        }

        Ok(matches)
    }

    /// Shared pipeline: collect candidates → read files → verify content → apply line filter.
    /// Returns per-file match data. If `target_matches` is Some(n), stops after collecting
    /// approximately n total matches (for early termination in score-based search).
    fn find_verified_matches(
        &self,
        candidates: &RoaringBitmap,
        plan: &QueryPlan,
        target_matches: Option<usize>,
    ) -> Result<Vec<FileMatchResult>> {
        let verification = match &plan.verification {
            Some(v) => v,
            None => {
                // No content verification needed — return one entry per file with empty matches
                let results: Vec<FileMatchResult> = candidates
                    .iter()
                    .filter_map(|doc_id| {
                        self.reader.get_document(doc_id).and_then(|doc| {
                            self.reader.get_full_path(doc).map(|full_path| {
                                let rel_path =
                                    self.reader.get_path(doc).cloned().unwrap_or_default();
                                (doc_id, full_path, rel_path, doc.mtime, Vec::new())
                            })
                        })
                    })
                    .collect();
                return Ok(results);
            }
        };

        // Extract line filter from plan (if present)
        let (line_start, line_end) = Self::extract_line_filter(&plan.steps);

        // Collect candidate doc_ids with their paths for processing
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

        let candidate_count = candidate_infos.len();
        let use_cache = !should_use_parallel(candidate_count);

        let all_matches: Vec<FileMatchResult> = if use_cache {
            // Small result set: use cached reads (sequential to leverage cache)
            let mut results =
                Vec::with_capacity(candidate_count.min(target_matches.unwrap_or(candidate_count)));
            let mut total_matches = 0;

            for (doc_id, full_path, rel_path, mtime) in candidate_infos {
                // Early termination check
                if let Some(target) = target_matches {
                    if total_matches >= target {
                        break;
                    }
                }

                let content = match self.reader.read_file_cached(&full_path) {
                    Some(c) => c,
                    None => continue,
                };

                let mut file_matches = Self::verify_content_static(&content, verification, doc_id);

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
                    results.push((doc_id, full_path, rel_path, mtime, file_matches));
                }
            }
            results
        } else {
            // Large result set: use parallel memory-mapped reads with early termination
            let match_count = AtomicUsize::new(0);

            candidate_infos
                .into_par_iter()
                .with_min_len(4)
                .filter_map(|(doc_id, full_path, rel_path, mtime)| {
                    // Early termination check
                    if let Some(target) = target_matches {
                        if match_count.load(Ordering::Relaxed) >= target {
                            return None;
                        }
                    }

                    let content = read_file_mmap(&full_path)?;

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
                        match_count.fetch_add(file_matches.len(), Ordering::Relaxed);
                        Some((doc_id, full_path, rel_path, mtime, file_matches))
                    }
                })
                .collect()
        };

        Ok(all_matches)
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
            if let PlanStep::Filter(filter) = step
                && (filter.line_start.is_some() || filter.line_end.is_some())
            {
                return (filter.line_start, filter.line_end);
            }
        }
        (None, None)
    }

    /// Extract search terms from verification step for filename matching
    fn extract_search_terms(verification: &VerificationStep) -> Vec<String> {
        let mut terms = Vec::new();
        Self::collect_terms(verification, &mut terms);

        // Normalize to lowercase and de-duplicate while preserving insertion order.
        let mut seen = HashSet::with_capacity(terms.len());
        let mut normalized = Vec::with_capacity(terms.len());
        for term in terms {
            let lower = term.to_lowercase();
            if seen.insert(lower.clone()) {
                normalized.push(lower);
            }
        }

        normalized
    }

    /// Recursively collect terms from verification steps
    fn collect_terms(verification: &VerificationStep, terms: &mut Vec<String>) {
        match verification {
            VerificationStep::Literal(text) | VerificationStep::Phrase { text, .. } => {
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
            VerificationStep::Near {
                terms: near_terms, ..
            } => {
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
            VerificationStep::Phrase {
                text,
                case_insensitive,
            } => Self::find_literal_matches_static(content, text, !case_insensitive, doc_id),
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
    ///
    /// OPTIMIZATION: Lowercases the content once, then locates each term with a
    /// single SIMD substring search over the whole haystack. Match offsets are
    /// mapped to line numbers via precomputed line starts; newlines are
    /// unaffected by lowercasing, so line numbers computed on the lowered copy
    /// are valid for the original content.
    fn find_proximity_matches_static(
        content: &str,
        terms: &[String],
        distance: u32,
        _doc_id: DocId,
    ) -> Vec<(u32, String, usize, usize)> {
        use memchr::memmem;

        if terms.is_empty() {
            return Vec::new();
        }

        // Pre-lowercase all terms once
        let terms_lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();

        // A term containing a line break can never match within a single line
        if terms_lower
            .iter()
            .any(|t| t.contains('\n') || t.contains('\r'))
        {
            return Vec::new();
        }

        let content_lower = if content.is_ascii() {
            content.to_ascii_lowercase()
        } else {
            content.to_lowercase()
        };
        let lower_bytes = content_lower.as_bytes();

        // Line start offsets in the lowered content (for offset → line mapping)
        let mut line_starts: Vec<usize> = Vec::with_capacity(256);
        line_starts.push(0);
        for nl in memchr::memchr_iter(b'\n', lower_bytes) {
            line_starts.push(nl + 1);
        }

        // For each term, the sorted, deduped 1-based line numbers where it
        // appears; for the first term, also the column of its first occurrence
        // in each of those lines (byte offset in the lowered line).
        let mut term_lines: Vec<Vec<u32>> = Vec::with_capacity(terms.len());
        let mut first_term_cols: Vec<usize> = Vec::new();

        for (term_idx, term_lower) in terms_lower.iter().enumerate() {
            let finder = memmem::Finder::new(term_lower.as_bytes());
            let mut lines_with_term: Vec<u32> = Vec::new();
            let mut line_cursor = 0usize;

            for pos in finder.find_iter(lower_bytes) {
                // Match positions are ascending, so the cursor only moves forward
                while line_cursor + 1 < line_starts.len() && line_starts[line_cursor + 1] <= pos {
                    line_cursor += 1;
                }
                let line_num = (line_cursor + 1) as u32;
                if lines_with_term.last() == Some(&line_num) {
                    continue;
                }
                lines_with_term.push(line_num);
                if term_idx == 0 {
                    first_term_cols.push(pos - line_starts[line_cursor]);
                }
            }

            if lines_with_term.is_empty() {
                // One of the terms doesn't exist in the file - early exit
                return Vec::new();
            }

            term_lines.push(lines_with_term);
        }

        // Find line combinations where all terms are within distance
        let lines: Vec<&str> = content.lines().collect();
        let mut matches = Vec::new();

        // Start with lines containing the first term
        for (idx, &first_line) in term_lines[0].iter().enumerate() {
            // Check if all other terms have a match within distance
            // (each term's line list is sorted, so binary search the window)
            let all_within_distance = term_lines[1..].iter().all(|other_term_lines| {
                let lo = first_line.saturating_sub(distance);
                let hi = first_line + distance;
                let i = other_term_lines.partition_point(|&l| l < lo);
                i < other_term_lines.len() && other_term_lines[i] <= hi
            });

            if all_within_distance {
                // Found a valid proximity match - return the first term's match
                let line_idx = (first_line - 1) as usize;
                if let Some(&line) = lines.get(line_idx) {
                    let pos = first_term_cols[idx];
                    matches.push((first_line, line.to_string(), pos, pos + terms[0].len()));
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
            // Case-insensitive: pre-lowercase needle once
            let needle_lower = needle.to_lowercase();
            let finder = memmem::Finder::new(needle_lower.as_bytes());

            if content.is_ascii()
                && needle.is_ascii()
                && !needle_lower.contains('\n')
                && !needle_lower.contains('\r')
            {
                // ASCII fast path: lowercase the whole haystack once so byte
                // offsets map 1:1 to the original content, then run a single
                // SIMD substring search and map hits back to lines.
                let content_lower = content.to_ascii_lowercase();
                let bytes = content.as_bytes();
                let mut line_start = 0usize;
                let mut line_num: u32 = 1;
                let mut last_matched_line: u32 = 0;

                for pos in finder.find_iter(content_lower.as_bytes()) {
                    // Advance line tracking to the line containing this match
                    while let Some(nl) = memchr::memchr(b'\n', &bytes[line_start..pos]) {
                        line_start += nl + 1;
                        line_num += 1;
                    }
                    if line_num == last_matched_line {
                        continue; // Only report first match per line
                    }
                    last_matched_line = line_num;

                    let mut line_end = memchr::memchr(b'\n', &bytes[line_start..])
                        .map(|nl| line_start + nl)
                        .unwrap_or(bytes.len());
                    if line_end > line_start && bytes[line_end - 1] == b'\r' {
                        line_end -= 1;
                    }
                    let col = pos - line_start;
                    matches.push((
                        line_num,
                        content[line_start..line_end].to_string(),
                        col,
                        col + needle.len(),
                    ));
                }
            } else {
                // Unicode path: lowercasing changes byte offsets, so match
                // line by line. ASCII lines reuse a scratch buffer to avoid
                // a String allocation per line.
                let mut scratch: Vec<u8> = Vec::new();

                for (line_num, line) in content.lines().enumerate() {
                    // Quick rejection: if line is shorter than needle, skip
                    if line.len() < needle_lower.len() {
                        continue;
                    }

                    let pos = if line.is_ascii() && needle.is_ascii() {
                        scratch.clear();
                        scratch.extend(line.as_bytes().iter().map(|b| b.to_ascii_lowercase()));
                        finder.find(&scratch)
                    } else {
                        let line_lower = line.to_lowercase();
                        finder.find(line_lower.as_bytes())
                    };

                    if let Some(pos) = pos {
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
                matches.push(((line_num + 1) as u32, line.to_string(), m.start(), m.end()));
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
                // One document lookup per element instead of two per comparison
                results.sort_by_cached_key(|m| {
                    std::cmp::Reverse(
                        self.reader
                            .get_document(m.doc_id)
                            .map(|d| d.mtime)
                            .unwrap_or(0),
                    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::reader::IndexReader;
    use crate::query::parser::parse_query;
    use std::fs;
    use tempfile::TempDir;

    /// Create a test index with multiple files for comprehensive testing
    fn create_test_index() -> (TempDir, PathBuf, IndexReader) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root_path = temp_dir.path().to_path_buf();

        // Create multiple test files
        fs::write(
            root_path.join("main.rs"),
            r#"fn main() {
    println!("Hello, world!");
    let x = 42;
}

fn helper() {
    // TODO: implement
}
"#,
        )
        .expect("Failed to write main.rs");

        fs::write(
            root_path.join("lib.rs"),
            r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }
}
"#,
        )
        .expect("Failed to write lib.rs");

        fs::write(
            root_path.join("utils.py"),
            r#"def format_error(msg: str) -> str:
    return f"ERROR: {msg}"

def format_warning(msg: str) -> str:
    return f"WARNING: {msg}"
"#,
        )
        .expect("Failed to write utils.py");

        // Build index
        crate::index::build::build_index(&root_path, false).expect("Failed to build index");

        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        (temp_dir, root_path, reader)
    }

    #[test]
    fn test_executor_simple_search() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("fn main");
        let results = executor.execute(&query);

        assert!(results.is_ok(), "Search should succeed");
        let results = results.unwrap();
        assert!(!results.is_empty(), "Should find matches for 'fn main'");

        // Should find main.rs
        assert!(
            results
                .iter()
                .any(|m| m.path.to_string_lossy().contains("main.rs")),
            "Should find main.rs"
        );
    }

    #[test]
    fn test_executor_phrase_search() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("\"Hello, world\"");
        let results = executor.execute(&query);

        assert!(results.is_ok(), "Phrase search should succeed");
        let results = results.unwrap();
        assert!(!results.is_empty(), "Should find exact phrase");
    }

    #[test]
    fn test_executor_no_results() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("xyznonexistent123abc");
        let results = executor.execute(&query);

        assert!(
            results.is_ok(),
            "Search should succeed even with no matches"
        );
        let results = results.unwrap();
        assert!(results.is_empty(), "Should find no matches");
    }

    #[test]
    fn test_executor_extension_filter() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("ext:rs fn");
        let results = executor.execute(&query);

        assert!(results.is_ok(), "Extension filter search should succeed");
        let results = results.unwrap();

        // All results should be .rs files
        for result in &results {
            assert!(
                result.path.extension().map(|e| e == "rs").unwrap_or(false),
                "All results should be .rs files"
            );
        }
    }

    #[test]
    fn test_executor_language_filter() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("lang:python def");
        let results = executor.execute(&query);

        assert!(results.is_ok(), "Language filter search should succeed");
        let results = results.unwrap();

        // Should find Python files
        assert!(
            results
                .iter()
                .any(|m| m.path.to_string_lossy().contains(".py")),
            "Should find Python files"
        );
    }

    #[test]
    fn test_executor_with_limit() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let mut query = parse_query("fn");
        query.options.limit = 1;
        let results = executor.execute(&query);

        assert!(results.is_ok(), "Limited search should succeed");
        let results = results.unwrap();
        assert!(results.len() <= 1, "Should respect limit of 1");
    }

    #[test]
    fn test_verify_content_literal() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let verification = VerificationStep::Literal("println".to_string());

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);

        assert!(!matches.is_empty(), "Should find literal match");
        assert_eq!(matches[0].0, 2, "Match should be on line 2");
    }

    #[test]
    fn test_verify_content_phrase() {
        let content = "fn main() {\n    println!(\"hello world\");\n}\n";
        let verification = VerificationStep::Phrase {
            text: "hello world".to_string(),
            case_insensitive: false,
        };

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);

        assert!(!matches.is_empty(), "Should find phrase match");
    }

    #[test]
    fn test_verify_content_regex() {
        let content = "fn main() {\n    let x = 42;\n    let y = 123;\n}\n";
        let verification = VerificationStep::Regex(r"\d+".to_string());

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);

        assert!(matches.len() >= 2, "Should find at least 2 number matches");
    }

    #[test]
    fn test_verify_content_and() {
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let verification = VerificationStep::And(vec![
            VerificationStep::Literal("fn".to_string()),
            VerificationStep::Literal("main".to_string()),
        ]);

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);

        assert!(!matches.is_empty(), "Should find AND match");
    }

    #[test]
    fn test_verify_content_or() {
        let content = "fn helper() {\n    // nothing here\n}\n";
        let verification = VerificationStep::Or(vec![
            VerificationStep::Literal("main".to_string()),
            VerificationStep::Literal("helper".to_string()),
        ]);

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);

        assert!(!matches.is_empty(), "Should find OR match (helper)");
    }

    #[test]
    fn test_extract_context_lines() {
        let content = "line1\nline2\nline3\nline4\nline5\n";
        let lines: Vec<&str> = content.lines().collect();

        let (before, after) = QueryExecutor::extract_context_from_lines(&lines, 3, 1, 1);

        assert_eq!(before.len(), 1, "Should have 1 line before");
        assert_eq!(after.len(), 1, "Should have 1 line after");
        assert!(before[0].1.contains("line2"), "Before should be line2");
        assert!(after[0].1.contains("line4"), "After should be line4");
    }

    #[test]
    fn test_extract_context_lines_at_start() {
        let content = "line1\nline2\nline3\n";
        let lines: Vec<&str> = content.lines().collect();

        let (before, after) = QueryExecutor::extract_context_from_lines(&lines, 1, 2, 1);

        assert!(before.is_empty(), "Should have no lines before line 1");
        assert_eq!(after.len(), 1, "Should have 1 line after");
    }

    #[test]
    fn test_extract_context_lines_at_end() {
        let content = "line1\nline2\nline3\n";
        let lines: Vec<&str> = content.lines().collect();

        let (before, after) = QueryExecutor::extract_context_from_lines(&lines, 3, 1, 2);

        assert_eq!(before.len(), 1, "Should have 1 line before");
        assert!(after.is_empty(), "Should have no lines after line 3");
    }

    #[test]
    fn test_parse_language() {
        assert_eq!(parse_language("rust"), Language::Rust);
        assert_eq!(parse_language("rs"), Language::Rust);
        assert_eq!(parse_language("PYTHON"), Language::Python);
        assert_eq!(parse_language("Py"), Language::Python);
        assert_eq!(parse_language("javascript"), Language::JavaScript);
        assert_eq!(parse_language("JS"), Language::JavaScript);
        assert_eq!(parse_language("unknown_lang"), Language::Unknown);
    }

    #[test]
    fn test_find_literal_matches_case_sensitive() {
        let content = "Hello World\nhello world\nHELLO WORLD\n";

        let matches = QueryExecutor::find_literal_matches_static(content, "Hello", true, 1);

        assert_eq!(
            matches.len(),
            1,
            "Case-sensitive should find exactly 1 match"
        );
        assert_eq!(matches[0].0, 1, "Match should be on line 1");
    }

    #[test]
    fn test_find_literal_matches_case_insensitive() {
        let content = "Hello World\nhello world\nHELLO WORLD\n";

        let matches = QueryExecutor::find_literal_matches_static(content, "hello", false, 1);

        assert_eq!(
            matches.len(),
            3,
            "Case-insensitive should find all 3 matches"
        );
    }

    #[test]
    fn test_find_literal_matches_first_per_line_and_columns() {
        let content = "foo foo foo\nbar\nFOO\n";

        let matches = QueryExecutor::find_literal_matches_static(content, "foo", false, 1);

        // Only first match per line is reported
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], (1, "foo foo foo".to_string(), 0, 3));
        assert_eq!(matches[1], (3, "FOO".to_string(), 0, 3));
    }

    #[test]
    fn test_find_literal_matches_crlf() {
        let content = "alpha\r\nBETA gamma\r\ndelta\r\n";

        let matches = QueryExecutor::find_literal_matches_static(content, "beta", false, 1);

        assert_eq!(matches.len(), 1);
        // Line content must not include the trailing \r
        assert_eq!(matches[0], (2, "BETA gamma".to_string(), 0, 4));
    }

    #[test]
    fn test_find_literal_matches_no_trailing_newline() {
        let content = "first\nlast line match";

        let matches = QueryExecutor::find_literal_matches_static(content, "MATCH", false, 1);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], (2, "last line match".to_string(), 10, 15));
    }

    #[test]
    fn test_find_literal_matches_unicode() {
        let content = "héllo wörld\nplain ascii line\nHÉLLO again\n";

        let matches = QueryExecutor::find_literal_matches_static(content, "héllo", false, 1);

        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].0, 1);
        assert_eq!(matches[1].0, 3);

        // ASCII needle in a file with non-ASCII lines (scratch-buffer path)
        let matches = QueryExecutor::find_literal_matches_static(content, "ASCII", false, 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], (2, "plain ascii line".to_string(), 6, 11));
    }

    #[test]
    fn test_find_proximity_matches_within_distance() {
        let content = "error here\nfiller\nfiller\nhandle it\n";

        // 3 lines apart, distance 3 → match reported on the first term's line
        let terms = vec!["error".to_string(), "handle".to_string()];
        let matches = QueryExecutor::find_proximity_matches_static(content, &terms, 3, 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], (1, "error here".to_string(), 0, 5));

        // distance 2 → too far apart
        let matches = QueryExecutor::find_proximity_matches_static(content, &terms, 2, 1);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_find_proximity_matches_missing_term() {
        let content = "error here\nsomething else\n";
        let terms = vec!["error".to_string(), "nonexistent".to_string()];

        let matches = QueryExecutor::find_proximity_matches_static(content, &terms, 10, 1);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_find_proximity_matches_case_insensitive() {
        let content = "ERROR detected\nHANDLE this\n";
        let terms = vec!["error".to_string(), "handle".to_string()];

        let matches = QueryExecutor::find_proximity_matches_static(content, &terms, 5, 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, 1);
    }

    // ========================================================================
    // NOT operator executor tests
    // ========================================================================

    #[test]
    fn test_verify_content_not_excludes() {
        // Content that DOES contain the negated term → no match
        let content = "fn main() {\n    println!(\"hello\");\n}\n";
        let verification =
            VerificationStep::Not(Box::new(VerificationStep::Literal("println".to_string())));

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            matches.is_empty(),
            "NOT should produce no matches when term is present"
        );
    }

    #[test]
    fn test_verify_content_not_includes() {
        // Content that does NOT contain the negated term → match
        let content = "fn main() {\n    let x = 42;\n}\n";
        let verification =
            VerificationStep::Not(Box::new(VerificationStep::Literal("println".to_string())));

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            !matches.is_empty(),
            "NOT should produce a match when term is absent"
        );
    }

    #[test]
    fn test_verify_content_and_with_not() {
        // "fn -println" → has fn, but NOT println
        let content_with_both = "fn main() { println!(\"hi\"); }";
        let content_without = "fn helper() { let x = 1; }";

        let verification = VerificationStep::And(vec![
            VerificationStep::Literal("fn".to_string()),
            VerificationStep::Not(Box::new(VerificationStep::Literal("println".to_string()))),
        ]);

        let matches_both =
            QueryExecutor::verify_content_static(content_with_both, &verification, 1);
        assert!(
            matches_both.is_empty(),
            "Should NOT match when negated term is present"
        );

        let matches_without =
            QueryExecutor::verify_content_static(content_without, &verification, 1);
        assert!(
            !matches_without.is_empty(),
            "Should match when negated term is absent"
        );
    }

    #[test]
    fn test_executor_not_operator() {
        // The Exclude step now verifies candidates by reading file content before
        // excluding, preventing trigram false positives from causing over-exclusion.
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root_path = temp_dir.path().to_path_buf();

        fs::write(
            root_path.join("has_excluded.rs"),
            "pub fn xyzabc_unique_marker() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(
            root_path.join("no_excluded.rs"),
            "pub fn xyzabc_unique_marker() {\n    let result = 42;\n}\n",
        )
        .unwrap();
        // Add padding files to ensure trigram index has some noise
        for i in 0..20 {
            fs::write(
                root_path.join(format!("padding_{}.txt", i)),
                format!("padding content number {} with various words\n", i),
            )
            .unwrap();
        }

        crate::index::build::build_index(&root_path, false).expect("Failed to build index");
        let reader = IndexReader::open(&root_path).expect("Failed to open index");
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("xyzabc_unique_marker -println");
        let results = executor.execute(&query).unwrap();

        // Must NOT contain files with "println"
        for result in &results {
            let path_str = result.path.to_string_lossy();
            assert!(
                !path_str.contains("has_excluded"),
                "File with println should never appear in NOT results, got: {}",
                path_str
            );
        }

        // Must contain the file WITHOUT "println" (the fix ensures this)
        let has_no_excluded = results
            .iter()
            .any(|r| r.path.to_string_lossy().contains("no_excluded"));
        assert!(
            has_no_excluded,
            "File without println should appear in results (Exclude verification fix)"
        );
    }

    // ========================================================================
    // Near (proximity) executor tests
    // ========================================================================

    #[test]
    fn test_verify_content_near_within_distance() {
        // Terms on adjacent lines → within distance
        let content = "line1: function\nline2: return\nline3: end\n";
        let verification = VerificationStep::Near {
            terms: vec!["function".to_string(), "return".to_string()],
            distance: 2,
        };

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            !matches.is_empty(),
            "Near should match when terms are within distance"
        );
    }

    #[test]
    fn test_verify_content_near_beyond_distance() {
        // Terms far apart → should NOT match
        let content = "line1: function\nline2: a\nline3: b\nline4: c\nline5: d\nline6: e\nline7: f\nline8: g\nline9: h\nline10: return\n";
        let verification = VerificationStep::Near {
            terms: vec!["function".to_string(), "return".to_string()],
            distance: 2,
        };

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            matches.is_empty(),
            "Near should NOT match when terms are beyond distance (9 lines apart, distance=2)"
        );
    }

    #[test]
    fn test_verify_content_near_same_line() {
        // Both terms on the same line → always within distance
        let content = "fn main() { return 42; }\n";
        let verification = VerificationStep::Near {
            terms: vec!["main".to_string(), "return".to_string()],
            distance: 1,
        };

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            !matches.is_empty(),
            "Near should match when terms are on the same line"
        );
    }

    #[test]
    fn test_verify_content_near_missing_term() {
        // One term is absent entirely → no match
        let content = "fn main() {\n    let x = 42;\n}\n";
        let verification = VerificationStep::Near {
            terms: vec!["main".to_string(), "nonexistent".to_string()],
            distance: 100,
        };

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            matches.is_empty(),
            "Near should NOT match when a term is missing"
        );
    }

    #[test]
    fn test_executor_near_search() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // "near:println,Hello,3" - should find main.rs where println and Hello are close
        let query = parse_query("near:println,Hello,3");
        let results = executor.execute(&query);
        assert!(results.is_ok());
        let results = results.unwrap();
        assert!(
            results
                .iter()
                .any(|m| m.path.to_string_lossy().contains("main.rs")),
            "Should find main.rs where println and Hello are close together"
        );
    }

    // ========================================================================
    // Size filter executor tests
    // ========================================================================

    #[test]
    fn test_executor_size_filter_min() {
        let (_temp_dir, root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Get actual file sizes for the test
        let main_size = fs::metadata(root_path.join("main.rs"))
            .expect("main.rs should exist")
            .len();

        // Search with size filter larger than all files → no results
        let query = parse_query(&format!("size:>{} fn", main_size + 10000));
        let results = executor.execute(&query).unwrap();
        assert!(results.is_empty(), "size:>huge should find no files");
    }

    #[test]
    fn test_executor_size_filter_max() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Search with very small max size → should exclude most/all files
        let query = parse_query("size:<1 fn");
        let results = executor.execute(&query).unwrap();
        assert!(
            results.is_empty(),
            "size:<1 should find no files (all files are > 0 bytes)"
        );
    }

    #[test]
    fn test_executor_size_filter_range() {
        let (_temp_dir, root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Get actual file sizes
        let main_size = fs::metadata(root_path.join("main.rs")).unwrap().len();
        let lib_size = fs::metadata(root_path.join("lib.rs")).unwrap().len();

        // Use a range that includes all our files
        let min_size = main_size.min(lib_size);
        let query = parse_query(&format!("size:>0 size:<{} fn", min_size + 1));
        let results = executor.execute(&query).unwrap();

        // Should find at least the smallest file
        assert!(
            !results.is_empty(),
            "size range should find at least one file"
        );
    }

    // ========================================================================
    // Mtime filter executor tests
    // ========================================================================

    #[test]
    fn test_executor_mtime_filter_future() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // mtime is stored as seconds (matching the query parser's interpretation)
        let future_secs = 4102444800u64; // year ~2100 in seconds
        let query = parse_query(&format!("ext:rs mtime:>{}", future_secs));
        let results = executor.execute(&query).unwrap();
        assert!(
            results.is_empty(),
            "mtime filter in far future should find no files, got {} results",
            results.len()
        );
    }

    #[test]
    fn test_executor_mtime_filter_past() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // mtime is stored as seconds. Files modified after epoch → all files
        let query = parse_query("ext:rs mtime:>0");
        let results = executor.execute(&query).unwrap();
        assert!(
            !results.is_empty(),
            "mtime:>0 should find files (all files were recently created)"
        );
    }

    #[test]
    fn test_executor_mtime_filter_max_excludes_recent() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // mtime is stored as seconds. max=1 second → excludes all recent files
        let query = parse_query("ext:rs mtime:<1");
        let results = executor.execute(&query).unwrap();
        assert!(
            results.is_empty(),
            "mtime:<1 should find no files (all files are recently created)"
        );
    }

    #[test]
    fn test_executor_mtime_filter_with_real_timestamp() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // mtime is now stored as seconds, matching the parser's interpretation.
        // Use a real-world seconds timestamp (Jan 1, 2020) that all test files
        // (created just now) should be newer than.
        let query = parse_query("ext:rs mtime:>1577836800");
        let results = executor.execute(&query).unwrap();
        assert!(
            !results.is_empty(),
            "mtime:>1577836800 (Jan 2020 in seconds) should find recently created files"
        );
    }

    // ========================================================================
    // Line filter executor tests
    // ========================================================================

    #[test]
    fn test_executor_line_filter() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Search for "fn" but only on line 1
        let query = parse_query("line:1 fn");
        let results = executor.execute(&query).unwrap();

        // main.rs has "fn main()" on line 1, lib.rs has "pub fn add" on line 1
        // All matches should be on line 1
        for result in &results {
            assert_eq!(
                result.line_number, 1,
                "Line filter line:1 should only return matches on line 1, got line {}",
                result.line_number
            );
        }
    }

    #[test]
    fn test_executor_line_filter_range() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Search for "fn" in lines 5-10
        let query = parse_query("line:5-10 fn");
        let results = executor.execute(&query).unwrap();

        // All matches should be within the line range
        for result in &results {
            assert!(
                result.line_number >= 5 && result.line_number <= 10,
                "Line filter line:5-10 should only return matches in range, got line {}",
                result.line_number
            );
        }
    }

    #[test]
    fn test_executor_line_filter_out_of_range() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Search for "fn" on line 1000 — none of our test files are that long
        let query = parse_query("line:1000 fn");
        let results = executor.execute(&query).unwrap();
        assert!(
            results.is_empty(),
            "line:1000 should find no matches in small test files"
        );
    }

    // ========================================================================
    // Regex executor tests
    // ========================================================================

    #[test]
    fn test_executor_regex_search() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query(r"re:/fn\s+\w+/");
        let results = executor.execute(&query).unwrap();
        assert!(
            !results.is_empty(),
            r"Regex re:/fn\s+\w+/ should match function definitions"
        );
    }

    #[test]
    fn test_executor_regex_no_match() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query(r"re:/zzzznonexistent\d+/");
        let results = executor.execute(&query).unwrap();
        assert!(
            results.is_empty(),
            "Regex with no matching content should return empty"
        );
    }

    #[test]
    fn test_verify_content_regex_digits() {
        let content = "let x = 42;\nlet y = hello;\nlet z = 99;\n";
        let verification = VerificationStep::Regex(r"\d+".to_string());

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert_eq!(
            matches.len(),
            2,
            "Regex \\d+ should match lines with 42 and 99"
        );
    }

    #[test]
    fn test_verify_content_regex_no_match() {
        let content = "hello world\nfoo bar\n";
        let verification = VerificationStep::Regex(r"\d+".to_string());

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            matches.is_empty(),
            "Regex \\d+ should not match text-only content"
        );
    }

    // ========================================================================
    // Boost executor tests
    // ========================================================================

    #[test]
    fn test_executor_boosted_search() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // Boosted search should still find results (boost affects scoring, not filtering)
        let query = parse_query("^main");
        let results = executor.execute(&query).unwrap();
        assert!(!results.is_empty(), "Boosted search should find results");
        assert!(
            results
                .iter()
                .any(|m| m.path.to_string_lossy().contains("main.rs")),
            "Should find main.rs"
        );
    }

    // ========================================================================
    // OR executor tests
    // ========================================================================

    #[test]
    fn test_verify_content_or_neither_match() {
        let content = "fn main() {\n    let x = 42;\n}\n";
        let verification = VerificationStep::Or(vec![
            VerificationStep::Literal("nonexistent1".to_string()),
            VerificationStep::Literal("nonexistent2".to_string()),
        ]);

        let matches = QueryExecutor::verify_content_static(content, &verification, 1);
        assert!(
            matches.is_empty(),
            "OR with no matching terms should be empty"
        );
    }

    // ========================================================================
    // Parenthesized / compound executor tests
    // ========================================================================

    #[test]
    fn test_executor_grouped_or_with_and() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // (println | multiply) fn → files containing fn AND (println OR multiply)
        let query = parse_query("(println | multiply) fn");
        let results = executor.execute(&query).unwrap();
        assert!(
            !results.is_empty(),
            "Grouped OR with AND should find results"
        );
        // main.rs has println+fn, lib.rs has multiply+fn
        let paths: Vec<String> = results
            .iter()
            .map(|m| m.path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|p| p.contains("main.rs")),
            "Should find main.rs (has println and fn)"
        );
    }

    // ========================================================================
    // File/path filter executor tests
    // ========================================================================

    #[test]
    fn test_executor_file_filter() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("file:main.rs fn");
        let results = executor.execute(&query).unwrap();

        assert!(!results.is_empty(), "file:main.rs should find main.rs");
        for result in &results {
            assert!(
                result
                    .path
                    .file_name()
                    .map(|f| f == "main.rs")
                    .unwrap_or(false),
                "All results should be from main.rs"
            );
        }
    }

    #[test]
    fn test_executor_file_glob_filter() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        let query = parse_query("file:*.rs fn");
        let results = executor.execute(&query).unwrap();

        assert!(!results.is_empty(), "file:*.rs should find .rs files");
        for result in &results {
            assert!(
                result.path.extension().map(|e| e == "rs").unwrap_or(false),
                "file:*.rs should only return .rs files"
            );
        }
        // Should NOT include utils.py
        assert!(
            !results
                .iter()
                .any(|m| m.path.to_string_lossy().contains(".py")),
            "file:*.rs should not include .py files"
        );
    }

    // ========================================================================
    // Edge case: empty results
    // ========================================================================

    #[test]
    fn test_executor_all_filters_combined_no_match() {
        let (_temp_dir, _root_path, reader) = create_test_index();
        let executor = QueryExecutor::new(&reader);

        // ext:json with fn → config.json doesn't contain "fn"
        let query = parse_query("ext:json fn");
        let results = executor.execute(&query).unwrap();
        assert!(
            results.is_empty(),
            "ext:json with fn should find nothing (json file has no fn)"
        );
    }
}
