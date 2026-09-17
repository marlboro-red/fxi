use crate::index::types::*;
use crate::utils::{BloomFilter, delta_decode, delta_decode_bitmap, delta_decode_intersect};
use ahash::AHashSet;
use anyhow::{Context, Result};
use lru::LruCache;
use memmap2::Mmap;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Empty posting files are valid, but cannot be memory mapped on every OS.
struct MappedBytes(Option<Mmap>);
impl MappedBytes {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self(if file.metadata()?.len() == 0 {
            None
        } else {
            Some(unsafe { Mmap::map(&file)? })
        }))
    }
}
impl std::ops::Deref for MappedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.as_deref().unwrap_or(&[])
    }
}

/// Trigram dictionary entry
struct TrigramDictEntry {
    trigram: Trigram,
    offset: u64,
    length: u32,
    #[allow(dead_code)]
    doc_freq: u32,
}

/// Immutable on-disk fixed-width entries, decoded only when accessed.
struct TrigramDict {
    data: MappedBytes,
    count: usize,
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

fn posting_range_fits(offset: u64, length: u32, size: usize) -> bool {
    offset
        .checked_add(u64::from(length))
        .is_some_and(|end| end <= size as u64)
}

impl TrigramDict {
    fn entry(&self, index: usize) -> TrigramDictEntry {
        let bytes = &self.data[4 + index * 20..];
        TrigramDictEntry {
            trigram: le32(bytes),
            offset: le64(&bytes[4..]),
            length: le32(&bytes[12..]),
            doc_freq: le32(&bytes[16..]),
        }
    }
    fn iter(&self) -> impl Iterator<Item = TrigramDictEntry> + '_ {
        (0..self.count).map(|i| self.entry(i))
    }
    fn lookup(&self, trigram: Trigram) -> Option<TrigramDictEntry> {
        let mut lo = 0;
        let mut hi = self.count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = self.entry(mid);
            match entry.trigram.cmp(&trigram) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(entry),
            }
        }
        None
    }
}

struct TokenDictEntry<'a> {
    token: &'a str,
    offset: u64,
    length: u32,
    pos_offset: u64,
    pos_length: u32,
}

/// Retain only offsets into the immutable mapping, not one String/allocation
/// and a widened metadata record per token. The variable-width format stays
/// compatible with existing generations.
struct TokenDict {
    data: MappedBytes,
    offsets: Vec<usize>,
    has_positions: bool,
}

impl TokenDict {
    fn entry(&self, index: usize) -> TokenDictEntry<'_> {
        let bytes = &self.data[self.offsets[index]..];
        let len = u16::from_le_bytes(bytes[..2].try_into().unwrap()) as usize;
        let token =
            std::str::from_utf8(&bytes[2..2 + len]).expect("validated immutable token dictionary");
        let fields = &bytes[2 + len..];
        TokenDictEntry {
            token,
            offset: le64(fields),
            length: le32(&fields[8..]),
            pos_offset: if self.has_positions {
                le64(&fields[16..])
            } else {
                0
            },
            pos_length: if self.has_positions {
                le32(&fields[24..])
            } else {
                0
            },
        }
    }
    fn iter(&self) -> impl Iterator<Item = TokenDictEntry<'_>> {
        (0..self.offsets.len()).map(|i| self.entry(i))
    }
    fn lookup(&self, token: &str) -> Option<TokenDictEntry<'_>> {
        let mut lo = 0;
        let mut hi = self.offsets.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = self.entry(mid);
            match entry.token.cmp(token) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(entry),
            }
        }
        None
    }
}

/// Positive gram constraints that can be evaluated independently within each
/// segment. A valid document's postings belong to one segment, so intersection
/// distributes over the union of these disjoint document-ID partitions.
pub(crate) enum GramQuery {
    All,
    Empty,
    Terms(Vec<Trigram>),
    And(Vec<GramQuery>),
    Or(Vec<GramQuery>),
}

impl GramQuery {
    pub(crate) fn terms(grams: Vec<Trigram>) -> Self {
        if grams.is_empty() {
            Self::All
        } else {
            Self::Terms(grams)
        }
    }
    pub(crate) fn and(mut children: Vec<Self>) -> Self {
        if children.iter().any(|q| matches!(q, Self::Empty)) {
            return Self::Empty;
        }
        children.retain(|q| !matches!(q, Self::All));
        match children.len() {
            0 => Self::All,
            1 => children.pop().unwrap(),
            _ => Self::And(children),
        }
    }
    pub(crate) fn or(mut children: Vec<Self>) -> Self {
        if children.iter().any(|q| matches!(q, Self::All)) {
            return Self::All;
        }
        children.retain(|q| !matches!(q, Self::Empty));
        match children.len() {
            0 => Self::Empty,
            1 => children.pop().unwrap(),
            _ => Self::Or(children),
        }
    }
    fn in_segment(&self, segment: &SegmentReader, universe: &RoaringBitmap) -> RoaringBitmap {
        match self {
            Self::All => universe.clone(),
            Self::Empty => RoaringBitmap::new(),
            Self::Terms(grams) => segment.intersect_trigrams(grams),
            Self::And(children) => {
                let mut children = children.iter();
                let mut result = children
                    .next()
                    .map(|q| q.in_segment(segment, universe))
                    .unwrap_or_else(|| universe.clone());
                for child in children {
                    if result.is_empty() {
                        break;
                    }
                    result &= child.in_segment(segment, universe);
                }
                result
            }
            Self::Or(children) => {
                children
                    .iter()
                    .fold(RoaringBitmap::new(), |mut result, child| {
                        result |= child.in_segment(segment, universe);
                        result
                    })
            }
        }
    }
}

struct TokenIndex {
    dictionary: TokenDict,
    postings: MappedBytes,
    positions: Option<MappedBytes>,
}

