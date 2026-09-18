pub use crate::index::source_positions::SourceSnapshot;
use crate::index::types::*;
use crate::utils::{BloomFilter, delta_decode, delta_decode_bitmap, delta_decode_intersect};
use ahash::{AHashMap, AHashSet};
use anyhow::{Context, Result};
use lru::LruCache;
use memmap2::Mmap;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Empty posting files are valid, but cannot be memory mapped on every OS.
pub(crate) enum MappedBytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}
impl MappedBytes {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        Ok(if file.metadata()?.len() == 0 {
            Self::Owned(Vec::new())
        } else {
            Self::Mapped(unsafe { Mmap::map(&file)? })
        })
    }
}
impl std::ops::Deref for MappedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
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
    compact: bool,
}

impl TokenDict {
    fn entry(&self, index: usize) -> TokenDictEntry<'_> {
        let (entry, _) = super::token_dictionary::entry(
            &self.data[self.offsets[index]..],
            self.compact,
            self.has_positions,
        )
        .expect("validated immutable token dictionary");
        TokenDictEntry {
            token: entry.token,
            offset: entry.offset,
            length: entry.length,
            pos_offset: entry.pos_offset,
            pos_length: entry.pos_length,
        }
    }

    fn lookup(&self, token: &str) -> Option<TokenDictEntry<'_>> {
        let mut lo = 0;
        let mut hi = self.offsets.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (candidate, _) = super::token_dictionary::token(&self.data[self.offsets[mid]..])
                .expect("validated immutable token dictionary");
            match candidate.cmp(token) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.entry(mid)),
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
    fn open(
        segment_path: &Path,
        required_positions: bool,
        allowed_docs: &RoaringBitmap,
    ) -> Result<Self> {
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
        let dictionary =
            read_token_dict(segment_path, &postings, positions.as_deref(), allowed_docs)?;
        Ok(Self {
            dictionary,
            postings,
            positions,
        })
    }
}

