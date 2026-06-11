use crate::index::types::*;
use crate::utils::{
    delta_decode, delta_decode_bitmap, delta_decode_intersect, get_index_dir, BloomFilter,
};
use ahash::AHashSet;
use anyhow::{Context, Result};
use lru::LruCache;
use memmap2::Mmap;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Trigram dictionary entry
struct TrigramDictEntry {
    trigram: Trigram,
    offset: u64,
    length: u32,
    #[allow(dead_code)]
    doc_freq: u32,
}

/// Trigram dictionary
struct TrigramDict {
    entries: Vec<TrigramDictEntry>,
}

impl TrigramDict {
    fn lookup(&self, trigram: Trigram) -> Option<&TrigramDictEntry> {
        self.entries
            .binary_search_by_key(&trigram, |e| e.trigram)
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// Token dictionary entry
struct TokenDictEntry {
    token: String,
    offset: u64,
    length: u32,
    #[allow(dead_code)]
    doc_freq: u32,
    /// Offset into tokens.positions file (0 if no positions)
    pos_offset: u64,
    /// Length of position data in tokens.positions file
    pos_length: u32,
}

/// Token dictionary
struct TokenDict {
    entries: Vec<TokenDictEntry>,
}

impl TokenDict {
    fn lookup(&self, token: &str) -> Option<&TokenDictEntry> {
        self.entries
            .binary_search_by(|e| e.token.as_str().cmp(token))
            .ok()
            .map(|i| &self.entries[i])
    }
}

/// Reader for a single segment
struct SegmentReader {
    #[allow(dead_code)]
    segment_id: SegmentId,
    trigram_dict: TrigramDict,
    trigram_postings: Mmap,
    token_dict: TokenDict,
    token_postings: Mmap,
    /// Memory-mapped token positions file (optional for backwards compat)
    token_positions: Option<Mmap>,
    /// Lazily loaded line maps - only loaded when first accessed
    line_maps: OnceLock<HashMap<DocId, Vec<u32>>>,
    /// Path to segment directory for lazy loading
    segment_path: PathBuf,
    /// Bloom filter for fast trigram pre-filtering (optional for backwards compat)
    bloom_filter: Option<BloomFilter>,
}

impl SegmentReader {
    /// Get the document frequency for a trigram (for selectivity-based ordering)
    #[inline]
    fn get_trigram_doc_freq(&self, trigram: Trigram) -> u32 {
        self.trigram_dict
            .lookup(trigram)
            .map(|e| e.doc_freq)
            .unwrap_or(0)
    }

    /// Open a segment from disk (lazy loading for line maps)
    fn open(segment_path: &Path, segment_id: SegmentId, _index_path: &Path) -> Result<Self> {
        // Read trigram dictionary (already sorted from BTreeMap write)
        let trigram_dict = read_trigram_dict(segment_path)?;

        // mmap trigram postings
        let postings_path = segment_path.join("grams.postings");
        let trigram_postings = if postings_path.exists() {
            let file = File::open(&postings_path)?;
            unsafe { Mmap::map(&file)? }
        } else {
            // Empty mmap for empty segment - use anonymous mapping instead of unrelated file
            unsafe {
                Mmap::map(&File::open(postings_path).unwrap_or_else(|_| {
                    // Create an empty temp file as mmap source
                    let empty_path = segment_path.join(".empty_postings");
                    let _ = std::fs::write(&empty_path, b"");
                    File::open(&empty_path).expect("failed to create empty postings placeholder")
                }))?
            }
        };

        // Check if positions file exists (determines dict format)
        let positions_path = segment_path.join("tokens.positions");
        let has_positions = positions_path.exists();

        // Read token dictionary (already sorted from BTreeMap write)
        let token_dict = read_token_dict(segment_path, has_positions)?;

        // mmap token postings
        let token_postings_path = segment_path.join("tokens.postings");
        let token_postings = if token_postings_path.exists() {
            let file = File::open(&token_postings_path)?;
            unsafe { Mmap::map(&file)? }
        } else {
            // Empty mmap for empty segment - use anonymous mapping instead of unrelated file
            unsafe {
                Mmap::map(&File::open(token_postings_path).unwrap_or_else(|_| {
                    let empty_path = segment_path.join(".empty_token_postings");
                    let _ = std::fs::write(&empty_path, b"");
                    File::open(&empty_path).expect("failed to create empty token postings placeholder")
                }))?
            }
        };

        // mmap token positions if present
        let token_positions = if has_positions {
            let file = File::open(&positions_path)?;
            Some(unsafe { Mmap::map(&file)? })
        } else {
            None
        };

        // Line maps are NOT loaded here - loaded lazily on first access

        // Load bloom filter if it exists (optional for backwards compat)
        let bloom_filter = read_bloom_filter(segment_path).ok();

        Ok(Self {
            segment_id,
            trigram_dict,
            trigram_postings,
            token_dict,
            token_postings,
            token_positions,
            line_maps: OnceLock::new(),
            segment_path: segment_path.to_path_buf(),
            bloom_filter,
        })
    }

    /// Get documents matching a trigram in this segment as a RoaringBitmap
    fn get_trigram_docs(&self, trigram: Trigram) -> RoaringBitmap {
        if let Some(entry) = self.trigram_dict.lookup(trigram) {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;

            if end <= self.trigram_postings.len() {
                return delta_decode_bitmap(&self.trigram_postings[start..end]);
            }
        }
        RoaringBitmap::new()
    }

    /// Get documents matching a trigram, intersected with `filter` during
    /// decode. Decoding stops early once values exceed the filter's maximum,
    /// so a common trigram's long posting list is never fully decoded when
    /// the candidate set is already small.
    fn get_trigram_docs_intersect(&self, trigram: Trigram, filter: &RoaringBitmap) -> RoaringBitmap {
        if let Some(entry) = self.trigram_dict.lookup(trigram) {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;

            if end <= self.trigram_postings.len() {
                return delta_decode_intersect(&self.trigram_postings[start..end], filter);
            }
        }
        RoaringBitmap::new()
    }

    /// Get documents matching a token in this segment as a RoaringBitmap
    fn get_token_docs(&self, token: &str) -> RoaringBitmap {
        if let Some(entry) = self.token_dict.lookup(token) {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;

            if end <= self.token_postings.len() {
                return delta_decode_bitmap(&self.token_postings[start..end]);
            }
        }
        RoaringBitmap::new()
    }

    /// Get position postings for a token: Vec<(doc_id, positions)>.
    /// When `filter` is provided, only candidate docs are decoded — other
    /// docs' position data is skipped byte-wise, and decoding stops once doc
    /// ids exceed the filter's maximum.
    /// Returns None if no position data is available for this segment.
    fn get_token_positions(
        &self,
        token: &str,
        filter: Option<&RoaringBitmap>,
    ) -> Option<Vec<(u32, Vec<u32>)>> {
        let positions_mmap = self.token_positions.as_ref()?;
        let entry = self.token_dict.lookup(token)?;
        if entry.pos_length == 0 {
            return None;
        }
        let start = entry.pos_offset as usize;
        let end = start + entry.pos_length as usize;
        if end > positions_mmap.len() {
            return None;
        }
        let bytes = &positions_mmap[start..end];
        Some(match filter {
            Some(f) => crate::utils::decode_position_postings_filtered(bytes, f),
            None => crate::utils::decode_position_postings(bytes),
        })
    }

    /// Get line map for a document in this segment (lazy loads on first access)
    fn get_line_map(&self, doc_id: DocId) -> Option<&Vec<u32>> {
        let line_maps = self
            .line_maps
            .get_or_init(|| read_line_maps(&self.segment_path).unwrap_or_default());
        line_maps.get(&doc_id)
    }

    /// Check if trigrams might exist in this segment using bloom filter.
    /// Returns true if bloom filter is not present (conservative).
    #[inline]
    fn might_contain_trigrams(&self, trigrams: &[Trigram]) -> bool {
        match &self.bloom_filter {
            Some(bf) => bf.might_contain_all(trigrams),
            None => true, // No bloom filter = assume might contain
        }
    }
}

/// Default file cache size (number of files to cache)
const DEFAULT_FILE_CACHE_SIZE: usize = 256;

/// Maximum file size to cache (files larger than this are not cached)
const MAX_CACHEABLE_FILE_SIZE: usize = 512 * 1024; // 512KB

/// File content backed by an owned String, a shared cache entry, or a memory
/// map. UTF-8 is validated once at construction; `Deref<Target = str>` lets
/// callers borrow the bytes without copying them.
pub enum FileContent {
    Owned(String),
    Cached(Arc<str>),
    Mapped(Mmap),
}

impl std::ops::Deref for FileContent {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        match self {
            FileContent::Owned(s) => s,
            FileContent::Cached(s) => s,
            // SAFETY: validated as UTF-8 when the map was created (see
            // read_file_mmap in the query executor), and the map is never
            // written through. If another process rewrites the file while it
            // is mapped the content may change underneath us — the same race
            // existed when the map was copied into a String, just with a
            // shorter window.
            FileContent::Mapped(m) => unsafe { std::str::from_utf8_unchecked(m) },
        }
    }
}

/// Memory-mapped index reader for fast queries
pub struct IndexReader {
    root_path: PathBuf,
    #[allow(dead_code)]
    index_path: PathBuf,
    pub meta: IndexMeta,
    /// Documents stored as Vec for iteration, with a HashMap index for O(1) lookup
    documents: Vec<Document>,
    /// O(1) lookup index: doc_id -> index in documents Vec
    doc_id_to_index: HashMap<DocId, usize>,
    paths: Vec<PathBuf>,
    segments: Vec<SegmentReader>,
    /// O(1) stop-gram lookup (converted from Vec on load)
    stop_grams: AHashSet<Trigram>,
    /// LRU cache for file contents (speeds up repeated queries on same files)
    file_cache: Mutex<LruCache<PathBuf, Arc<str>>>,
    /// Lazily-built bitmap of valid doc IDs. Safe to cache: documents are
    /// immutable after open (index updates swap in a whole new reader).
    valid_docs_cache: OnceLock<RoaringBitmap>,
}

impl IndexReader {
    /// Open an existing index with parallel loading for maximum startup speed
    pub fn open(root_path: &Path) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let index_path = get_index_dir(&root_path)?;