impl TokenIndex {
    fn open(segment_path: &Path, required_positions: bool) -> Result<Self> {
        let positions_path = segment_path.join("tokens.positions");
        anyhow::ensure!(
            !required_positions || positions_path.is_file(),
            "Missing position index; rebuild the index"
        );
        let has_positions = positions_path.exists();
        let postings = MappedBytes::open(&segment_path.join("tokens.postings"))?;
        let positions = if has_positions {
            Some(MappedBytes::open(&positions_path)?)
        } else {
            None
        };
        let dictionary = read_token_dict(
            segment_path,
            postings.len(),
            positions.as_ref().map(|data| data.len()),
        )?;
        Ok(Self {
            dictionary,
            postings,
            positions,
        })
    }
}

/// Reader for a single segment
struct SegmentReader {
    #[allow(dead_code)]
    segment_id: SegmentId,
    trigram_dict: TrigramDict,
    trigram_postings: MappedBytes,
    tokens: OnceLock<std::result::Result<TokenIndex, String>>,
    required_positions: bool,
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
    fn open(
        segment_path: &Path,
        segment_id: SegmentId,
        required_positions: bool,
        load_tokens: bool,
    ) -> Result<Self> {
        // Read trigram dictionary (already sorted from BTreeMap write)
        let trigram_dict = read_trigram_dict(segment_path)?;

        let trigram_postings = MappedBytes::open(&segment_path.join("grams.postings"))?;

        // Validate each immutable gram record once.
        let mut previous_gram = None;
        for entry in trigram_dict.iter() {
            anyhow::ensure!(
                posting_range_fits(entry.offset, entry.length, trigram_postings.len()),
                "Truncated trigram postings"
            );
            anyhow::ensure!(
                previous_gram.is_none_or(|gram| gram < entry.trigram),
                "Unsorted trigram dictionary"
            );
            previous_gram = Some(entry.trigram);
        }
        // Line maps are NOT loaded here - loaded lazily on first access

        // Load bloom filter if it exists (optional for backwards compat)
        let bloom_filter = read_bloom_filter(segment_path).ok();

        let reader = Self {
            segment_id,
            trigram_dict,
            trigram_postings,
            tokens: OnceLock::new(),
            required_positions,
            line_maps: OnceLock::new(),
            segment_path: segment_path.to_path_buf(),
            bloom_filter,
        };
        if load_tokens {
            reader.ensure_tokens()?;
        }
        Ok(reader)
    }

    fn ensure_tokens(&self) -> Result<()> {
        self.tokens
            .get_or_init(|| {
                TokenIndex::open(&self.segment_path, self.required_positions)
                    .map_err(|error| format!("{error:#}"))
            })
            .as_ref()
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// Public reader constructors eagerly establish this invariant. Internal
    /// query-only readers must cross the fallible ensure_tokens barrier first.
    fn tokens(&self) -> &TokenIndex {
        self.tokens
            .get()
            .expect("token access requires ensure_tokens")
            .as_ref()
            .expect("token access requires successful validation")
    }

    fn intersect_trigrams(&self, trigrams: &[Trigram]) -> RoaringBitmap {
        if !self.might_contain_trigrams(trigrams) {
            return RoaringBitmap::new();
        }
        let mut sorted: Vec<_> = trigrams
            .iter()
            .map(|&g| (g, self.get_trigram_doc_freq(g)))
            .collect();
        sorted.sort_unstable_by_key(|&(_, frequency)| frequency);
        let mut result = self.get_trigram_docs(sorted[0].0);
        for &(gram, _) in &sorted[1..] {
            if result.is_empty() {
                break;
            }
            result = self.get_trigram_docs_intersect(gram, &result);
        }
        result
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
    fn get_trigram_docs_intersect(
        &self,
        trigram: Trigram,
        filter: &RoaringBitmap,
    ) -> RoaringBitmap {
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
        if let Some(entry) = self.tokens().dictionary.lookup(token) {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;

            if end <= self.tokens().postings.len() {
                return delta_decode_bitmap(&self.tokens().postings[start..end]);
            }
        }
        RoaringBitmap::new()
    }

    /// Union the postings of every dictionary token that CONTAINS `needle` as
    /// a substring. Any alphanumeric substring of the content lies inside a
    /// single token, so this recovers substring matches (e.g. "println"
    /// inside "eprintln") when trigram narrowing is unavailable.
    fn get_token_docs_containing(&self, needle: &str) -> RoaringBitmap {
        use memchr::memmem;
        let finder = memmem::Finder::new(needle.as_bytes());

        let mut result = RoaringBitmap::new();
        for entry in self.tokens().dictionary.iter() {
            if entry.token.len() >= needle.len() && finder.find(entry.token.as_bytes()).is_some() {
                let start = entry.offset as usize;
                let end = start + entry.length as usize;
                if end <= self.tokens().postings.len() {
                    result |= delta_decode_bitmap(&self.tokens().postings[start..end]);
                }
            }
        }
        result
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
        let positions_mmap = self.tokens().positions.as_ref()?;
        let entry = self.tokens().dictionary.lookup(token)?;
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
impl FileStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        }
    }
}

/// A shared process budget, allocated on demand rather than per reader.
const FILE_CACHE_SHARDS: usize = 16;
const FILE_CACHE_SHARD_ENTRIES: usize = 8192;
const FILE_CACHE_SHARD_BYTES: usize = 64 * 1024 * 1024;

/// Byte and entry limits both apply, including replacement accounting.
struct ContentCache {
    entries: LruCache<PathBuf, (FileStamp, Arc<str>)>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl ContentCache {
    fn new() -> Self {
        Self {
            // No eager allocation proportional to the maximum entry budget.
            entries: LruCache::unbounded(),
            bytes: 0,
            max_bytes: FILE_CACHE_SHARD_BYTES,
            max_entries: FILE_CACHE_SHARD_ENTRIES,
        }
    }

    fn put(&mut self, path: PathBuf, stamp: FileStamp, content: Arc<str>) {
        let size = content.len();
        if let Some((_, (_, old))) = self.entries.push(path, (stamp, content)) {
            self.bytes -= old.len();
        }
        self.bytes += size;
        while self.bytes > self.max_bytes || self.entries.len() > self.max_entries {
            if let Some((_, (_, old))) = self.entries.pop_lru() {
                self.bytes -= old.len();
            }
        }
    }
}

/// Limit each entry to one eighth of the default shard budget.
const MAX_CACHEABLE_FILE_SIZE: usize = 8 * 1024 * 1024;

struct SharedContentCache {
    shards: [Mutex<ContentCache>; FILE_CACHE_SHARDS],
    hasher: ahash::RandomState,
    max_bytes: usize,
    max_entry_bytes: usize,
}
impl SharedContentCache {
    fn acquire() -> Arc<Self> {
        // Release retained text when the last reader closes; while readers
        // coexist (including generation replacement), they share one budget.
        static SHARED: OnceLock<Mutex<Weak<SharedContentCache>>> = OnceLock::new();
        let mut shared = SHARED
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cache) = shared.upgrade() {
            return cache;
        }
        let max_bytes = std::env::var("FXI_CACHE_MIB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&mib| mib <= 4096)
            .and_then(|mib| mib.checked_mul(1024 * 1024))
            .unwrap_or(FILE_CACHE_SHARDS * FILE_CACHE_SHARD_BYTES);
        let cache = Arc::new(Self {
            shards: std::array::from_fn(|_| {
                let mut shard = ContentCache::new();
                shard.max_bytes = max_bytes / FILE_CACHE_SHARDS;
                Mutex::new(shard)
            }),
            hasher: ahash::RandomState::new(),
            max_bytes,
            max_entry_bytes: MAX_CACHEABLE_FILE_SIZE.min(max_bytes / FILE_CACHE_SHARDS),
        });
        *shared = Arc::downgrade(&cache);
        cache
    }
}

/// An owned source snapshot or a shared, immutable copy of one.
pub enum FileContent {
    Owned(String),
    Cached(Arc<str>),
}
impl std::ops::Deref for FileContent {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            FileContent::Owned(s) => s,
            FileContent::Cached(s) => s,
        }
    }
}