/// Reader for a single segment
struct SegmentReader {
    source_pack: OnceLock<Option<crate::index::source_pack::SourcePack>>,
    #[allow(dead_code)]
    segment_id: SegmentId,
    trigram_dict: TrigramDict,
    trigram_postings: MappedBytes,
    tokens: OnceLock<std::result::Result<TokenIndex, String>>,
    required_positions: bool,
    allowed_docs: Option<Arc<RoaringBitmap>>,
    /// Lazily loaded line maps - only loaded when first accessed
    line_maps: OnceLock<std::result::Result<HashMap<DocId, Vec<u32>>, String>>,
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
        allowed_docs: Arc<RoaringBitmap>,
    ) -> Result<Self> {
        // Read trigram dictionary (already sorted from BTreeMap write)
        let trigram_dict = read_trigram_dict(segment_path)?;

        let trigram_postings = MappedBytes::open(&segment_path.join("grams.postings"))?;

        // Validate every payload before exposing the segment. Large segments
        // use the existing Rayon pool so a single base segment does not serialize
        // validation; small segments avoid scheduling overhead.
        let validator = crate::utils::encoding::DocumentPostingsValidator::new(&allowed_docs);
        let validate_entry = |index: usize| -> Result<()> {
            let entry = trigram_dict.entry(index);
            anyhow::ensure!(
                posting_range_fits(entry.offset, entry.length, trigram_postings.len()),
                "Truncated trigram postings"
            );
            anyhow::ensure!(
                index == 0 || trigram_dict.entry(index - 1).trigram < entry.trigram,
                "Unsorted trigram dictionary"
            );
            let bytes = &trigram_postings
                [entry.offset as usize..entry.offset as usize + entry.length as usize];
            validator.validate(bytes, entry.doc_freq)?;
            Ok(())
        };
        if trigram_postings.len() >= 1024 * 1024 && trigram_dict.count >= 512 {
            (0..trigram_dict.count)
                .into_par_iter()
                .try_for_each(validate_entry)?;
        } else {
            (0..trigram_dict.count).try_for_each(validate_entry)?;
        }
        // Line maps are NOT loaded here - loaded lazily on first access

        // Load bloom filter if it exists (optional for backwards compat)
        let bloom_filter = read_bloom_filter(segment_path).ok();

        let reader = Self {
            source_pack: OnceLock::new(),
            segment_id,
            trigram_dict,
            trigram_postings,
            tokens: OnceLock::new(),
            required_positions,
            allowed_docs: Some(allowed_docs),
            line_maps: OnceLock::new(),
            segment_path: segment_path.to_path_buf(),
            bloom_filter,
        };
        if load_tokens {
            reader.ensure_tokens()?;
        }
        Ok(reader)
    }

    /// Encode the ordinary immutable segment representation into owned bytes.
    /// Reusing the existing decoders keeps memory and disk query semantics equal.
    fn from_memory(
        segment_id: SegmentId,
        files: Vec<(DocId, crate::index::build::ProcessedFile)>,
    ) -> Result<Self> {
        fn checked_len(len: usize) -> Result<u32> {
            u32::try_from(len).context("Memory-delta posting capacity exhausted")
        }
        let mut grams: BTreeMap<Trigram, Vec<DocId>> = BTreeMap::new();
        let mut tokens: BTreeMap<String, Vec<DocId>> = BTreeMap::new();
        let mut positions: BTreeMap<String, BTreeMap<DocId, Vec<u32>>> = BTreeMap::new();
        let mut line_maps = HashMap::with_capacity(files.len());
        for (doc_id, file) in files {
            for gram in file.trigrams {
                grams.entry(gram).or_default().push(doc_id);
            }
            for (token, position) in file.token_positions {
                let token = file
                    .tokens
                    .get(token as usize)
                    .context("Memory-delta position references a missing token")?;
                positions
                    .entry(token.clone())
                    .or_default()
                    .entry(doc_id)
                    .or_default()
                    .push(position);
            }
            for token in file.tokens {
                tokens.entry(token).or_default().push(doc_id);
            }
            line_maps.insert(doc_id, file.line_offsets);
        }

        let gram_count = checked_len(grams.len())?;
        let mut gram_dictionary = gram_count.to_le_bytes().to_vec();
        let mut gram_postings = Vec::new();
        let mut bloom = BloomFilter::new(grams.len().max(1), 0.01);
        for (gram, mut docs) in grams {
            docs.sort_unstable();
            docs.dedup();
            let offset = gram_postings.len();
            crate::utils::delta_encode(&docs, &mut gram_postings);
            gram_dictionary.extend_from_slice(&gram.to_le_bytes());
            gram_dictionary.extend_from_slice(&(offset as u64).to_le_bytes());
            gram_dictionary
                .extend_from_slice(&checked_len(gram_postings.len() - offset)?.to_le_bytes());
            gram_dictionary.extend_from_slice(&checked_len(docs.len())?.to_le_bytes());
            bloom.insert(gram);
        }
        let mut token_dictionary = checked_len(tokens.len())?.to_le_bytes().to_vec();
        let mut token_offsets = Vec::with_capacity(tokens.len());
        let mut token_postings = Vec::new();
        let mut token_positions = Vec::new();
        for (token, mut docs) in tokens {
            docs.sort_unstable();
            docs.dedup();
            let postings_offset = token_postings.len();
            crate::utils::delta_encode(&docs, &mut token_postings);
            let positions_offset = token_positions.len();
            if let Some(doc_positions) = positions.get_mut(&token) {
                for positions in doc_positions.values_mut() {
                    positions.sort_unstable();
                    positions.dedup();
                    checked_len(positions.len())?;
                }
                let references: Vec<_> = doc_positions
                    .iter()
                    .map(|(&id, positions)| (id, positions.as_slice()))
                    .collect();
                crate::utils::encode_position_postings(&references, &mut token_positions);
            }
            token_offsets.push(token_dictionary.len());
            token_dictionary.extend_from_slice(
                &u16::try_from(token.len())
                    .context("Memory-delta token exceeds format limit")?
                    .to_le_bytes(),
            );
            token_dictionary.extend_from_slice(token.as_bytes());
            token_dictionary.extend_from_slice(&(postings_offset as u64).to_le_bytes());
            token_dictionary.extend_from_slice(
                &checked_len(token_postings.len() - postings_offset)?.to_le_bytes(),
            );
            token_dictionary.extend_from_slice(&checked_len(docs.len())?.to_le_bytes());
            token_dictionary.extend_from_slice(&(positions_offset as u64).to_le_bytes());
            token_dictionary.extend_from_slice(
                &checked_len(token_positions.len() - positions_offset)?.to_le_bytes(),
            );
        }
        Ok(Self {
            source_pack: OnceLock::from(None),
            segment_id,
            trigram_dict: TrigramDict {
                data: MappedBytes::Owned(gram_dictionary),
                count: gram_count as usize,
            },
            trigram_postings: MappedBytes::Owned(gram_postings),
            tokens: OnceLock::from(Ok(TokenIndex {
                dictionary: TokenDict {
                    data: MappedBytes::Owned(token_dictionary),
                    offsets: token_offsets,
                    has_positions: true,
                    compact: false,
                },
                postings: MappedBytes::Owned(token_postings),
                positions: Some(MappedBytes::Owned(token_positions)),
            })),
            required_positions: true,
            allowed_docs: None,
            line_maps: OnceLock::from(Ok(line_maps)),
            // All lazy cells are populated; this path is never opened.
            segment_path: PathBuf::new(),
            bloom_filter: Some(bloom),
        })
    }

    fn ensure_tokens(&self) -> Result<()> {
        self.tokens
            .get_or_init(|| {
                TokenIndex::open(
                    &self.segment_path,
                    self.required_positions,
                    self.allowed_docs
                        .as_deref()
                        .expect("disk segment document IDs"),
                )
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
        let dictionary = &self.tokens().dictionary;
        for index in 0..dictionary.offsets.len() {
            let (token, _) =
                super::token_dictionary::token(&dictionary.data[dictionary.offsets[index]..])
                    .expect("validated immutable token dictionary");
            if token.len() >= needle.len() && finder.find(token.as_bytes()).is_some() {
                let entry = dictionary.entry(index);
                debug_assert_eq!(entry.token, token);
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

    /// Get a line map, preserving lazy validation failures for every caller.
    fn get_line_map(&self, doc_id: DocId) -> Result<Option<&Vec<u32>>> {
        let line_maps = self.line_maps.get_or_init(|| {
            (|| -> Result<_> {
                let maps = read_line_maps(&self.segment_path)?;
                anyhow::ensure!(
                    self.allowed_docs
                        .as_ref()
                        .is_none_or(|allowed| maps.keys().all(|id| allowed.contains(*id))),
                    "Line map references an unknown segment document"
                );
                Ok(maps)
            })()
            .map_err(|error| format!("{error:#}"))
        });
        match line_maps {
            Ok(maps) => Ok(maps.get(&doc_id)),
            Err(error) => anyhow::bail!("{error}"),
        }
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
    entries: LruCache<PathBuf, (FileStamp, Arc<SourceSnapshot>)>,
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

    fn put(&mut self, path: PathBuf, stamp: FileStamp, content: Arc<SourceSnapshot>) {
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
    Cached(Arc<SourceSnapshot>),
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

/// The durable path table is immutable and shared by its memory snapshots.
/// Only paths first introduced by a memory delta need cloning on the next one.
#[derive(Clone)]
struct PathTable {
    base: Arc<Vec<PathBuf>>,
    base_lookup: Arc<OnceLock<BasePathLookup>>,
    appended: Vec<PathBuf>,
}

struct BasePathLookup {
    /// Equal legacy paths use their last path-table ID, matching disk writers.
    ids: AHashMap<PathBuf, PathId>,
    aliased_ids: AHashSet<PathId>,
}

impl PathTable {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            base: Arc::new(paths),
            base_lookup: Arc::new(OnceLock::new()),
            appended: Vec::new(),
        }
    }

    #[inline]
    fn get(&self, index: usize) -> Option<&PathBuf> {
        if index < self.base.len() {
            self.base.get(index)
        } else {
            self.appended.get(index - self.base.len())
        }
    }

    fn len(&self) -> usize {
        self.base.len() + self.appended.len()
    }

    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.base.iter().chain(&self.appended)
    }

    fn push(&mut self, path: PathBuf) {
        self.appended.push(path);
    }

    fn prepared_lookup(&self) -> &BasePathLookup {
        self.base_lookup.get_or_init(|| {
            let mut ids = AHashMap::with_capacity(self.base.len());
            let mut aliased_ids = AHashSet::new();
            for (index, path) in self.base.iter().enumerate() {
                // Disk path counts are validated u32 values when loaded.
                let id = index as PathId;
                if let Some(previous) = ids.insert(path.clone(), id) {
                    aliased_ids.remove(&previous);
                    aliased_ids.insert(id);
                }
            }
            BasePathLookup { ids, aliased_ids }
        })
    }

    /// Return the canonical path ID and whether older IDs name the same path.
    fn find(&self, path: &Path) -> Option<(PathId, bool)> {
        let base = self.prepared_lookup();
        let mut appended = self
            .appended
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, candidate)| candidate.as_path() == path);
        if let Some((index, _)) = appended.next() {
            let id = (self.base.len() + index) as PathId;
            return Some((id, appended.next().is_some() || base.ids.contains_key(path)));
        }
        base.ids
            .get(path)
            .map(|&id| (id, base.aliased_ids.contains(&id)))
    }
}

/// Memory-mapped index reader for fast queries
pub struct IndexReader {
    _generation_lease: Option<Arc<File>>,
    root_path: PathBuf,
    #[allow(dead_code)]
    index_path: PathBuf,
    pub meta: IndexMeta,
    /// Documents stored in on-disk order for iteration.
    documents: Vec<Document>,
    /// O(1) lookup index: doc_id -> index in documents Vec
    doc_id_to_index: DocumentLookup,
    paths: PathTable,
    segments: Vec<Arc<SegmentReader>>,
    /// O(1) stop-gram lookup (converted from Vec on load)
    stop_grams: AHashSet<Trigram>,
    /// LRU cache for file contents (speeds up repeated queries on same files)
    file_cache: Arc<SharedContentCache>,
    content_cache_enabled: bool,
    source_pack_enabled: bool,
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

    /// Internal search constructor. Only QueryExecutor should perform
    /// token operations on this reader, using its fallible dependency barrier.
    /// Gram and document data are validated immediately; token data are
    /// validated once, before a dependent query can use them.
    pub(crate) fn open_for_search(root: &Path) -> Result<Self> {
        Self::open_with_tokens(root, false)
    }

    /// A one-shot reader cannot reuse content admitted during its search.
    #[allow(dead_code)] // Used by the CLI crate; intentionally not a public library API.
    pub(crate) fn open_for_search_uncached(root: &Path) -> Result<Self> {
        let mut reader = Self::open_for_search(root)?;
        reader.content_cache_enabled = false;
        Ok(reader)
    }

    pub(crate) fn ensure_tokens(&self) -> Result<()> {
        anyhow::ensure!(
            self.meta.profile == IndexProfile::Full,
            "Token evidence is unavailable in a lean index; rebuild with --profile full"
        );
        if self.segments.len() <= 4 {
            self.segments
                .iter()
                .try_for_each(|segment| segment.ensure_tokens())
        } else {
            self.segments
                .par_iter()
                .try_for_each(|segment| segment.ensure_tokens())
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
        meta.validate_format()?;

        // Collect all segment IDs to load
        let mut segment_ids: Vec<SegmentId> = Vec::new();
        if let Some(base_id) = meta.base_segment {
            segment_ids.push(base_id);
        }
        segment_ids.extend(&meta.delta_segments);

        // Document membership is required to validate segment payloads. Read
        // metadata in parallel, then validate independent segments in parallel.
        let (documents, paths) = rayon::join(
            || read_documents_version(&index_path, meta.version),
            || read_paths(&index_path),
        );
        let documents = documents?;
        let paths = PathTable::new(paths?);
        validate_document_references(&meta, &documents, paths.len())?;
        let allowed = segment_document_ids(&documents);
        let segments = segment_ids
            .par_iter()
            .map(|&seg_id| {
                let path = index_path.join("segments").join(format!("seg_{seg_id:04}"));
                SegmentReader::open(
                    &path,
                    seg_id,
                    meta.has_positions,
                    load_tokens && meta.profile == IndexProfile::Full,
                    allowed.get(&seg_id).cloned().unwrap_or_default(),
                )
                .map(Arc::new)
                .with_context(|| format!("Cannot open segment {seg_id}; rebuild the index"))
            })
            .collect::<Result<Vec<_>>>()?;

        let doc_id_to_index = DocumentLookup::new(&documents);

        // Convert stop-grams Vec to HashSet for O(1) lookup (was O(512) per check)
        let stop_grams: AHashSet<Trigram> = meta.stop_grams.iter().copied().collect();

        // Initialize file content cache
        let file_cache = SharedContentCache::acquire();

        Ok(Self {
            _generation_lease: generation_lease.map(Arc::new),
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
            source_pack_enabled: cfg!(unix)
                && std::env::var_os("FXI_SOURCE_PACK").is_none_or(|v| v != "0"),
            valid_docs_cache: OnceLock::new(),
            path_order_cache: OnceLock::new(),
        })
    }

    /// Derive a query-ready immutable snapshot without publishing index files.
    /// Every replacement gets complete postings and positions, so the ordinary
    /// planner, filters, exclusions and verifiers retain their normal semantics.
    /// The caller must retain a separate durable reader for disk reconciliation:
    /// this snapshot shares its generation identity but includes memory-only IDs.
    pub(crate) fn with_memory_delta(
        &self,
        files: Vec<crate::index::build::ProcessedFile>,
        removed: &[PathBuf],
    ) -> Result<Self> {
        fn valid_relative(path: &Path) -> bool {
            !path.as_os_str().is_empty()
                && path.to_str().is_some()
                && path
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
        }
        let mut replacements = HashSet::with_capacity(files.len() + removed.len());
        for file in &files {
            anyhow::ensure!(
                valid_relative(&file.rel_path),
                "Invalid memory-delta file path"
            );
            anyhow::ensure!(
                replacements.insert(file.rel_path.clone()),
                "Duplicate memory-delta file path"
            );
            anyhow::ensure!(
                !file.flags.is_stale() && !file.flags.is_tombstone(),
                "Invalid memory-delta document flags"
            );
            anyhow::ensure!(
                file.tokens
                    .iter()
                    .all(|token| u16::try_from(token.len()).is_ok()),
                "Memory-delta token exceeds format limit"
            );
            anyhow::ensure!(
                file.token_positions
                    .iter()
                    .all(|&(token, _)| (token as usize) < file.tokens.len()),
                "Memory-delta position references a missing token"
            );
        }
        for path in removed {
            anyhow::ensure!(valid_relative(path), "Invalid memory-delta removal path");
            replacements.insert(path.clone());
        }
        let added =
            u32::try_from(files.len()).context("Memory-delta document capacity exceeded")?;
        let maximum_doc_id = self
            .documents
            .iter()
            .map(|doc| doc.doc_id)
            .max()
            .unwrap_or(0);
        maximum_doc_id
            .checked_add(added)
            .context("Memory-delta document ID capacity exhausted")?;
        let document_count = self
            .documents
            .len()
            .checked_add(files.len())
            .and_then(|count| u32::try_from(count).ok())
            .context("Memory-delta document count capacity exhausted")?;
        let segment_id = if files.is_empty() {
            None
        } else {
            Some(
                self.segments
                    .iter()
                    .map(|segment| segment.segment_id)
                    .max()
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("Memory-delta segment ID capacity exhausted")?,
            )
        };
        let mut paths = self.paths.clone();
        let mut replacement_path_ids: HashMap<PathBuf, PathId> =
            HashMap::with_capacity(replacements.len());
        let mut aliased_replacements = Vec::new();
        for path in &replacements {
            if let Some((id, aliased)) = self.paths.find(path) {
                replacement_path_ids.insert(path.clone(), id);
                if aliased {
                    aliased_replacements.push(path.as_path());
                }
            }
        }
        let superseded_path_ids: AHashSet<PathId> =
            replacement_path_ids.values().copied().collect();
        let mut documents = self.documents.clone();
        for doc in &mut documents {
            if superseded_path_ids.contains(&doc.path_id)
                || (!aliased_replacements.is_empty()
                    && self
                        .paths
                        .get(doc.path_id as usize)
                        .is_some_and(|path| aliased_replacements.contains(&path.as_path())))
            {
                doc.flags.set_tombstone();
            }
        }
        let mut new_files = Vec::with_capacity(files.len());
        for (offset, file) in files.into_iter().enumerate() {
            let doc_id = maximum_doc_id + offset as u32 + 1;
            let path_id = if let Some(&id) = replacement_path_ids.get(&file.rel_path) {
                id
            } else {
                let id = u32::try_from(paths.len())
                    .context("Memory-delta path ID capacity exhausted")?;
                replacement_path_ids.insert(file.rel_path.clone(), id);
                paths.push(file.rel_path.clone());
                id
            };
            documents.push(Document {
                doc_id,
                path_id,
                size: file.size,
                mtime: file.mtime,
                language: file.language,
                flags: file.flags,
                segment_id: segment_id.expect("nonempty files have a checked segment ID"),
            });
            new_files.push((doc_id, file));
        }
        let mut segments = self.segments.clone();
        let mut meta = self.meta.clone();
        if let Some(segment_id) = segment_id {
            segments.push(Arc::new(SegmentReader::from_memory(segment_id, new_files)?));
            if meta.base_segment.is_none() {
                meta.base_segment = Some(segment_id);
            } else {
                meta.delta_segments.push(segment_id);
            }
        }
        meta.doc_count = document_count;
        meta.segment_count = u16::try_from(segments.len())
            .context("Memory-delta segment count capacity exhausted")?;
        meta.tombstone_count = documents
            .iter()
            .filter(|doc| doc.flags.is_tombstone())
            .count() as u32;
        meta.valid_doc_count = documents.iter().filter(|doc| doc.is_valid()).count() as u32;
        meta.rejected_files
            .retain(|(path, _)| !replacements.contains(path));
        let doc_id_to_index = DocumentLookup::new(&documents);
        Ok(Self {
            _generation_lease: self._generation_lease.clone(),
            root_path: self.root_path.clone(),
            index_path: self.index_path.clone(),
            meta,
            documents,
            doc_id_to_index,
            paths,
            segments,
            stop_grams: self.stop_grams.clone(),
            file_cache: Arc::clone(&self.file_cache),
            content_cache_enabled: self.content_cache_enabled,
            source_pack_enabled: self.source_pack_enabled,
            valid_docs_cache: OnceLock::new(),
            path_order_cache: OnceLock::new(),
        })
    }

    /// Warm path lookups before accepting watcher events. Ordinary searches
    /// never call this and retain their allocation-free path-table access.
    pub(crate) fn prepare_watched_paths(&self) {
        self.paths.prepared_lookup();
    }

    /// Find the newest live incarnation without hashing every document path.
    /// Tombstones remain in the path table so a later recreation reuses its ID.
    pub(crate) fn document_for_path(&self, path: &Path) -> Option<&Document> {
        let (path_id, aliased) = self.paths.find(path)?;
        if aliased {
            // Legacy tables may assign multiple IDs to one normalized path.
            // Full reconciliation chooses the highest live document ID.
            self.documents
                .iter()
                .filter(|doc| {
                    doc.is_valid()
                        && self
                            .paths
                            .get(doc.path_id as usize)
                            .is_some_and(|candidate| candidate == path)
                })
                .max_by_key(|doc| doc.doc_id)
        } else if matches!(&self.doc_id_to_index, DocumentLookup::Contiguous(_)) {
            self.documents
                .iter()
                .rev()
                .find(|doc| doc.path_id == path_id && doc.is_valid())
        } else {
            // Sparse legacy tables need not store rows in document-ID order.
            self.documents
                .iter()
                .filter(|doc| doc.path_id == path_id && doc.is_valid())
                .max_by_key(|doc| doc.doc_id)
        }
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

    /// Get documents matching a token. Returns an error if token evidence is
    /// unavailable or invalid; lean indexes require rebuilding with the full profile.
    pub fn get_token_docs(&self, token: &str) -> Result<RoaringBitmap> {
        self.ensure_tokens()?;
        let token_lower = token.to_lowercase();
        Ok(if self.segments.len() <= 1 {
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
        })
    }

    /// Get documents whose token dictionary has any token containing `needle`
    /// as a substring (queries all segments in parallel). Used as a recall
    /// fallback when trigram narrowing is unavailable (stop-grams).
    pub fn get_token_docs_containing(&self, needle: &str) -> Result<RoaringBitmap> {
        self.ensure_tokens()?;
        let needle_lower = needle.to_lowercase();
        Ok(if self.segments.len() <= 1 {
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
        })
    }

    /// Get stored line offsets. Missing maps return `None`; unreadable or
    /// malformed maps return an error instead of silently inventing line 1.
    #[allow(dead_code)]
    pub fn get_line_map(&self, doc_id: DocId) -> Result<Option<&Vec<u32>>> {
        if self.meta.profile == IndexProfile::Lean {
            return Ok(None);
        }
        for segment in &self.segments {
            if let Some(line_map) = segment.get_line_map(doc_id)? {
                return Ok(Some(line_map));
            }
        }
        Ok(None)
    }

    /// Convert a representable byte offset to a one-based stored line number.
    /// Returns `None` when no map is available. Offsets beyond EOF select the
    /// last stored line, as before; this API does not validate source length.
    #[allow(dead_code)]
    pub fn offset_to_line(&self, doc_id: DocId, offset: usize) -> Result<Option<u32>> {
        let offset = u32::try_from(offset).context("Byte offset exceeds line map format")?;
        Ok(self
            .get_line_map(doc_id)?
            .map(|map| map.partition_point(|&start| start <= offset) as u32))
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
        let search = |segment: &Arc<SegmentReader>| segment.intersect_trigrams(trigrams);
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
        let search = |segment: &Arc<SegmentReader>| query.in_segment(segment, self.valid_doc_ids());
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
    /// Errors if token evidence is unavailable or invalid.
    /// Returns None if any full-profile segment lacks position data (legacy fallback).
    /// Returns Some(bitmap) of doc_ids where the phrase appears.
    pub fn resolve_phrase_positional(
        &self,
        phrase_tokens: &[(String, u32)],
        candidates: Option<&RoaringBitmap>,
    ) -> Result<Option<RoaringBitmap>> {
        self.ensure_tokens()?;
        if phrase_tokens.len() < 2 {
            return Ok(None);
        }

        // Check all segments have position data
        if self.segments.iter().any(|s| s.tokens().positions.is_none()) {
            return Ok(None);
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

        Ok(Some(result))
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

    pub(crate) fn should_use_source_pack(&self, candidates: usize) -> bool {
        self.source_pack_enabled
            && !self.content_cache_enabled
            && candidates >= 128
            && self
                .segments
                .iter()
                .any(|segment| segment.segment_path.join("source.table").is_file())
    }

    pub(crate) fn packed_literal(
        &self,
        doc: &Document,
        full_path: &Path,
        finder: &memchr::memmem::Finder<'_>,
    ) -> Option<bool> {
        if !self.source_pack_enabled {
            return None;
        }
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.segment_id == doc.segment_id)?;
        segment
            .source_pack
            .get_or_init(|| crate::index::source_pack::SourcePack::open(&segment.segment_path).ok())
            .as_ref()?
            .contains_literal(doc.doc_id, self.get_path(doc)?, full_path, finder)
    }

    pub(crate) fn packed_line_match(
        &self,
        doc: &Document,
        full_path: &Path,
        matches: impl Fn(&str) -> bool,
    ) -> Option<bool> {
        if !self.source_pack_enabled {
            return None;
        }
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.segment_id == doc.segment_id)?;
        segment
            .source_pack
            .get_or_init(|| crate::index::source_pack::SourcePack::open(&segment.segment_path).ok())
            .as_ref()?
            .matches_lines(doc.doc_id, self.get_path(doc)?, full_path, matches)
    }
    pub(crate) fn packed_source(
        &self,
        doc: &Document,
        full_path: &Path,
    ) -> Option<std::borrow::Cow<'_, str>> {
        if !self.source_pack_enabled {
            return None;
        }
        let segment = self
            .segments
            .iter()
            .find(|segment| segment.segment_id == doc.segment_id)?;
        segment
            .source_pack
            .get_or_init(|| crate::index::source_pack::SourcePack::open(&segment.segment_path).ok())
            .as_ref()?
            .read(doc.doc_id, self.get_path(doc)?, full_path)
    }

    pub(crate) fn read_file_for_scan(&self, path: &Path, cache_scan: bool) -> Option<FileContent> {
        self.read_file_with_cache_policy(path, cache_scan)
    }

    pub(crate) fn cached_literal_anchor(&self, literal: &[u8]) -> Option<usize> {
        if !self.content_cache_enabled || self.file_cache.max_bytes == 0 || literal.len() < 8 {
            return None;
        }
        literal
            .windows(3)
            .enumerate()
            .filter(|(_, bytes)| !self.is_stop_gram(bytes_to_trigram(bytes[0], bytes[1], bytes[2])))
            .min_by_key(|(_, bytes)| {
                let gram = bytes_to_trigram(bytes[0], bytes[1], bytes[2]);
                self.segments
                    .iter()
                    .map(|segment| u64::from(segment.get_trigram_doc_freq(gram)))
                    .sum::<u64>()
            })
            .map(|(offset, _)| offset)
    }

    /// Read file content with LRU caching.
    /// This speeds up repeated queries that access the same files.
    /// The cache stores Arc<SourceSnapshot>; Unix hits only bump the refcount,
    /// while non-Unix hits also compare source bytes. Files too large to cache return
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
                let snapshot = Arc::clone(content);
                #[cfg(unix)]
                return Some(FileContent::Cached(snapshot));
                #[cfg(not(unix))]
                {
                    // Size/mtime/creation time cannot detect same-size rewrites
                    // with restored or coarse timestamps. Read outside the lock.
                    drop(cache);
                    let verified = Self::revalidate_cached_bytes(path, &snapshot);
                    if !matches!(verified, Some(FileContent::Cached(_)))
                        && let Ok(mut cache) = shard.lock()
                        && cache
                            .entries
                            .peek(path)
                            .is_some_and(|(_, current)| Arc::ptr_eq(current, &snapshot))
                        && let Some((_, old)) = cache.entries.pop(path)
                    {
                        cache.bytes -= old.len();
                    }
                    return verified;
                }
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
            let content: Arc<SourceSnapshot> = Arc::new(content.into());
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

    #[cfg(any(not(unix), test))]
    fn revalidate_cached_bytes(path: &Path, cached: &Arc<SourceSnapshot>) -> Option<FileContent> {
        let current = Self::read_file_uncached(path)?;
        if current.as_str() == &***cached {
            Some(FileContent::Cached(Arc::clone(cached)))
        } else {
            Some(FileContent::Owned(current))
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

fn validate_document_references(
    meta: &IndexMeta,
    documents: &[Document],
    path_count: usize,
) -> Result<()> {
    anyhow::ensure!(
        meta.doc_count as usize == documents.len(),
        "Document count does not match metadata"
    );
    let ids: AHashSet<_> = meta
        .base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
        .collect();
    anyhow::ensure!(
        ids.len() == usize::from(meta.base_segment.is_some()) + meta.delta_segments.len(),
        "Duplicate segment IDs"
    );
    anyhow::ensure!(
        documents
            .iter()
            .all(|doc| (doc.path_id as usize) < path_count && ids.contains(&doc.segment_id)),
        "Document references a missing path or segment"
    );
    Ok(())
}

fn segment_document_ids(documents: &[Document]) -> HashMap<SegmentId, Arc<RoaringBitmap>> {
    let mut ids: HashMap<SegmentId, RoaringBitmap> = HashMap::new();
    for doc in documents {
        ids.entry(doc.segment_id).or_default().insert(doc.doc_id);
    }
    ids.into_iter()
        .map(|(id, docs)| (id, Arc::new(docs)))
        .collect()
}

/// Read documents from an immutable index generation.
pub fn read_documents(index_path: &Path) -> Result<Vec<Document>> {
    let meta: IndexMeta = serde_json::from_reader(File::open(index_path.join("meta.json"))?)?;
    meta.validate_format()?;
    read_documents_version(index_path, meta.version)
}

fn read_documents_version(index_path: &Path, version: u32) -> Result<Vec<Document>> {
    anyhow::ensure!(
        matches!(version, 1..=3),
        "Unsupported index version; rebuild the index"
    );
    let data = MappedBytes::open(&index_path.join("docs.bin"))?;
    anyhow::ensure!(data.len() >= 4, "Truncated document header");
    let count = le32(&data) as usize;
    anyhow::ensure!(
        count <= (data.len() - 4) / 30,
        "Index count exceeds file bounds"
    );
    anyhow::ensure!(data.len() - 4 == count * 30, "Trailing document bytes");
    let documents: Vec<Document> = data[4..]
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
        .collect();
    anyhow::ensure!(
        documents
            .iter()
            .all(|doc| doc.doc_id != 0 && doc.flags.0 & !0x3f == 0),
        "Invalid document ID or flags"
    );
    if !documents
        .windows(2)
        .all(|pair| pair[0].doc_id < pair[1].doc_id)
    {
        let mut seen = AHashSet::with_capacity(documents.len());
        anyhow::ensure!(
            documents.iter().all(|doc| seen.insert(doc.doc_id)),
            "Duplicate document IDs"
        );
    }
    Ok(documents)
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
        let path = PathBuf::from(std::str::from_utf8(bytes).context("Invalid path UTF-8")?);
        anyhow::ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
            "Unsafe index path"
        );
        paths.push(path);
        cursor += 4 + len;
    }
    anyhow::ensure!(cursor == data.len(), "Trailing path bytes");
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
    postings: &[u8],
    positions: Option<&[u8]>,
    allowed_docs: &RoaringBitmap,
) -> Result<TokenDict> {
    let has_positions = positions.is_some();
    let data = MappedBytes::open(&segment_path.join("tokens.dict"))?;
    let header = super::token_dictionary::header(&data, has_positions)?;
    let mut offsets = Vec::with_capacity(header.count);
    let mut cursor = header.start;
    let mut previous_token = None;
    let validator = crate::utils::encoding::DocumentPostingsValidator::new(allowed_docs);
    for _ in 0..header.count {
        let (entry, consumed) =
            super::token_dictionary::entry(&data[cursor..], header.compact, has_positions)?;
        anyhow::ensure!(
            previous_token.is_none_or(|previous| previous < entry.token),
            "Unsorted token dictionary"
        );
        previous_token = Some(entry.token);
        anyhow::ensure!(
            posting_range_fits(entry.offset, entry.length, postings.len()),
            "Truncated token postings"
        );
        let start = entry.offset as usize;
        validator.validate(
            &postings[start..start + entry.length as usize],
            entry.doc_freq,
        )?;
        if let Some(positions) = positions {
            anyhow::ensure!(
                posting_range_fits(entry.pos_offset, entry.pos_length, positions.len()),
                "Truncated token positions"
            );
            let start = entry.pos_offset as usize;
            crate::utils::encoding::validate_position_stream_with_documents(
                &positions[start..start + entry.pos_length as usize],
                Some(allowed_docs),
            )?;
        }
        offsets.push(cursor);
        cursor += consumed;
    }
    anyhow::ensure!(cursor == data.len(), "Trailing token dictionary bytes");
    Ok(TokenDict {
        data,
        offsets,
        has_positions,
        compact: header.compact,
    })
}

/// Read line maps
pub(crate) fn read_line_maps(segment_path: &Path) -> Result<HashMap<DocId, Vec<u32>>> {
    let linemap_path = segment_path.join("linemap.bin");

    let file = match File::open(&linemap_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashMap::new());
        }
        Err(error) => return Err(error).context("Opening line map"),
    };
    let mut file = BufReader::new(file);

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
        let line_count = u32::from_le_bytes(buf4);

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
        let count = crate::utils::encoding::validate_delta_stream(&encoded)?;
        anyhow::ensure!(
            count == line_count as usize,
            "Line map count does not match payload"
        );
        let offsets = delta_decode(&encoded);
        anyhow::ensure!(
            offsets.first() == Some(&0) && offsets.windows(2).all(|pair| pair[0] < pair[1]),
            "Line starts must begin at zero and strictly increase"
        );
        anyhow::ensure!(
            line_maps.insert(doc_id, offsets).is_none(),
            "Duplicate line map document"
        );
    }

    anyhow::ensure!(remaining == 0, "Trailing line map data");
    Ok(line_maps)
}

/// Read bloom filter from segment
pub(crate) fn read_bloom_filter(segment_path: &Path) -> Result<BloomFilter> {
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

/// Establish exactly the core invariants required by a files-only gram query,
/// including inherited segments. A routing certificate may reuse these checks
/// only while strong file stamps remain unchanged. Unused token/line-map/source
/// evidence keeps its existing independent, lazy validation behavior.
pub(crate) fn validate_negative_routing_core(index_path: &Path, meta: &IndexMeta) -> Result<()> {
    meta.validate_format()?;
    let documents = read_documents_version(index_path, meta.version)?;
    let paths = read_paths(index_path)?;
    validate_document_references(meta, &documents, paths.len())?;
    let allowed = segment_document_ids(&documents);
    for id in meta
        .base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
    {
        let path = index_path.join("segments").join(format!("seg_{id:04}"));
        let segment = SegmentReader::open(
            &path,
            id,
            meta.has_positions,
            false,
            allowed.get(&id).cloned().unwrap_or_default(),
        )?;
        let bloom = segment
            .bloom_filter
            .context("Missing or invalid routing Bloom")?;
        anyhow::ensure!(
            segment
                .trigram_dict
                .iter()
                .all(|entry| bloom.might_contain(entry.trigram)),
            "Routing Bloom omits a stored gram"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn line_map_errors_are_cached_and_missing_maps_are_distinct() {
        let (_temp, root) = create_test_index();
        let reader = IndexReader::open(&root).unwrap();
        let segment = &reader.segments[0];
        let path = segment.segment_path.join("linemap.bin");
        let original = fs::read(&path).unwrap();
        fs::write(&path, [1, 0]).unwrap();
        assert!(reader.get_line_map(0).is_err());
        assert!(reader.offset_to_line(0, 0).is_err());
        fs::write(&path, original).unwrap();
        assert!(
            reader.get_line_map(0).is_err(),
            "failure must remain cached"
        );
        drop(reader);
        let reader = IndexReader::open(&root).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(reader.get_line_map(0).unwrap(), None);
        assert_eq!(reader.offset_to_line(0, 0).unwrap(), None);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn line_map_boundaries_and_large_offsets() {
        let (_temp, root) = create_test_index();
        let reader = IndexReader::open(&root).unwrap();
        let doc = reader.documents[0].doc_id;
        let starts = reader.get_line_map(doc).unwrap().unwrap();
        for (index, &start) in starts.iter().enumerate() {
            assert_eq!(
                reader.offset_to_line(doc, start as usize).unwrap(),
                Some(index as u32 + 1)
            );
            if start > 0 {
                assert_eq!(
                    reader.offset_to_line(doc, start as usize - 1).unwrap(),
                    Some(index as u32)
                );
            }
        }
        assert_eq!(
            reader.offset_to_line(doc, u32::MAX as usize).unwrap(),
            Some(starts.len() as u32)
        );
        if let Some(large) = (u32::MAX as usize).checked_add(1) {
            assert!(reader.offset_to_line(doc, large).is_err());
        }
        assert_eq!(reader.offset_to_line(u32::MAX, 0).unwrap(), None);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn line_maps_reject_invalid_starts_and_trailing_bytes() {
        let temp = TempDir::new().unwrap();
        for offsets in [vec![], vec![1], vec![0, 0], vec![0, 2, 2]] {
            super::super::segment_io::write_line_maps(temp.path(), &HashMap::from([(0, offsets)]))
                .unwrap();
            assert!(read_line_maps(temp.path()).is_err());
        }
        super::super::segment_io::write_line_maps(temp.path(), &HashMap::from([(0, vec![0, 3])]))
            .unwrap();
        assert_eq!(read_line_maps(temp.path()).unwrap()[&0], vec![0, 3]);
        let path = temp.path().join("linemap.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        fs::write(path, bytes).unwrap();
        assert!(read_line_maps(temp.path()).is_err());
    }

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
    fn large_segment_parallel_validation_rejects_late_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let mut dictionary = 1024u32.to_le_bytes().to_vec();
        for gram in 0..1024u32 {
            dictionary.extend_from_slice(&gram.to_le_bytes());
            dictionary.extend_from_slice(&(u64::from(gram) * 1024).to_le_bytes());
            dictionary.extend_from_slice(&1024u32.to_le_bytes());
            dictionary.extend_from_slice(&1024u32.to_le_bytes());
        }
        let postings = vec![1u8; 1024 * 1024];
        let allowed = Arc::new((1..=1024).collect::<RoaringBitmap>());
        let open = || SegmentReader::open(directory.path(), 1, false, false, allowed.clone());
        fs::write(directory.path().join("grams.dict"), &dictionary).unwrap();
        fs::write(directory.path().join("grams.postings"), &postings).unwrap();
        assert_eq!(open().unwrap().get_trigram_docs(1023).len(), 1024);
        let mut bad = postings.clone();
        *bad.last_mut().unwrap() = 0; // Duplicate document at the end of the final task.
        fs::write(directory.path().join("grams.postings"), bad).unwrap();
        assert!(open().is_err());
        fs::write(directory.path().join("grams.postings"), postings).unwrap();
        dictionary[4 + 1023 * 20..4 + 1023 * 20 + 4].copy_from_slice(&0u32.to_le_bytes());
        fs::write(directory.path().join("grams.dict"), dictionary).unwrap();
        assert!(open().is_err());
    }

    #[test]
    fn ordinary_search_rejects_corrupt_gram_payloads_before_planning() {
        for damage in ["truncated_varint", "zero_id", "unknown_id", "frequency"] {
            let (_temp, root) = create_test_index();
            let generation = crate::utils::get_index_dir(&root).unwrap();
            let segment = generation.join("segments/seg_0001");
            if damage == "frequency" {
                let path = segment.join("grams.dict");
                let mut bytes = fs::read(&path).unwrap();
                bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
                fs::write(path, bytes).unwrap();
            } else {
                let path = segment.join("grams.postings");
                let mut bytes = fs::read(&path).unwrap();
                bytes[0] = match damage {
                    "truncated_varint" => 0x80,
                    "zero_id" => 0,
                    _ => 127,
                };
                fs::write(path, bytes).unwrap();
            }
            assert!(
                IndexReader::open_for_search_uncached(&root).is_err(),
                "{damage}"
            );
            assert!(IndexReader::open(&root).is_err(), "{damage}");
            crate::utils::remove_index(&root).unwrap();
        }
    }

    #[test]
    fn lazy_token_initialization_rejects_corrupt_postings_and_positions() {
        for file in ["tokens.postings", "tokens.positions"] {
            let (_temp, root) = create_test_index();
            let generation = crate::utils::get_index_dir(&root).unwrap();
            let path = generation.join("segments/seg_0001").join(file);
            let mut bytes = fs::read(&path).unwrap();
            bytes.fill(0x80);
            fs::write(path, bytes).unwrap();
            let core = IndexReader::open_for_search_uncached(&root).unwrap();
            assert!(core.ensure_tokens().is_err(), "{file}");
            assert!(core.ensure_tokens().is_err(), "cached failure {file}");
            assert!(IndexReader::open(&root).is_err(), "{file}");
            // Unused auxiliary data is not required by a gram-only query.
            assert_eq!(
                crate::query::QueryExecutor::new(&core)
                    .execute_files_only(&crate::query::parse_query("main"), 0)
                    .unwrap(),
                vec![PathBuf::from("test.rs")]
            );
            crate::utils::remove_index(&root).unwrap();
        }
    }

    #[test]
    fn internal_search_readers_load_complete_token_data_on_demand() {
        let (_temp, root) = create_test_index();
        let full = IndexReader::open(&root).unwrap();
        let expected_docs = full.get_token_docs("main").unwrap();
        let phrase = vec![("fn".into(), 0), ("main".into(), 1)];
        let expected_positions = full.resolve_phrase_positional(&phrase, None).unwrap();
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
        assert_eq!(core.get_token_docs("main").unwrap(), expected_docs);
        assert_eq!(
            core.resolve_phrase_positional(&phrase, None).unwrap(),
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
        docs.extend_from_slice(&0x25u16.to_le_bytes());
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
        assert_eq!(doc.flags.0, 0x25);
        assert_eq!(doc.segment_id, u16::MAX);
        assert_eq!(
            read_documents_version(dir.path(), 1).unwrap()[0].mtime,
            123_000_000_000
        );
        let names: [&[u8]; 3] = [b"simple.rs", "space/K.rs".as_bytes(), b"nested/./valid.rs"];
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
    fn damaged_metadata_is_rejected_without_lossy_interpretation() {
        let dir = tempfile::tempdir().unwrap();
        for name in [b"".as_slice(), b"invalid\xff", b"../outside", b"/absolute"] {
            let mut bytes = 1u32.to_le_bytes().to_vec();
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(name);
            fs::write(dir.path().join("paths.bin"), bytes).unwrap();
            assert!(read_paths(dir.path()).is_err(), "{name:?}");
        }
        let (_temp, root) = create_test_index();
        let generation = crate::utils::get_index_dir(&root).unwrap();
        let original = fs::read(generation.join("docs.bin")).unwrap();
        for (offset, replacement) in [
            (4, 0u32.to_le_bytes().to_vec()),
            (8, u32::MAX.to_le_bytes().to_vec()),
            (30, 0x8000u16.to_le_bytes().to_vec()),
            (32, u16::MAX.to_le_bytes().to_vec()),
        ] {
            let mut bytes = original.clone();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            fs::write(generation.join("docs.bin"), bytes).unwrap();
            assert!(IndexReader::open(&root).is_err(), "offset {offset}");
        }
        crate::utils::remove_index(&root).unwrap();
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
        let docs = reader.get_token_docs("main").unwrap();
        assert!(!docs.is_empty(), "Should find documents with 'main' token");

        let docs = reader.get_token_docs("println").unwrap();
        assert!(
            !docs.is_empty(),
            "Should find documents with 'println' token"
        );
    }

    #[test]
    fn test_token_lookup_nonexistent() {
        let (_temp_dir, root_path) = create_test_index();
        let reader = IndexReader::open(&root_path).expect("Failed to open index");

        let docs = reader.get_token_docs("xyznonexistent123").unwrap();
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
    #[test]
    fn byte_revalidation_detects_same_stamp_edits_and_invalid_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.txt");
        std::fs::write(&path, "needle\n").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let cached = Arc::new(SourceSnapshot::from(String::from("needle\n")));
        for text in ["absent\n", "needle\n", "absent\n"] {
            std::fs::write(&path, text).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            let result = IndexReader::revalidate_cached_bytes(&path, &cached).unwrap();
            match result {
                FileContent::Cached(snapshot) => {
                    assert_eq!(text, "needle\n");
                    assert!(Arc::ptr_eq(&snapshot, &cached));
                }
                FileContent::Owned(current) => {
                    assert_eq!(text, "absent\n");
                    assert_eq!(current, text);
                }
            }
        }
        std::fs::write(&path, b"needle\xff").unwrap();
        assert!(IndexReader::revalidate_cached_bytes(&path, &cached).is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(IndexReader::revalidate_cached_bytes(&path, &cached).is_none());
    }

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
                Arc::new(SourceSnapshot::from("x".repeat(128 * 1024))),
            );
            assert!(cache.bytes <= cache.max_bytes);
        }
        assert!(!cache.entries.contains(Path::new("f0")));
        cache.put(
            "f63".into(),
            stamp.clone(),
            Arc::new(SourceSnapshot::from("small".to_owned())),
        );
        assert_eq!(
            cache.bytes,
            cache
                .entries
                .iter()
                .map(|(_, (_, text))| text.len())
                .sum::<usize>()
        );
        for i in 0..cache.max_entries + 1 {
            cache.put(
                format!("small{i}").into(),
                stamp.clone(),
                Arc::new(SourceSnapshot::from("x".to_owned())),
            );
        }
        assert_eq!(cache.entries.len(), cache.max_entries);
        assert_eq!(cache.bytes, cache.max_entries);
    }
}

#[cfg(test)]
mod memory_delta_tests {
    use super::*;
    use crate::index::build::ProcessedFile;
    use crate::query::{QueryExecutor, parse_query};
    use std::fs;

    fn processed(root: &Path, relative: &str) -> ProcessedFile {
        let path = root.join(relative);
        let content = fs::read_to_string(&path).unwrap();
        let (tokens, token_positions) = crate::utils::extract_tokens_and_positions(&content);
        let mut line_offsets = vec![0];
        line_offsets.extend(
            content
                .bytes()
                .enumerate()
                .filter(|(offset, byte)| *byte == b'\n' && offset + 1 < content.len())
                .map(|(offset, _)| (offset + 1) as u32),
        );
        ProcessedFile {
            rel_path: relative.into(),
            mtime: fs::metadata(path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
            size: content.len() as u64,
            language: Language::from_extension(
                Path::new(relative)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or(""),
            ),
            flags: DocFlags::new(),
            trigrams: crate::utils::extract_trigrams(content.as_bytes()),
            tokens,
            token_positions,
            line_offsets,
        }
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn compare_all_outputs(memory: &IndexReader, disk: &IndexReader) {
        let memory_executor = QueryExecutor::new(memory);
        let disk_executor = QueryExecutor::new(disk);
        for text in [
            "alpha",
            "alpha beta",
            "alpha -forbidden",
            "(alpha | stable) -forbidden",
            "\"alpha beta\"",
            "re:/alpha.*beta/",
            "re:/^(alpha|stable)/",
            "alpha lang:rust",
            "alpha ext:py",
            "path:nested",
            "alpha path:nested",
            "alpha line:2-3",
            "K",
            "Σ",
            "x | alpha",
            "neverpresent",
            "alpha missing",
            "-forbidden",
            "alpha | re:/[x]/",
        ] {
            let mut query = parse_query(text);
            query.options.limit = 0;
            for limit in [0, 1, 2, 9] {
                assert_eq!(
                    memory_executor.execute_files_only(&query, limit).unwrap(),
                    disk_executor.execute_files_only(&query, limit).unwrap(),
                    "files: {text}, limit {limit}"
                );
                assert_eq!(
                    memory_executor.execute_match_counts(&query, limit).unwrap(),
                    disk_executor.execute_match_counts(&query, limit).unwrap(),
                    "counts: {text}, limit {limit}"
                );
            }
            let content = |executor: &QueryExecutor<'_>| {
                executor
                    .execute_with_content(&query, 1, 1)
                    .unwrap()
                    .into_iter()
                    .map(|m| {
                        (
                            m.path,
                            m.line_number,
                            m.line_content,
                            m.match_start,
                            m.match_end,
                            m.context_before,
                            m.context_after,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                content(&memory_executor),
                content(&disk_executor),
                "content: {text}"
            );
            // Document IDs and stable ordering among equal scores depend on
            // build order. Compare the complete ranked records by path/line.
            let scores = |executor: &QueryExecutor<'_>| {
                let mut scores: Vec<_> = executor
                    .execute(&query)
                    .unwrap()
                    .into_iter()
                    .map(|m| (m.path, m.line_number, m.score))
                    .collect();
                scores.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                scores
            };
            let actual = scores(&memory_executor);
            let expected = scores(&disk_executor);
            assert_eq!(actual.len(), expected.len(), "ranked count: {text}");
            for (actual, expected) in actual.iter().zip(&expected) {
                assert_eq!(
                    (&actual.0, actual.1),
                    (&expected.0, expected.1),
                    "ranked match: {text}"
                );
                assert!(
                    (actual.2 - expected.2).abs() < 0.00001,
                    "ranked score: {text}"
                );
            }
        }
        for text in ["re:/[/", "absent re:/[/", "-re:/[/"] {
            let query = parse_query(text);
            assert!(memory_executor.execute_files_only(&query, 0).is_err());
            assert!(memory_executor.execute_match_counts(&query, 0).is_err());
            assert!(memory_executor.execute_with_content(&query, 0, 0).is_err());
            assert!(memory_executor.execute(&query).is_err());
        }
    }

    #[test]
    fn memory_deltas_match_fresh_indexes_across_queries_and_repeated_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for (path, text) in [
            ("edited.rs", "oldmarker stable\n"),
            ("deleted.rs", "alpha beta\n"),
            ("untouched.rs", "stable alpha beta\n"),
            ("nested/other.py", "alpha beta\nforbidden\n"),
            ("unicode.txt", "K alpha Σ\n"),
            ("retained.rs", "alpha beta\nalpha beta\nalpha beta\n"),
        ] {
            write(&root, path, text);
        }
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let base = IndexReader::open(&root).unwrap();
        let generation = base.generation_path().to_path_buf();
        write(
            &root,
            "edited.rs",
            "alpha beta\nstable retained\nalpha beta\n",
        );
        write(&root, "new.rs", "alpha x beta\nforbidden\n");
        fs::remove_file(root.join("deleted.rs")).unwrap();
        let mut memory = base
            .with_memory_delta(
                vec![processed(&root, "edited.rs"), processed(&root, "new.rs")],
                &["edited.rs".into(), "deleted.rs".into()],
            )
            .unwrap();
        assert!(Arc::ptr_eq(&base.segments[0], &memory.segments[0]));
        assert_eq!(crate::utils::get_index_dir(&root).unwrap(), generation);
        assert_eq!(base.valid_doc_ids().len(), 6);
        assert_eq!(memory.valid_doc_ids().len(), 6);
        for round in 0..4 {
            if round > 0 {
                write(
                    &root,
                    "edited.rs",
                    if round % 2 == 0 {
                        "alpha beta\n"
                    } else {
                        "stable alpha\n"
                    },
                );
                write(&root, "deleted.rs", "alpha beta recreated\n");
                fs::remove_file(root.join("new.rs")).ok();
                memory = memory
                    .with_memory_delta(
                        vec![
                            processed(&root, "edited.rs"),
                            processed(&root, "deleted.rs"),
                        ],
                        &["new.rs".into()],
                    )
                    .unwrap();
            }
            crate::index::build::build_index_with_progress(&root, true, true).unwrap();
            let disk = IndexReader::open(&root).unwrap();
            compare_all_outputs(&memory, &disk);
            let live_paths: HashSet<_> = memory
                .valid_doc_ids()
                .iter()
                .map(|id| memory.get_path(memory.get_document(id).unwrap()).unwrap())
                .collect();
            assert_eq!(live_paths.len() as u64, memory.valid_doc_ids().len());
        }
        // A final deletion-only snapshot has no extra segment and still
        // supersedes every historical incarnation of the path.
        fs::remove_file(root.join("edited.rs")).unwrap();
        let deleted = memory
            .with_memory_delta(vec![], &["edited.rs".into()])
            .unwrap();
        assert_eq!(deleted.segments.len(), memory.segments.len());
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        compare_all_outputs(&deleted, &IndexReader::open(&root).unwrap());
        drop((deleted, memory, base));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn memory_delta_pins_lazy_base_resources_after_durable_generations_change() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write(&root, "old.rs", "alpha beta\n");
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let base = IndexReader::open_for_search_uncached(&root).unwrap();
        let generation = base.generation_path().to_path_buf();
        assert!(base.segments[0].tokens.get().is_none());
        assert!(base.segments[0].line_maps.get().is_none());
        write(&root, "new.rs", "alpha beta\n");
        let memory = base
            .with_memory_delta(vec![processed(&root, "new.rs")], &[])
            .unwrap();
        drop(base);
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        assert!(
            generation.exists(),
            "memory reader lost the original generation lease"
        );
        memory.ensure_tokens().unwrap();
        let base_doc = memory
            .documents
            .iter()
            .find(|doc| memory.get_path(doc).unwrap() == Path::new("old.rs"))
            .unwrap();
        assert_eq!(
            memory.segments[0].get_line_map(base_doc.doc_id).unwrap(),
            Some(&vec![0])
        );
        assert_eq!(
            QueryExecutor::new(&memory)
                .execute_files_only(&parse_query("\"alpha beta\""), 0)
                .unwrap(),
            vec![PathBuf::from("new.rs"), PathBuf::from("old.rs")]
        );
        drop(memory);
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        assert!(
            !generation.exists(),
            "dropping memory reader did not release generation lease"
        );
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn memory_paths_share_durable_storage_and_preserve_appended_ids_across_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write(&root, "base.rs", "alpha beta\n");
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let base = IndexReader::open(&root).unwrap();
        assert!(base.paths.base_lookup.get().is_none());
        assert_eq!(
            QueryExecutor::new(&base)
                .execute_files_only(&parse_query("alpha"), 0)
                .unwrap(),
            vec![PathBuf::from("base.rs")]
        );
        assert!(
            base.paths.base_lookup.get().is_none(),
            "unwatched query allocated a path lookup"
        );
        base.prepare_watched_paths();
        assert!(base.paths.base_lookup.get().is_some());
        assert_eq!(
            base.document_for_path(Path::new("base.rs"))
                .unwrap()
                .path_id,
            0
        );
        assert!(base.document_for_path(Path::new("absent.rs")).is_none());
        write(&root, "nested/one.rs", "alpha beta\n");
        write(&root, "two.rs", "alpha beta\n");
        let first = base
            .with_memory_delta(
                vec![
                    processed(&root, "nested/one.rs"),
                    processed(&root, "two.rs"),
                ],
                &[],
            )
            .unwrap();
        let path_id = |reader: &IndexReader, path: &str| {
            reader
                .documents
                .iter()
                .find(|doc| doc.is_valid() && reader.get_path(doc).unwrap() == Path::new(path))
                .unwrap()
                .path_id
        };
        let first_id = path_id(&first, "nested/one.rs");
        assert!(Arc::ptr_eq(&base.paths.base, &first.paths.base));
        assert!(Arc::ptr_eq(
            &base.paths.base_lookup,
            &first.paths.base_lookup
        ));
        assert!(std::ptr::eq(
            base.paths.prepared_lookup(),
            first.paths.prepared_lookup()
        ));
        assert_eq!(
            first
                .document_for_path(Path::new("nested/./one.rs"))
                .unwrap()
                .path_id,
            first_id
        );
        assert!(base.paths.appended.is_empty());
        assert_eq!(first.paths.appended.len(), 2);
        write(&root, "nested/one.rs", "stable alpha\n");
        write(&root, "three.rs", "alpha beta\n");
        let second = first
            .with_memory_delta(
                vec![
                    processed(&root, "nested/one.rs"),
                    processed(&root, "three.rs"),
                ],
                &[],
            )
            .unwrap();
        assert!(Arc::ptr_eq(&base.paths.base, &second.paths.base));
        assert_eq!(second.paths.appended.len(), 3);
        assert_eq!(
            first.paths.appended.len(),
            2,
            "derived snapshot mutated its predecessor"
        );
        assert_eq!(path_id(&second, "nested/one.rs"), first_id);
        let deleted = second
            .with_memory_delta(vec![], &["nested/one.rs".into()])
            .unwrap();
        assert!(
            deleted
                .document_for_path(Path::new("nested/one.rs"))
                .is_none()
        );
        let restored = deleted
            .with_memory_delta(vec![processed(&root, "nested/one.rs")], &[])
            .unwrap();
        assert_eq!(
            restored
                .document_for_path(Path::new("nested/one.rs"))
                .unwrap()
                .path_id,
            first_id
        );
        assert!(Arc::ptr_eq(
            &base.paths.base_lookup,
            &restored.paths.base_lookup
        ));
        assert_eq!(path_id(&restored, "nested/one.rs"), first_id);
        assert_eq!(restored.paths.len(), 4);
        for (index, path) in restored.paths.iter().enumerate() {
            assert_eq!(restored.paths.get(index), Some(path));
        }
        assert!(restored.paths.get(restored.paths.len()).is_none());
        assert!(restored.paths.get(usize::MAX).is_none());
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        compare_all_outputs(&restored, &IndexReader::open(&root).unwrap());
        drop((restored, deleted, second, first, base));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn small_and_large_replacement_sets_preserve_path_component_equality() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write(&root, "nested/base.rs", "alpha beta\n");
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let base = IndexReader::open(&root).unwrap();
        for count in [1, 9] {
            let mut removed: Vec<PathBuf> = (0..count)
                .map(|index| format!("absent{index}.rs").into())
                .collect();
            removed[0] = "nested/./base.rs".into();
            let memory = base.with_memory_delta(vec![], &removed).unwrap();
            assert!(
                memory.valid_doc_ids().is_empty(),
                "replacement count {count}"
            );
            assert_eq!(base.valid_doc_ids().len(), 1);
            assert!(Arc::ptr_eq(&base.paths.base, &memory.paths.base));
        }
        drop(base);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn watched_lookup_matches_full_reconciliation_for_reordered_documents() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write(&root, "same.rs", "alpha beta\n");
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let mut reader = IndexReader::open(&root).unwrap();
        let template = reader.documents[0].clone();
        let path = Path::new("same.rs");
        reader.prepare_watched_paths();

        for ids in [[1, 2, 3], [1, 5, 9], [9, 1, 5]] {
            reader.documents = ids
                .iter()
                .map(|&doc_id| Document {
                    doc_id,
                    size: u64::from(doc_id) + 10,
                    mtime: u64::from(doc_id) + 100,
                    ..template.clone()
                })
                .collect();
            reader.doc_id_to_index = DocumentLookup::new(&reader.documents);
            let mut newest_first = ids;
            newest_first.sort_unstable_by(|left, right| right.cmp(left));

            for expected in newest_first.into_iter().map(Some).chain([None]) {
                reader.valid_docs_cache = OnceLock::new();
                // Full reconciliation inserts documents in ascending ID order,
                // so its final metadata for a path comes from the highest ID.
                let full = reader
                    .valid_doc_ids()
                    .iter()
                    .filter_map(|id| reader.get_document(id))
                    .rfind(|doc| reader.get_path(doc).is_some_and(|p| p == path));
                let scoped = reader.document_for_path(path);
                assert_eq!(scoped.map(|doc| doc.doc_id), expected, "rows {ids:?}");
                assert_eq!(
                    scoped.map(|doc| (doc.doc_id, doc.mtime, doc.size)),
                    full.map(|doc| (doc.doc_id, doc.mtime, doc.size)),
                    "rows {ids:?}"
                );
                if let Some(id) = expected {
                    reader
                        .documents
                        .iter_mut()
                        .find(|doc| doc.doc_id == id)
                        .unwrap()
                        .flags
                        .set_tombstone();
                }
            }
        }
        drop(reader);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn watched_lookup_handles_legacy_path_aliases_and_recreation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write(&root, "nested/same.rs", "alpha beta\n");
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let mut base = IndexReader::open(&root).unwrap();
        // The last path-table ID is canonical, but the newest live document
        // can still refer to an earlier equivalent ID in a legacy table.
        base.paths = PathTable::new(vec!["nested/same.rs".into(), "nested/./same.rs".into()]);
        let mut first = base.documents[0].clone();
        first.doc_id = 1;
        first.path_id = 1;
        let mut newest = first.clone();
        newest.doc_id = 2;
        newest.path_id = 0;
        base.documents = vec![first, newest];
        base.doc_id_to_index = DocumentLookup::new(&base.documents);
        base.meta.doc_count = 2;
        base.meta.valid_doc_count = 2;
        base.prepare_watched_paths();
        assert_eq!(
            base.paths.find(Path::new("nested/same.rs")),
            Some((1, true))
        );
        assert_eq!(
            base.document_for_path(Path::new("nested/same.rs"))
                .unwrap()
                .doc_id,
            2
        );
        base.documents[1].flags.set_tombstone();
        assert_eq!(
            base.document_for_path(Path::new("nested/./same.rs"))
                .unwrap()
                .doc_id,
            1
        );
        base.documents[1].flags = DocFlags::new();
        let memory = base
            .with_memory_delta(vec![processed(&root, "nested/same.rs")], &[])
            .unwrap();
        assert_eq!(
            memory
                .document_for_path(Path::new("nested/same.rs"))
                .unwrap()
                .path_id,
            1
        );
        assert_eq!(memory.valid_doc_ids().len(), 1);
        assert_eq!(base.valid_doc_ids().len(), 2);
        assert_eq!(
            QueryExecutor::new(&memory)
                .execute_files_only(&parse_query("alpha"), 0)
                .unwrap(),
            vec![PathBuf::from("nested/same.rs")]
        );
        let removed = memory
            .with_memory_delta(vec![], &["nested/same.rs".into()])
            .unwrap();
        assert!(
            removed
                .document_for_path(Path::new("nested/same.rs"))
                .is_none()
        );
        let recreated = removed
            .with_memory_delta(vec![processed(&root, "nested/same.rs")], &[])
            .unwrap();
        assert_eq!(
            recreated
                .document_for_path(Path::new("nested/same.rs"))
                .unwrap()
                .path_id,
            1
        );
        assert_eq!(recreated.valid_doc_ids().len(), 1);
        assert_eq!(recreated.paths.len(), 2);
        drop((recreated, removed, memory, base));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn memory_delta_rejects_invalid_inputs_and_id_exhaustion_without_mutating_base() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write(&root, "base.rs", "alpha beta\n");
        crate::index::build::build_index_with_progress(&root, true, true).unwrap();
        let mut base = IndexReader::open(&root).unwrap();
        let generation = base.generation_path().to_path_buf();
        let good = processed(&root, "base.rs");
        for path in ["", "../escape.rs", "/absolute.rs", "nested/../escape.rs"] {
            let mut invalid = good.clone();
            invalid.rel_path = path.into();
            assert!(base.with_memory_delta(vec![invalid], &[]).is_err());
            assert!(base.with_memory_delta(vec![], &[path.into()]).is_err());
        }
        assert!(
            base.with_memory_delta(vec![good.clone(), good.clone()], &[])
                .is_err()
        );
        let mut invalid = good.clone();
        invalid
            .token_positions
            .push((invalid.tokens.len() as u32, 0));
        assert!(base.with_memory_delta(vec![invalid], &[]).is_err());
        let mut invalid = good.clone();
        invalid.tokens.push("x".repeat(u16::MAX as usize + 1));
        assert!(base.with_memory_delta(vec![invalid], &[]).is_err());
        let mut invalid = good.clone();
        invalid.flags.set_tombstone();
        assert!(base.with_memory_delta(vec![invalid], &[]).is_err());
        base.documents[0].doc_id = u32::MAX;
        assert!(base.with_memory_delta(vec![good.clone()], &[]).is_err());
        base.documents[0].doc_id = 1;
        Arc::get_mut(&mut base.segments[0]).unwrap().segment_id = u16::MAX;
        assert!(base.with_memory_delta(vec![good], &[]).is_err());
        assert_eq!(crate::utils::get_index_dir(&root).unwrap(), generation);
        assert_eq!(IndexReader::open(&root).unwrap().valid_doc_ids().len(), 1);
        drop(base);
        crate::utils::remove_index(&root).unwrap();
    }
}