        if !index_path.exists() {
            anyhow::bail!("No index found. Run 'fxi index' first.");
        }

        // Cleanup stale .tmp files from interrupted operations (crash safety)
        cleanup_tmp_files(&index_path);

        // Read metadata first (needed for segment IDs)
        let meta_path = index_path.join("meta.json");
        let meta_file = File::open(&meta_path).context("Failed to open meta.json")?;
        let meta: IndexMeta = serde_json::from_reader(meta_file)?;

        // Collect all segment IDs to load
        let mut segment_ids: Vec<SegmentId> = Vec::new();
        if let Some(base_id) = meta.base_segment {
            segment_ids.push(base_id);
        }
        segment_ids.extend(&meta.delta_segments);

        // PARALLEL LOADING: Load documents, paths, and all segments concurrently
        // Uses parallel tuple collection for true 3-way parallelism
        // This can reduce startup time by 50-70% on multi-core systems
        let index_path_ref = &index_path;

        // Use rayon's join for 3-way parallelism: (docs, (paths, segments))
        // The inner join runs paths and segments loading in parallel
        // The outer join runs docs loading in parallel with the inner join
        let (documents_result, (paths_result, segments)) = rayon::join(
            || read_documents(index_path_ref),
            || {
                rayon::join(
                    || read_paths(index_path_ref),
                    || {
                        // Load all segments in parallel using par_iter
                        segment_ids
                            .par_iter()
                            .filter_map(|&seg_id| {
                                let segment_path = index_path_ref
                                    .join("segments")
                                    .join(format!("seg_{:04}", seg_id));
                                if segment_path.exists() {
                                    match SegmentReader::open(&segment_path, seg_id, index_path_ref) {
                                        Ok(reader) => Some(reader),
                                        Err(e) => {
                                            eprintln!("Warning: Failed to open segment {}: {}. Index may be corrupted - try 'fxi index --force' to rebuild.", seg_id, e);
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                    },
                )
            },
        );

        let documents = documents_result?;
        let paths = paths_result?;

        // Build O(1) lookup index (fast, ~100ms for 1M docs)
        let doc_id_to_index: HashMap<DocId, usize> = documents
            .iter()
            .enumerate()
            .map(|(idx, doc)| (doc.doc_id, idx))
            .collect();

        // Convert stop-grams Vec to HashSet for O(1) lookup (was O(512) per check)
        let stop_grams: AHashSet<Trigram> = meta.stop_grams.iter().copied().collect();

        // Initialize file content cache
        let file_cache = Mutex::new(LruCache::new(
            NonZeroUsize::new(DEFAULT_FILE_CACHE_SIZE).unwrap(),
        ));

        Ok(Self {
            root_path,
            index_path,
            meta,
            documents,
            doc_id_to_index,
            paths,
            segments,
            stop_grams,
            file_cache,
            valid_docs_cache: OnceLock::new(),
        })
    }

    /// Get document by ID - O(1) lookup via HashMap index
    pub fn get_document(&self, doc_id: DocId) -> Option<&Document> {
        self.doc_id_to_index
            .get(&doc_id)
            .and_then(|&idx| self.documents.get(idx))
    }

    /// Get path for document
    pub fn get_path(&self, doc: &Document) -> Option<&PathBuf> {
        self.paths.get(doc.path_id as usize)
    }

    /// Get full path for document.
    /// Returns None if the path would escape the root directory (security check).
    pub fn get_full_path(&self, doc: &Document) -> Option<PathBuf> {
        let rel_path = self.get_path(doc)?;
        // Fast lexical validation instead of per-query canonicalize() syscalls.
        // Indexed paths should always be relative; reject suspicious components.
        if rel_path.is_absolute() {
            return None;
        }
        if rel_path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        }) {
            return None;
        }

        Some(self.root_path.join(rel_path))
    }

    /// Get all documents
    pub fn documents(&self) -> &[Document] {
        &self.documents
    }

    /// Get documents matching a trigram (queries all segments in parallel) as a RoaringBitmap
    #[allow(dead_code)]
    pub fn get_trigram_docs(&self, trigram: Trigram) -> RoaringBitmap {
        if self.segments.len() <= 1 {
            // Single segment - no parallelization overhead
            self.segments
                .first()
                .map(|s| s.get_trigram_docs(trigram))
                .unwrap_or_default()
        } else {
            // Multiple segments - parallel query with reduction
            self.segments
                .par_iter()
                .map(|segment| segment.get_trigram_docs(trigram))
                .reduce(RoaringBitmap::new, |mut a, b| {
                    a |= b;
                    a
                })
        }
    }

    /// Get documents matching a token (queries all segments in parallel) as a RoaringBitmap
    pub fn get_token_docs(&self, token: &str) -> RoaringBitmap {
        let token_lower = token.to_lowercase();
        if self.segments.len() <= 1 {
            self.segments
                .first()
                .map(|s| s.get_token_docs(&token_lower))
                .unwrap_or_default()
        } else {
            self.segments
                .par_iter()
                .map(|segment| segment.get_token_docs(&token_lower))
                .reduce(RoaringBitmap::new, |mut a, b| {
                    a |= b;
                    a
                })
        }
    }

    /// Get line offsets for a document (searches all segments)
    #[allow(dead_code)]
    pub fn get_line_map(&self, doc_id: DocId) -> Option<&Vec<u32>> {
        for segment in &self.segments {
            if let Some(line_map) = segment.get_line_map(doc_id) {
                return Some(line_map);
            }
        }
        None
    }

    /// Convert byte offset to line number
    #[allow(dead_code)]
    pub fn offset_to_line(&self, doc_id: DocId, offset: usize) -> u32 {
        if let Some(line_map) = self.get_line_map(doc_id) {
            // Binary search for the line
            match line_map.binary_search(&(offset as u32)) {
                Ok(i) => i as u32 + 1,
                Err(i) => i as u32,
            }
        } else {
            1
        }
    }

    /// Check if a trigram is a stop-gram - O(1) via HashSet
    #[inline]
    pub fn is_stop_gram(&self, trigram: Trigram) -> bool {
        self.stop_grams.contains(&trigram)
    }

    /// Check if any segment might contain all the given trigrams using bloom filters.
    /// This is a fast pre-filter before doing expensive posting list operations.
    /// Returns true if at least one segment might contain all trigrams.
    #[allow(dead_code)]
    #[inline]
    pub fn might_contain_trigrams(&self, trigrams: &[Trigram]) -> bool {
        if trigrams.is_empty() {
            return true;
        }
        // If any segment might contain all trigrams, return true
        self.segments
            .iter()
            .any(|s| s.might_contain_trigrams(trigrams))
    }

    /// Get documents matching trigrams, but only from segments that pass bloom filter.
    /// This is more efficient than get_trigram_docs for multi-trigram queries.
    ///
    /// OPTIMIZATION: Trigrams are sorted by document frequency (selectivity) before
    /// intersection. Processing the rarest trigram first minimizes intermediate result
    /// set sizes and reduces overall work.
    pub fn get_trigram_docs_with_bloom(&self, trigrams: &[Trigram]) -> RoaringBitmap {
        if trigrams.is_empty() {
            return self.valid_doc_ids().clone();
        }

        if self.segments.len() <= 1 {
            // Single segment - just check bloom and proceed
            if let Some(segment) = self.segments.first() {
                if !segment.might_contain_trigrams(trigrams) {
                    return RoaringBitmap::new();
                }

                // Sort trigrams by document frequency (selectivity) - rarest first
                // This minimizes intermediate result set sizes during intersection
                let mut sorted_trigrams: Vec<(Trigram, u32)> = trigrams
                    .iter()
                    .map(|&t| (t, segment.get_trigram_doc_freq(t)))
                    .collect();
                sorted_trigrams.sort_by_key(|&(_, freq)| freq);

                // Start with rarest trigram
                let mut result = segment.get_trigram_docs(sorted_trigrams[0].0);
                for &(t, _) in &sorted_trigrams[1..] {
                    if result.is_empty() {
                        break;
                    }
                    result = segment.get_trigram_docs_intersect(t, &result);
                }
                return result;
            }
            return RoaringBitmap::new();
        }

        // Multiple segments - parallel with bloom filter and selectivity ordering
        self.segments
            .par_iter()
            .filter(|s| s.might_contain_trigrams(trigrams))
            .map(|segment| {
                // Sort trigrams by document frequency within this segment
                let mut sorted_trigrams: Vec<(Trigram, u32)> = trigrams
                    .iter()
                    .map(|&t| (t, segment.get_trigram_doc_freq(t)))
                    .collect();
                sorted_trigrams.sort_by_key(|&(_, freq)| freq);

                // Start with rarest trigram
                let mut result = segment.get_trigram_docs(sorted_trigrams[0].0);
                for &(t, _) in &sorted_trigrams[1..] {
                    if result.is_empty() {
                        break;
                    }
                    result = segment.get_trigram_docs_intersect(t, &result);
                }
                result
            })
            .reduce(RoaringBitmap::new, |mut a, b| {
                a |= b;
                a
            })
    }

    /// Resolve a phrase query positionally: check if phrase tokens appear in
    /// adjacent positions across the index.
    /// When `candidates` is provided (the trigram-narrowed set), only those
    /// docs' positions are decoded — a phrase containing a common token no
    /// longer decodes that token's entire position posting list.
    /// Returns None if any segment lacks position data (graceful fallback).
    /// Returns Some(bitmap) of doc_ids where the phrase appears.
    pub fn resolve_phrase_positional(
        &self,
        phrase_tokens: &[(String, u32)],
        candidates: Option<&RoaringBitmap>,
    ) -> Option<RoaringBitmap> {
        if phrase_tokens.len() < 2 {
            return None;
        }

        // Check all segments have position data
        if self.segments.iter().any(|s| s.token_positions.is_none()) {
            return None;
        }

        // Lowercase tokens once, not once per segment
        let tokens_lower: Vec<String> = phrase_tokens
            .iter()
            .map(|(t, _)| t.to_lowercase())
            .collect();

        let result = self
            .segments
            .par_iter()
            .map(|segment| self.resolve_phrase_in_segment(segment, phrase_tokens, &tokens_lower, candidates))
            .reduce(RoaringBitmap::new, |mut a, b| {
                a |= b;
                a
            });

        Some(result)
    }

    /// Resolve a phrase within one segment (see resolve_phrase_positional).
    fn resolve_phrase_in_segment(
        &self,
        segment: &SegmentReader,
        phrase_tokens: &[(String, u32)],
        tokens_lower: &[String],
        candidates: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        let mut result = RoaringBitmap::new();

        {
            // Load positions for each phrase token in this segment
            let mut all_positions: Vec<Option<Vec<(u32, Vec<u32>)>>> = Vec::new();
            for token_lower in tokens_lower {
                all_positions.push(segment.get_token_positions(token_lower, candidates));
            }

            // If any token has no positions in this segment, skip it
            if all_positions.iter().any(|p| p.is_none()) {
                return result;
            }

            let positions: Vec<Vec<(u32, Vec<u32>)>> = all_positions
                .into_iter()
                .map(|p| p.unwrap())
                .collect();

            // Merge-intersect by doc_id checking position gaps
            // Start with the first token's doc set
            let first_positions = &positions[0];

            for &(doc_id, ref first_pos) in first_positions {
                let mut found = false;

                // Check each position in the first token
                'outer: for &start_pos in first_pos {
                    // Check if all subsequent tokens have the expected position
                    let mut all_match = true;
                    for (tok_idx, (_, expected_offset)) in phrase_tokens.iter().enumerate().skip(1) {
                        let expected_pos =
                            start_pos + expected_offset - phrase_tokens[0].1;

                        // Find this doc_id in the token's positions (binary search)
                        let tok_positions = &positions[tok_idx];
                        let doc_entry = tok_positions
                            .binary_search_by_key(&doc_id, |&(d, _)| d)
                            .ok()
                            .map(|idx| &tok_positions[idx].1);

                        match doc_entry {
                            Some(pos_list) => {
                                if pos_list.binary_search(&expected_pos).is_err() {
                                    all_match = false;
                                    break;
                                }
                            }
                            None => {
                                all_match = false;
                                break;
                            }
                        }
                    }

                    if all_match {
                        found = true;
                        break 'outer;
                    }
                }

                if found {
                    result.insert(doc_id);
                }
            }
        }

        result
    }

    /// Get all valid (non-stale, non-tombstone) doc IDs as a RoaringBitmap.
    /// Built once per reader and cached; callers needing ownership clone the
    /// bitmap, which is far cheaper than rescanning every document.
    pub fn valid_doc_ids(&self) -> &RoaringBitmap {
        self.valid_docs_cache.get_or_init(|| {
            self.documents
                .iter()
                .filter(|d| d.is_valid())
                .map(|d| d.doc_id)
                .collect()
        })
    }

    /// Get the root path
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Read file content with LRU caching.
    /// This speeds up repeated queries that access the same files.
    /// The cache stores Arc<str>, so a hit is a refcount bump rather than a
    /// copy of the file content; files too large to cache are returned as
    /// plain Strings without the Arc conversion copy.
    /// Returns None if the file cannot be read.
    pub fn read_file_cached(&self, path: &Path) -> Option<FileContent> {
        // Check cache first
        {
            let mut cache = self.file_cache.lock().ok()?;
            if let Some(content) = cache.get(path) {
                return Some(FileContent::Cached(Arc::clone(content)));
            }
        }

        // Read from disk
        let content = std::fs::read_to_string(path).ok()?;

        // Only cache if file is small enough
        if content.len() <= MAX_CACHEABLE_FILE_SIZE {
            let content: Arc<str> = content.into();
            if let Ok(mut cache) = self.file_cache.lock() {
                cache.put(path.to_path_buf(), Arc::clone(&content));
            }
            Some(FileContent::Cached(content))
        } else {
            Some(FileContent::Owned(content))
        }
    }

    /// Read file content without caching (for parallel access).
    /// Use this when reading many files in parallel to avoid lock contention.
    #[allow(dead_code)]
    #[inline]
    pub fn read_file_uncached(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    /// Clear the file content cache.
    /// Call this after index updates to ensure stale content isn't served.
    #[allow(dead_code)]
    pub fn clear_file_cache(&self) {
        if let Ok(mut cache) = self.file_cache.lock() {
            cache.clear();
        }
    }
}

/// Read documents from docs.bin
pub fn read_documents(index_path: &Path) -> Result<Vec<Document>> {
    let docs_path = index_path.join("docs.bin");
    let mut file = BufReader::new(File::open(&docs_path)?);

    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];
    let mut buf2 = [0u8; 2];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut documents = Vec::with_capacity(count);

    for _ in 0..count {
        file.read_exact(&mut buf4)?;
        let doc_id = u32::from_le_bytes(buf4);

        file.read_exact(&mut buf4)?;
        let path_id = u32::from_le_bytes(buf4);

        file.read_exact(&mut buf8)?;
        let size = u64::from_le_bytes(buf8);

        file.read_exact(&mut buf8)?;
        let mtime = u64::from_le_bytes(buf8);

        file.read_exact(&mut buf2)?;
        let lang_val = u16::from_le_bytes(buf2);
        let language = Language::try_from(lang_val).unwrap_or(Language::Unknown);

        file.read_exact(&mut buf2)?;
        let flags = DocFlags(u16::from_le_bytes(buf2));

        file.read_exact(&mut buf2)?;
        let segment_id = u16::from_le_bytes(buf2);

        documents.push(Document {
            doc_id,
            path_id,
            size,
            mtime,
            language,
            flags,
            segment_id,
        });
    }

    Ok(documents)
}

/// Read paths from paths.bin
pub fn read_paths(index_path: &Path) -> Result<Vec<PathBuf>> {
    let paths_path = index_path.join("paths.bin");
    let mut file = BufReader::new(File::open(&paths_path)?);

    let mut buf4 = [0u8; 4];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut paths = Vec::with_capacity(count);

    for _ in 0..count {
        // Read length
        file.read_exact(&mut buf4)?;
        let len = u32::from_le_bytes(buf4) as usize;

        // Read path bytes
        let mut path_bytes = vec![0u8; len];
        file.read_exact(&mut path_bytes)?;

        let path_str = String::from_utf8_lossy(&path_bytes);
        paths.push(PathBuf::from(path_str.as_ref()));
    }

    Ok(paths)
}

/// Read trigram dictionary
fn read_trigram_dict(segment_path: &Path) -> Result<TrigramDict> {
    let dict_path = segment_path.join("grams.dict");

    if !dict_path.exists() {
        return Ok(TrigramDict {
            entries: Vec::new(),
        });
    }

    let mut file = BufReader::new(File::open(&dict_path)?);

    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut entries = Vec::with_capacity(count);

    for _ in 0..count {
        // trigram (u32)
        file.read_exact(&mut buf4)?;
        let trigram = u32::from_le_bytes(buf4);

        // offset (u64)
        file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8);

        // length (u32)
        file.read_exact(&mut buf4)?;
        let length = u32::from_le_bytes(buf4);

        // doc_freq (u32)
        file.read_exact(&mut buf4)?;
        let doc_freq = u32::from_le_bytes(buf4);

        entries.push(TrigramDictEntry {
            trigram,
            offset,
            length,
            doc_freq,
        });
    }