/// Full builds normally assign consecutive IDs in document-vector order.
/// Preserve sparse/reordered legacy and incremental layouts without allocating
/// an extra lookup table for the common contiguous case.
enum DocumentLookup {
    Contiguous(DocId),
    Sparse(HashMap<DocId, usize>),
}

impl DocumentLookup {
    fn new(documents: &[Document]) -> Self {
        if documents
            .windows(2)
            .all(|docs| docs[0].doc_id.checked_add(1) == Some(docs[1].doc_id))
        {
            Self::Contiguous(documents.first().map_or(0, |doc| doc.doc_id))
        } else {
            Self::Sparse(
                documents
                    .iter()
                    .enumerate()
                    .map(|(index, doc)| (doc.doc_id, index))
                    .collect(),
            )
        }
    }

    fn index(&self, id: DocId) -> Option<usize> {
        match self {
            Self::Contiguous(first) => id.checked_sub(*first).map(|offset| offset as usize),
            Self::Sparse(indices) => indices.get(&id).copied(),
        }
    }
}

/// Memory-mapped index reader for fast queries
pub struct IndexReader {
    _generation_lease: Option<File>,
    root_path: PathBuf,
    #[allow(dead_code)]
    index_path: PathBuf,
    pub meta: IndexMeta,
    /// Documents stored in on-disk order for iteration.
    documents: Vec<Document>,
    /// O(1) lookup index: doc_id -> index in documents Vec
    doc_id_to_index: DocumentLookup,
    paths: Vec<PathBuf>,
    segments: Vec<SegmentReader>,
    /// O(1) stop-gram lookup (converted from Vec on load)
    stop_grams: AHashSet<Trigram>,
    /// LRU cache for file contents (speeds up repeated queries on same files)
    file_cache: Arc<SharedContentCache>,
    content_cache_enabled: bool,
    /// Lazily-built bitmap of valid doc IDs. Safe to cache: documents are
    /// immutable after open (index updates swap in a whole new reader).
    valid_docs_cache: OnceLock<RoaringBitmap>,
    path_order_cache: OnceLock<Vec<DocId>>,
}

impl IndexReader {
    /// Open an existing index with parallel loading for maximum startup speed
    /// One-shot callers cannot reuse content admitted during their search.
    #[allow(dead_code)] // Public library API; the CLI uses dependency-aware loading.
    pub fn open_uncached(root: &Path) -> Result<Self> {
        let mut reader = Self::open(root)?;
        reader.content_cache_enabled = false;
        Ok(reader)
    }

    pub fn open(root_path: &Path) -> Result<Self> {
        Self::open_with_tokens(root_path, true)
    }

    /// Internal one-shot search constructor. Only QueryExecutor should perform
    /// token operations on this reader, using its fallible dependency barrier.
    /// The public constructors retain eager validation of the complete index.
    #[allow(dead_code)] // Used by the CLI crate; intentionally not a public library API.
    pub(crate) fn open_for_search_uncached(root: &Path) -> Result<Self> {
        let mut reader = Self::open_with_tokens(root, false)?;
        reader.content_cache_enabled = false;
        Ok(reader)
    }

    pub(crate) fn ensure_tokens(&self) -> Result<()> {
        if self.segments.len() <= 4 {
            self.segments
                .iter()
                .try_for_each(SegmentReader::ensure_tokens)
        } else {
            self.segments
                .par_iter()
                .try_for_each(SegmentReader::ensure_tokens)
        }
    }