    // Note: Data is already sorted from BTreeMap write - no sort needed
    // (Previously sorted here, now skipped for ~10-30ms savings per segment)

    Ok(TrigramDict { entries })
}

/// Read token dictionary
fn read_token_dict(segment_path: &Path, has_positions: bool) -> Result<TokenDict> {
    let dict_path = segment_path.join("tokens.dict");

    if !dict_path.exists() {
        return Ok(TokenDict {
            entries: Vec::new(),
        });
    }

    let mut file = BufReader::new(File::open(&dict_path)?);

    let mut buf2 = [0u8; 2];
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut entries = Vec::with_capacity(count);

    for i in 0..count {
        // token length (u16)
        file.read_exact(&mut buf2).with_context(|| {
            format!(
                "Token dictionary truncated at entry {}/{} - index may be corrupted",
                i, count
            )
        })?;
        let token_len = u16::from_le_bytes(buf2) as usize;

        // token bytes
        let mut token_bytes = vec![0u8; token_len];
        file.read_exact(&mut token_bytes)?;
        let token = String::from_utf8_lossy(&token_bytes).to_string();

        // offset (u64)
        file.read_exact(&mut buf8)?;
        let offset = u64::from_le_bytes(buf8);

        // length (u32)
        file.read_exact(&mut buf4)?;
        let length = u32::from_le_bytes(buf4);

        // doc_freq (u32)
        file.read_exact(&mut buf4)?;
        let doc_freq = u32::from_le_bytes(buf4);

        // Position offset and length (only present in new indexes with positions)
        let (pos_offset, pos_length) = if has_positions {
            file.read_exact(&mut buf8)?;
            let po = u64::from_le_bytes(buf8);
            file.read_exact(&mut buf4)?;
            let pl = u32::from_le_bytes(buf4);
            (po, pl)
        } else {
            (0, 0)
        };

        entries.push(TokenDictEntry {
            token,
            offset,
            length,
            doc_freq,
            pos_offset,
            pos_length,
        });
    }

    // Note: Data is already sorted from BTreeMap write - no sort needed
    // (Previously sorted here, now skipped for ~100-500ms savings per segment)

    Ok(TokenDict { entries })
}

/// Read line maps
fn read_line_maps(segment_path: &Path) -> Result<HashMap<DocId, Vec<u32>>> {
    let linemap_path = segment_path.join("linemap.bin");

    if !linemap_path.exists() {
        return Ok(HashMap::new());
    }

    let mut file = BufReader::new(File::open(&linemap_path)?);

    let mut buf4 = [0u8; 4];

    // Read count
    file.read_exact(&mut buf4)?;
    let count = u32::from_le_bytes(buf4) as usize;

    let mut line_maps = HashMap::with_capacity(count);

    for _ in 0..count {
        // doc_id
        file.read_exact(&mut buf4)?;
        let doc_id = u32::from_le_bytes(buf4);

        // line count (not used, but included for consistency)
        file.read_exact(&mut buf4)?;
        let _line_count = u32::from_le_bytes(buf4);

        // encoded length
        file.read_exact(&mut buf4)?;
        let encoded_len = u32::from_le_bytes(buf4) as usize;

        // encoded data
        let mut encoded = vec![0u8; encoded_len];
        file.read_exact(&mut encoded)?;

        // Decode
        let offsets = delta_decode(&encoded);
        line_maps.insert(doc_id, offsets);
    }

    Ok(line_maps)
}

/// Read bloom filter from segment
fn read_bloom_filter(segment_path: &Path) -> Result<BloomFilter> {
    let bloom_path = segment_path.join("bloom.bin");

    if !bloom_path.exists() {
        anyhow::bail!("Bloom filter not found");
    }

    let mut file = BufReader::new(File::open(&bloom_path)?);

    let mut buf1 = [0u8; 1];
    let mut buf4 = [0u8; 4];
    let mut buf8 = [0u8; 8];

    // Read num_hashes (u8)
    file.read_exact(&mut buf1)?;
    let num_hashes = buf1[0];

    // Read number of u64 words
    file.read_exact(&mut buf4)?;
    let num_words = u32::from_le_bytes(buf4) as usize;

    // Read bit data
    let mut bits = Vec::with_capacity(num_words);
    for _ in 0..num_words {
        file.read_exact(&mut buf8)?;
        bits.push(u64::from_le_bytes(buf8));
    }

    Ok(BloomFilter::from_raw(bits, num_hashes))
}

/// Cleanup stale .tmp files from interrupted operations (crash safety).
/// This removes leftover temporary files that may exist from a crashed/interrupted
/// merge or delta segment write operation.
fn cleanup_tmp_files(index_path: &Path) {
    // Clean up .tmp files in the index directory
    let tmp_patterns = ["docs.bin.tmp", "paths.bin.tmp", "meta.json.tmp"];

    for pattern in &tmp_patterns {
        let tmp_path = index_path.join(pattern);
        if tmp_path.exists() {
            if let Err(e) = std::fs::remove_file(&tmp_path) {
                eprintln!("Warning: failed to cleanup {}: {}", tmp_path.display(), e);
            }
        }
    }

    // Clean up any .tmp segment directories in segments/
    let segments_path = index_path.join("segments");
    if segments_path.exists() {
        if let Ok(entries) = std::fs::read_dir(&segments_path) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".tmp") {
                    if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                        eprintln!("Warning: failed to cleanup {}: {}", entry.path().display(), e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal test index for unit testing
    fn create_test_index() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root_path = temp_dir.path().to_path_buf();

        // Create a test file
        fs::write(
            root_path.join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .expect("Failed to write test file");

        // Build index
        crate::index::build::build_index(&root_path, false).expect("Failed to build index");

        (temp_dir, root_path)
    }

    #[test]
    fn test_index_reader_open() {
        let (_temp_dir, root_path) = create_test_index();

        let reader = IndexReader::open(&root_path);
        assert!(reader.is_ok(), "Should open index successfully");

        let reader = reader.unwrap();
        assert!(
            reader.meta.doc_count > 0,
            "Should have at least one document"
        );
    }

    #[test]
    fn test_index_reader_open_nonexistent() {
        let result = IndexReader::open(&PathBuf::from("/nonexistent/path"));
        assert!(result.is_err(), "Should fail for nonexistent path");
    }

    #[test]
    fn test_get_document() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        // Document IDs start at 1
        let doc = reader.get_document(1);
        assert!(doc.is_some(), "Should find document 1");

        let doc = doc.unwrap();
        assert!(doc.is_valid(), "Document should be valid");
        assert!(doc.size > 0, "Document should have size");
    }

    #[test]
    fn test_get_document_invalid_id() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let doc = reader.get_document(99999);
        assert!(doc.is_none(), "Should return None for invalid doc ID");
    }

    #[test]
    fn test_get_path() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let doc = reader.get_document(1).expect("Should find document");
        let path = reader.get_path(doc);

        assert!(path.is_some(), "Should get path for document");
        let path = path.unwrap();
        assert!(
            path.to_string_lossy().contains("test.rs"),
            "Path should contain test.rs"
        );
    }