    fn open_with_tokens(root_path: &Path, load_tokens: bool) -> Result<Self> {
        let root_path = root_path.canonicalize()?;
        let (index_path, generation_lease) = crate::index::generation::pin(&root_path)?;

        if !index_path.exists() {
            anyhow::bail!("No index found. Run 'fxi index' first.");
        }

        // Read metadata first (needed for segment IDs)
        let meta_path = index_path.join("meta.json");
        let meta_file = File::open(&meta_path).context("Failed to open meta.json")?;
        let meta: IndexMeta = serde_json::from_reader(meta_file)?;
        anyhow::ensure!(
            matches!(meta.version, 1 | 2),
            "Unsupported index version {}; rebuild the index",
            meta.version
        );

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
            || read_documents_version(index_path_ref, meta.version),
            || {
                rayon::join(
                    || read_paths(index_path_ref),
                    || {
                        // Load all segments in parallel using par_iter
                        segment_ids
                            .par_iter()
                            .map(|&seg_id| {
                                let segment_path = index_path_ref
                                    .join("segments")
                                    .join(format!("seg_{:04}", seg_id));
                                SegmentReader::open(
                                    &segment_path,
                                    seg_id,
                                    meta.has_positions,
                                    load_tokens,
                                )
                                .with_context(|| {
                                    format!("Cannot open segment {seg_id}; rebuild the index")
                                })
                            })
                            .collect::<Result<Vec<_>>>()
                    },
                )
            },
        );

        let segments = segments?;
        let documents = documents_result?;
        let paths = paths_result?;

        let doc_id_to_index = DocumentLookup::new(&documents);

        // Convert stop-grams Vec to HashSet for O(1) lookup (was O(512) per check)
        let stop_grams: AHashSet<Trigram> = meta.stop_grams.iter().copied().collect();

        // Initialize file content cache
        let file_cache = SharedContentCache::acquire();