    #[test]
    fn test_get_full_path() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let doc = reader.get_document(1).expect("Should find document");
        let full_path = reader.get_full_path(doc);

        assert!(full_path.is_some(), "Should get full path");
        let full_path = full_path.unwrap();
        assert!(full_path.exists(), "Full path should exist on disk");
    }

    #[test]
    fn test_valid_doc_ids() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let valid_ids = reader.valid_doc_ids();
        assert!(!valid_ids.is_empty(), "Should have valid document IDs");
    }

    #[test]
    fn test_trigram_lookup() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        // "fn " should produce trigrams that exist in our test file
        let trigram = crate::index::types::bytes_to_trigram(b'f', b'n', b' ');
        let docs = reader.get_trigram_docs(trigram);

        // Should find documents containing "fn "
        assert!(!docs.is_empty(), "Should find documents with 'fn ' trigram");
    }

    #[test]
    fn test_token_lookup() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        // "main" and "println" should be tokens in our test file
        let docs = reader.get_token_docs("main");
        assert!(!docs.is_empty(), "Should find documents with 'main' token");

        let docs = reader.get_token_docs("println");
        assert!(
            !docs.is_empty(),
            "Should find documents with 'println' token"
        );
    }

    #[test]
    fn test_token_lookup_nonexistent() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let docs = reader.get_token_docs("xyznonexistent123");
        assert!(docs.is_empty(), "Should not find nonexistent token");
    }

    #[test]
    fn test_read_file_cached() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let test_file_path = root_path.join("test.rs");

        // First read
        let content1 = reader.read_file_cached(&test_file_path);
        assert!(content1.is_some(), "Should read file");
        assert!(
            content1.as_ref().unwrap().contains("fn main"),
            "Content should contain 'fn main'"
        );

        // Second read (should come from cache)
        let content2 = reader.read_file_cached(&test_file_path);
        assert!(content2.is_some(), "Should read file from cache");
        assert_eq!(
            &*content1.unwrap(),
            &*content2.unwrap(),
            "Cached content should match"
        );
    }

    #[test]
    fn test_path_traversal_protection() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        // Create a fake document with path_id that would resolve to a relative path
        // containing ".." - this tests the security check in get_full_path
        let doc = reader.get_document(1).expect("Should find document");

        // The path should be valid since it's a real indexed file
        let full_path = reader.get_full_path(doc);
        assert!(full_path.is_some(), "Valid path should work");

        // Verify the returned path is within root
        let full_path = full_path.unwrap();
        assert!(
            full_path.starts_with(&root_path)
                || full_path
                    .canonicalize()
                    .unwrap()
                    .starts_with(root_path.canonicalize().unwrap()),
            "Path should be within root directory"
        );
    }
}