        Ok(Self {
            _generation_lease: generation_lease,
            root_path,
            index_path,
            meta,
            documents,
            doc_id_to_index,
            paths,
            segments,
            stop_grams,
            file_cache,
            content_cache_enabled: true,
            valid_docs_cache: OnceLock::new(),
            path_order_cache: OnceLock::new(),
        })
    }

    /// Get document by ID in constant time.
    pub fn get_document(&self, doc_id: DocId) -> Option<&Document> {
        self.doc_id_to_index
            .index(doc_id)
            .and_then(|idx| self.documents.get(idx))
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

    /// Get documents whose token dictionary has any token containing `needle`
    /// as a substring (queries all segments in parallel). Used as a recall
    /// fallback when trigram narrowing is unavailable (stop-grams).
    pub fn get_token_docs_containing(&self, needle: &str) -> RoaringBitmap {
        let needle_lower = needle.to_lowercase();
        if self.segments.len() <= 1 {
            self.segments
                .first()
                .map(|s| s.get_token_docs_containing(&needle_lower))
                .unwrap_or_default()
        } else {
            self.segments
                .par_iter()
                .map(|segment| segment.get_token_docs_containing(&needle_lower))
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
        let search = |segment: &SegmentReader| segment.intersect_trigrams(trigrams);
        if self.segments.len() <= 4 {
            self.segments
                .iter()
                .map(search)
                .fold(RoaringBitmap::new(), |mut a, b| {
                    a |= b;
                    a
                })
        } else {
            self.segments
                .par_iter()
                .map(search)
                .reduce(RoaringBitmap::new, |mut a, b| {
                    a |= b;
                    a
                })
        }
    }

    /// Dispatch once for the entire positive plan, rather than once per
    /// case variant per gram window. Keep intermediate bitmaps segment-local.
    pub(crate) fn get_gram_query_docs(&self, query: &GramQuery) -> RoaringBitmap {
        match query {
            GramQuery::All => return self.valid_doc_ids().clone(),
            GramQuery::Empty => return RoaringBitmap::new(),
            _ => {}
        }
        let search = |segment: &SegmentReader| query.in_segment(segment, self.valid_doc_ids());
        if self.segments.len() <= 4 {
            self.segments
                .iter()
                .map(search)
                .fold(RoaringBitmap::new(), |mut a, b| {
                    a |= b;
                    a
                })
        } else {
            self.segments
                .par_iter()
                .map(search)
                .reduce(RoaringBitmap::new, |mut a, b| {
                    a |= b;
                    a
                })
        }
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
        if self.segments.iter().any(|s| s.tokens().positions.is_none()) {
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
            .map(|segment| {
                self.resolve_phrase_in_segment(segment, phrase_tokens, &tokens_lower, candidates)
            })
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
            // Load positions for each phrase token in this segment:
            // per token, an optional list of (doc_id, positions)
            type TokenPositions = Vec<(u32, Vec<u32>)>;
            let mut all_positions: Vec<Option<TokenPositions>> = Vec::new();
            for token_lower in tokens_lower {
                all_positions.push(segment.get_token_positions(token_lower, candidates));
            }

            // If any token has no positions in this segment, skip it
            if all_positions.iter().any(|p| p.is_none()) {
                return result;
            }

            let positions: Vec<Vec<(u32, Vec<u32>)>> =
                all_positions.into_iter().map(|p| p.unwrap()).collect();

            // Merge-intersect by doc_id checking position gaps
            // Start with the first token's doc set
            let first_positions = &positions[0];

            for &(doc_id, ref first_pos) in first_positions {
                let mut found = false;

                // Check each position in the first token
                'outer: for &start_pos in first_pos {
                    // Check if all subsequent tokens have the expected position
                    let mut all_match = true;
                    for (tok_idx, (_, expected_offset)) in phrase_tokens.iter().enumerate().skip(1)
                    {
                        let expected_pos = start_pos + expected_offset - phrase_tokens[0].1;

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

    pub(crate) fn should_cache_path_order(&self, candidates: &RoaringBitmap) -> bool {
        self.content_cache_enabled
            && candidates.len() >= 256
            && candidates.len().saturating_mul(2) >= self.valid_doc_ids().len()
    }

    /// Order immutable document IDs without allocating candidate path strings.
    pub(crate) fn candidates_in_path_order(&self, candidates: &RoaringBitmap) -> Vec<DocId> {
        let sort_ids = |ids: &RoaringBitmap| {
            let mut paths: Vec<_> = ids
                .iter()
                .filter_map(|id| {
                    let doc = self.get_document(id)?;
                    Some((id, self.get_path(doc)?))
                })
                .collect();
            paths.sort_unstable_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)));
            paths.into_iter().map(|(id, _)| id).collect::<Vec<_>>()
        };
        if self.should_cache_path_order(candidates) {
            self.path_order_cache
                .get_or_init(|| sort_ids(self.valid_doc_ids()))
                .iter()
                .copied()
                .filter(|id| candidates.contains(*id))
                .collect()
        } else {
            sort_ids(candidates)
        }
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

    /// Identity of the immutable generation held by this reader's lease.
    pub(crate) fn generation_path(&self) -> &Path {
        &self.index_path
    }

    /// Get the root path
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Decide whether this scan may evict existing cache entries. Oversized
    /// scans may reuse hits and fill spare space, but must not churn the LRU.
    pub(crate) fn should_cache_scan(&self, candidates: &RoaringBitmap) -> bool {
        if !self.content_cache_enabled || self.file_cache.max_bytes == 0 {
            return false;
        }
        if candidates.len() > (FILE_CACHE_SHARDS * FILE_CACHE_SHARD_ENTRIES) as u64 {
            return false;
        }
        let mut bytes = 0u64;
        for id in candidates.iter() {
            if let Some(doc) = self.get_document(id) {
                // Oversized files are never admitted; they must not evict the
                // small-file scan from the policy's estimated cache budget.
                if doc.size > self.file_cache.max_entry_bytes as u64 {
                    continue;
                }
                bytes = bytes.saturating_add(doc.size);
                if bytes > self.file_cache.max_bytes as u64 {
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn read_file_for_scan(&self, path: &Path, cache_scan: bool) -> Option<FileContent> {
        self.read_file_with_cache_policy(path, cache_scan)
    }

    /// Read file content with LRU caching.
    /// This speeds up repeated queries that access the same files.
    /// The cache stores Arc<str>, so a hit is a refcount bump rather than a
    /// copy of the file content; files too large to cache are returned as
    /// plain Strings without the Arc conversion copy.
    /// Returns None if the file cannot be read.
    pub fn read_file_cached(&self, path: &Path) -> Option<FileContent> {
        self.read_file_with_cache_policy(path, true)
    }

    fn read_file_with_cache_policy(
        &self,
        path: &Path,
        allow_eviction: bool,
    ) -> Option<FileContent> {
        if !self.content_cache_enabled || self.file_cache.max_bytes == 0 {
            return Self::read_file_uncached(path).map(FileContent::Owned);
        }
        let shard = &self.file_cache.shards
            [self.file_cache.hasher.hash_one(path) as usize % FILE_CACHE_SHARDS];
        let stamp = FileStamp::from_metadata(&std::fs::metadata(path).ok()?);
        let may_admit = if let Ok(mut cache) = shard.lock() {
            if let Some((cached_stamp, content)) = cache.entries.get(path)
                && *cached_stamp == stamp
            {
                return Some(FileContent::Cached(Arc::clone(content)));
            }
            // A stale entry is not useful residency. Reclaim its budget even
            // when this scan is forbidden from evicting other live entries.
            if let Some((_, old)) = cache.entries.pop(path) {
                cache.bytes -= old.len();
            }
            stamp.size <= self.file_cache.max_entry_bytes as u64
                && (allow_eviction
                    || (stamp.size <= cache.max_bytes.saturating_sub(cache.bytes) as u64
                        && cache.entries.len() < cache.max_entries))
        } else {
            false
        };
        if !may_admit {
            return Self::read_file_uncached(path).map(FileContent::Owned);
        }

        let mut file = File::open(path).ok()?;
        let before = FileStamp::from_metadata(&file.metadata().ok()?);
        let mut content = String::new();
        file.read_to_string(&mut content).ok()?;
        let after = FileStamp::from_metadata(&file.metadata().ok()?);
        if content.len() <= self.file_cache.max_entry_bytes && before == after {
            let content: Arc<str> = content.into();
            if let Ok(mut cache) = shard.lock() {
                // Another worker may have filled the spare space while this
                // file was read. Recheck under the insertion lock.
                let previous = cache.entries.peek(path);
                let replacing = previous.is_some();
                let old_size = previous.map_or(0, |(_, old)| old.len());
                if allow_eviction
                    || (content.len() <= cache.max_bytes.saturating_sub(cache.bytes - old_size)
                        && (replacing || cache.entries.len() < cache.max_entries))
                {
                    cache.put(path.to_path_buf(), after, Arc::clone(&content));
                }
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

    /// Clear the shared process content cache.
    /// Call this after index updates to ensure stale content isn't served.
    #[allow(dead_code)]
    pub fn clear_file_cache(&self) {
        for shard in &self.file_cache.shards {
            if let Ok(mut cache) = shard.lock() {
                cache.entries.clear();
                cache.bytes = 0;
            }
        }
    }
}

/// Reject impossible on-disk counts before using them as allocation sizes.
fn validate_disk_count(
    file: &BufReader<File>,
    count: usize,
    header: u64,
    minimum_record: u64,
) -> Result<u64> {
    let bytes = file.get_ref().metadata()?.len().saturating_sub(header);
    anyhow::ensure!(
        count as u64 <= bytes / minimum_record,
        "Index count exceeds file bounds"
    );
    Ok(bytes)
}

/// Read documents from an immutable index generation.
pub fn read_documents(index_path: &Path) -> Result<Vec<Document>> {
    let meta: IndexMeta = serde_json::from_reader(File::open(index_path.join("meta.json"))?)?;
    read_documents_version(index_path, meta.version)
}

fn read_documents_version(index_path: &Path, version: u32) -> Result<Vec<Document>> {
    let data = MappedBytes::open(&index_path.join("docs.bin"))?;
    anyhow::ensure!(data.len() >= 4, "Truncated document header");
    let count = le32(&data) as usize;
    anyhow::ensure!(
        count <= (data.len() - 4) / 30,
        "Index count exceeds file bounds"
    );
    Ok(data[4..]
        .as_chunks::<30>()
        .0
        .iter()
        .take(count)
        .map(|record| {
            let raw_mtime = le64(&record[16..]);
            // Legacy generations contain both second and nanosecond timestamps.
            let mtime = if version == 1 && raw_mtime < 1_000_000_000_000 {
                raw_mtime.saturating_mul(1_000_000_000)
            } else {
                raw_mtime
            };
            Document {
                doc_id: le32(record),
                path_id: le32(&record[4..]),
                size: le64(&record[8..]),
                mtime,
                language: Language::try_from(u16::from_le_bytes([record[24], record[25]]))
                    .unwrap_or(Language::Unknown),
                flags: DocFlags(u16::from_le_bytes([record[26], record[27]])),
                segment_id: u16::from_le_bytes([record[28], record[29]]),
            }
        })
        .collect())
}

/// Read paths with one owned allocation per path, after validating bounds in
/// the immutable mapping. No temporary per-path byte buffers are required.
pub fn read_paths(index_path: &Path) -> Result<Vec<PathBuf>> {
    let data = MappedBytes::open(&index_path.join("paths.bin"))?;
    anyhow::ensure!(data.len() >= 4, "Truncated path header");
    let count = le32(&data) as usize;
    anyhow::ensure!(
        count <= (data.len() - 4) / 4,
        "Index count exceeds file bounds"
    );
    let mut paths = Vec::with_capacity(count);
    let mut cursor = 4;
    for _ in 0..count {
        let remaining = &data[cursor..];
        anyhow::ensure!(remaining.len() >= 4, "Truncated path record");
        let len = le32(remaining) as usize;
        let bytes = remaining[4..]
            .get(..len)
            .context("Path length exceeds file bounds")?;
        paths.push(PathBuf::from(String::from_utf8_lossy(bytes).into_owned()));
        cursor += 4 + len;
    }
    Ok(paths)
}

/// Validate lengths before accessing records in immutable mappings.
fn read_trigram_dict(segment_path: &Path) -> Result<TrigramDict> {
    let data = MappedBytes::open(&segment_path.join("grams.dict"))?;
    anyhow::ensure!(data.len() >= 4, "Truncated trigram dictionary header");
    let count = le32(&data) as usize;
    anyhow::ensure!(
        count <= (data.len() - 4) / 20 && data.len() - 4 == count * 20,
        "Trigram dictionary count exceeds file bounds"
    );
    Ok(TrigramDict { data, count })
}

fn read_token_dict(
    segment_path: &Path,
    posting_size: usize,
    position_size: Option<usize>,
) -> Result<TokenDict> {
    let has_positions = position_size.is_some();
    let data = MappedBytes::open(&segment_path.join("tokens.dict"))?;
    anyhow::ensure!(data.len() >= 4, "Truncated token dictionary header");
    let count = le32(&data) as usize;
    let fixed_size = if has_positions { 30 } else { 18 };
    anyhow::ensure!(
        count <= (data.len() - 4) / fixed_size,
        "Token dictionary count exceeds file bounds"
    );
    let mut offsets = Vec::with_capacity(count);
    let mut cursor = 4;
    let mut previous_token = None;
    for _ in 0..count {
        anyhow::ensure!(
            data.len() - cursor >= fixed_size,
            "Truncated token dictionary record"
        );
        let len = u16::from_le_bytes(data[cursor..cursor + 2].try_into().unwrap()) as usize;
        anyhow::ensure!(
            len <= data.len() - cursor - fixed_size,
            "Token length exceeds file bounds"
        );
        let token = std::str::from_utf8(&data[cursor + 2..cursor + 2 + len])
            .context("Invalid token UTF-8")?;
        anyhow::ensure!(
            previous_token.is_none_or(|previous| previous < token),
            "Unsorted token dictionary"
        );
        previous_token = Some(token);
        let fields = &data[cursor + 2 + len..cursor + fixed_size + len];
        anyhow::ensure!(
            posting_range_fits(le64(fields), le32(&fields[8..]), posting_size),
            "Truncated token postings"
        );
        if let Some(size) = position_size {
            anyhow::ensure!(
                posting_range_fits(le64(&fields[16..]), le32(&fields[24..]), size),
                "Truncated token positions"
            );
        }
        offsets.push(cursor);
        cursor += fixed_size + len;
    }
    anyhow::ensure!(cursor == data.len(), "Trailing token dictionary bytes");
    Ok(TokenDict {
        data,
        offsets,
        has_positions,
    })
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

    let mut remaining = validate_disk_count(&file, count, 4, 12)?;
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

        remaining = remaining
            .checked_sub(12)
            .context("Truncated line map record")?;
        anyhow::ensure!(
            encoded_len as u64 <= remaining,
            "Line map length exceeds file bounds"
        );
        remaining -= encoded_len as u64;
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
    let mut file = BufReader::new(File::open(segment_path.join("bloom.bin"))?);
    let mut magic = [0u8; 6];
    file.read_exact(&mut magic)?;
    anyhow::ensure!(
        &magic == crate::utils::bloom::BLOOM_MAGIC,
        "Unrecognized bloom hash format; optional filter disabled"
    );
    let mut probe_count = [0u8; 1];
    let mut count = [0u8; 4];
    file.read_exact(&mut probe_count)?;
    file.read_exact(&mut count)?;
    let num_hashes = probe_count[0];
    let num_words = u32::from_le_bytes(count) as usize;
    anyhow::ensure!((1..=16).contains(&num_hashes), "Invalid bloom probe count");
    anyhow::ensure!(num_words > 0, "Empty bloom filter");
    // Six-byte magic, probe count, word count, and checksum trailer.
    let remaining = validate_disk_count(&file, num_words, 19, 8)?;
    anyhow::ensure!(
        remaining == num_words as u64 * 8,
        "Invalid bloom file length"
    );
    let mut bits = Vec::with_capacity(num_words);
    let mut word = [0u8; 8];
    for _ in 0..num_words {
        file.read_exact(&mut word)?;
        bits.push(u64::from_le_bytes(word));
    }
    file.read_exact(&mut word)?;
    let filter = BloomFilter::from_raw(bits, num_hashes);
    anyhow::ensure!(
        filter.checksum() == u64::from_le_bytes(word),
        "Bloom checksum mismatch"
    );
    Ok(filter)
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
    fn internal_search_readers_load_complete_token_data_on_demand() {
        let (_temp, root) = create_test_index();
        let full = IndexReader::open(&root).unwrap();
        let expected_docs = full.get_token_docs("main");
        let phrase = vec![("fn".into(), 0), ("main".into(), 1)];
        let expected_positions = full.resolve_phrase_positional(&phrase, None);
        drop(full);
        let core = IndexReader::open_for_search_uncached(&root).unwrap();
        assert!(
            core.segments
                .iter()
                .all(|segment| segment.tokens.get().is_none())
        );
        let query = crate::query::parse_query("re:/main/");
        assert_eq!(
            crate::query::QueryExecutor::new(&core)
                .execute_files_only(&query, 0)
                .unwrap(),
            vec![PathBuf::from("test.rs")]
        );
        assert!(
            core.segments
                .iter()
                .all(|segment| segment.tokens.get().is_none())
        );
        (0..16)
            .into_par_iter()
            .for_each(|_| core.ensure_tokens().unwrap());
        assert!(
            core.segments
                .iter()
                .all(|segment| segment.tokens.get().unwrap().is_ok())
        );
        assert_eq!(core.get_token_docs("main"), expected_docs);
        assert_eq!(
            core.resolve_phrase_positional(&phrase, None),
            expected_positions
        );
        drop(core);
        crate::utils::remove_index(&root).unwrap();
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
    fn mapped_metadata_decoding_checks_every_truncation_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut docs = 1u32.to_le_bytes().to_vec();
        docs.extend_from_slice(&123u32.to_le_bytes());
        docs.extend_from_slice(&456u32.to_le_bytes());
        docs.extend_from_slice(&u64::MAX.to_le_bytes());
        docs.extend_from_slice(&123u64.to_le_bytes());
        docs.extend_from_slice(&u16::MAX.to_le_bytes());
        docs.extend_from_slice(&0xa5a5u16.to_le_bytes());
        docs.extend_from_slice(&u16::MAX.to_le_bytes());
        for len in 0..docs.len() {
            fs::write(dir.path().join("docs.bin"), &docs[..len]).unwrap();
            assert!(
                read_documents_version(dir.path(), 2).is_err(),
                "doc length {len}"
            );
        }
        fs::write(dir.path().join("docs.bin"), &docs).unwrap();
        let decoded = read_documents_version(dir.path(), 2).unwrap();
        let doc = &decoded[0];
        assert_eq!(
            (doc.doc_id, doc.path_id, doc.size, doc.mtime),
            (123, 456, u64::MAX, 123)
        );
        assert_eq!(doc.language, Language::Unknown);
        assert_eq!(doc.flags.0, 0xa5a5);
        assert_eq!(doc.segment_id, u16::MAX);
        assert_eq!(
            read_documents_version(dir.path(), 1).unwrap()[0].mtime,
            123_000_000_000
        );
        let names: [&[u8]; 3] = [b"", "space/K.rs".as_bytes(), b"invalid\xff"];
        let mut paths = (names.len() as u32).to_le_bytes().to_vec();
        for name in names {
            paths.extend_from_slice(&(name.len() as u32).to_le_bytes());
            paths.extend_from_slice(name);
        }
        for len in 0..paths.len() {
            fs::write(dir.path().join("paths.bin"), &paths[..len]).unwrap();
            assert!(read_paths(dir.path()).is_err(), "path length {len}");
        }
        fs::write(dir.path().join("paths.bin"), &paths).unwrap();
        assert_eq!(
            read_paths(dir.path()).unwrap(),
            names.map(|name| PathBuf::from(String::from_utf8_lossy(name).as_ref()))
        );
    }

    #[test]
    fn document_lookup_handles_contiguous_sparse_and_extreme_ids() {
        for ids in [
            vec![],
            vec![0],
            vec![1, 2, 3],
            vec![u32::MAX],
            vec![u32::MAX - 1, u32::MAX],
            vec![1, 3, u32::MAX],
            vec![2, 1],
            vec![1, 1],
        ] {
            let documents: Vec<_> = ids
                .iter()
                .map(|&id| Document {
                    doc_id: id,
                    path_id: 0,
                    size: 0,
                    mtime: 0,
                    language: Language::Unknown,
                    flags: DocFlags::new(),
                    segment_id: 0,
                })
                .collect();
            let lookup = DocumentLookup::new(&documents);
            let reference: HashMap<_, _> = ids
                .iter()
                .enumerate()
                .map(|(index, &id)| (id, index))
                .collect();
            for id in [0, 1, 2, 3, 4, u32::MAX - 1, u32::MAX] {
                assert_eq!(
                    lookup.index(id).filter(|&index| index < documents.len()),
                    reference.get(&id).copied(),
                    "{ids:?}, {id}"
                );
            }
        }
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
    fn noncacheable_files_do_not_consume_scan_admission_budget() {
        let (_temp_dir, root) = create_test_index();
        let mut reader = IndexReader::open(&root).unwrap();
        reader.documents[0].size = (FILE_CACHE_SHARDS * FILE_CACHE_SHARD_BYTES + 1) as u64;
        assert!(reader.should_cache_scan(reader.valid_doc_ids()));
    }

    #[test]
    fn scan_resistant_reads_preserve_snapshot_and_utf8_semantics() {
        let (_temp_dir, root) = create_test_index();
        let reader = IndexReader::open(&root).unwrap();
        let path = root.join("test.rs");
        let first = reader.read_file_for_scan(&path, false).unwrap();
        assert!(matches!(first, FileContent::Cached(_)));
        let cached = reader.read_file_for_scan(&path, true).unwrap();
        assert_eq!(&*first, &*cached);
        fs::write(&path, "updated needle").unwrap();
        for use_cache in [false, true] {
            assert_eq!(
                &*reader.read_file_for_scan(&path, use_cache).unwrap(),
                "updated needle"
            );
        }
        assert_eq!(&*first, &*cached); // old owned/shared snapshots remain valid
        fs::write(&path, [0xff]).unwrap();
        for use_cache in [false, true] {
            assert!(reader.read_file_for_scan(&path, use_cache).is_none());
        }
        fs::remove_file(&path).unwrap();
        for use_cache in [false, true] {
            assert!(reader.read_file_for_scan(&path, use_cache).is_none());
        }
    }

    #[test]
    fn oversized_scans_fill_spare_capacity_without_eviction() {
        let (_temp, root) = create_test_index();
        let mut reader = IndexReader::open(&root).unwrap();
        reader.file_cache = Arc::new(SharedContentCache {
            shards: std::array::from_fn(|_| {
                let mut cache = ContentCache::new();
                cache.max_bytes = 16;
                cache.max_entries = 2;
                Mutex::new(cache)
            }),
            hasher: ahash::RandomState::with_seeds(1, 2, 3, 4),
            max_bytes: 16 * FILE_CACHE_SHARDS,
            max_entry_bytes: 16,
        });
        let paths: Vec<_> = (0..1024)
            .map(|i| root.join(format!("cache-{i}.txt")))
            .filter(|path| {
                (reader.file_cache.hasher.hash_one(path) as usize).is_multiple_of(FILE_CACHE_SHARDS)
            })
            .take(10)
            .collect();
        assert_eq!(paths.len(), 10);
        for path in &paths {
            fs::write(path, "12345678").unwrap();
        }
        let first = reader.read_file_for_scan(&paths[0], false).unwrap();
        reader.read_file_for_scan(&paths[1], false).unwrap();
        assert!(matches!(
            reader.read_file_for_scan(&paths[2], false),
            Some(FileContent::Owned(_))
        ));
        let again = reader.read_file_for_scan(&paths[0], false).unwrap();
        match (&first, &again) {
            (FileContent::Cached(a), FileContent::Cached(b)) => assert!(Arc::ptr_eq(a, b)),
            _ => panic!("existing residency must survive an oversized scan"),
        }
        fs::write(&paths[0], "changed!").unwrap();
        assert_eq!(
            &*reader.read_file_for_scan(&paths[0], false).unwrap(),
            "changed!"
        );
        assert_eq!(&*first, "12345678");
        // Growing beyond the entry cap releases stale residency; another file
        // can fill the freed space without evicting the untouched second file.
        fs::write(&paths[0], "x".repeat(32)).unwrap();
        assert_eq!(
            reader.read_file_for_scan(&paths[0], false).unwrap().len(),
            32
        );
        reader.read_file_for_scan(&paths[2], false).unwrap();
        let cache = reader.file_cache.shards[0].lock().unwrap();
        assert!(!cache.entries.contains(&paths[0]));
        assert!(cache.entries.contains(&paths[1]));
        assert!(cache.entries.contains(&paths[2]));
        assert_eq!(cache.bytes, 16);
        assert_eq!(cache.entries.len(), 2);
        drop(cache);
        reader.clear_file_cache();
        let barrier = std::sync::Barrier::new(paths.len());
        std::thread::scope(|scope| {
            for path in &paths {
                let reader = &reader;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    assert!(reader.read_file_for_scan(path, false).is_some());
                });
            }
        });
        let cache = reader.file_cache.shards[0].lock().unwrap();
        assert_eq!(cache.bytes, 16);
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn readers_share_one_budget_but_single_queries_do_not_admit_content() {
        let (_temp_a, root_a) = create_test_index();
        let (_temp_b, root_b) = create_test_index();
        let a = IndexReader::open(&root_a).unwrap();
        let b = IndexReader::open(&root_b).unwrap();
        assert!(Arc::ptr_eq(&a.file_cache, &b.file_cache));
        let path = root_a.join("test.rs");
        let once = IndexReader::open_uncached(&root_a).unwrap();
        assert!(!once.should_cache_scan(once.valid_doc_ids()));
        assert!(matches!(
            once.read_file_cached(&path),
            Some(FileContent::Owned(_))
        ));
        assert!(
            a.file_cache
                .shards
                .iter()
                .all(|shard| !shard.lock().unwrap().entries.contains(&path))
        );
        assert!(matches!(
            a.read_file_cached(&path),
            Some(FileContent::Cached(_))
        ));
        assert!(matches!(
            b.read_file_cached(&path),
            Some(FileContent::Cached(_))
        ));
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

#[cfg(test)]
mod cache_budget_tests {
    use super::*;

    #[test]
    fn cache_accounts_for_replacement_and_byte_eviction() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let stamp = FileStamp::from_metadata(&file.as_file().metadata().unwrap());
        let mut cache = ContentCache::new();
        cache.max_bytes = 4 * 1024 * 1024;
        cache.max_entries = 256;
        for i in 0..64 {
            cache.put(
                format!("f{i}").into(),
                stamp.clone(),
                Arc::from("x".repeat(128 * 1024)),
            );
            assert!(cache.bytes <= cache.max_bytes);
        }
        assert!(!cache.entries.contains(Path::new("f0")));
        cache.put("f63".into(), stamp.clone(), Arc::from("small"));
        assert_eq!(
            cache.bytes,
            cache
                .entries
                .iter()
                .map(|(_, (_, text))| text.len())
                .sum::<usize>()
        );
        for i in 0..cache.max_entries + 1 {
            cache.put(format!("small{i}").into(), stamp.clone(), Arc::from("x"));
        }
        assert_eq!(cache.entries.len(), cache.max_entries);
        assert_eq!(cache.bytes, cache.max_entries);
    }
}
