//! Optional generation-wide, checked gram-to-segment routing evidence.
//!
//! A strictly validated publisher derives this complete dictionary afresh. Its
//! pages are independent authoritative evidence for literal absence: a negative
//! proof does not depend on the original segment dictionaries, checks or Blooms.
//! Thus damage to skipped segment evidence may coexist with a correct empty
//! answer in this separately opted-in mode. Full integrity checking still reads
//! every routing page and every segment. This is accidental-damage detection,
//! not authentication against coordinated rewriting or proof of source freshness.
//! Published files must remain immutable while the generation lease is held.

use super::reader::MappedBytes;
use super::types::{IndexMeta, SegmentId};
use ahash::AHashSet;
use anyhow::{Context, Result, ensure};
use std::path::Path;
use xxhash_rust::xxh3::xxh3_64;

const NAME: &str = "generation-routing.bin";
const MAGIC: &[u8; 8] = b"FXIGROU1";
const EPOCH: u32 = 1;
const HEADER: usize = 80;
const PAGE_ENTRIES: usize = 512;
const PAGE_RECORD: usize = 16;
const GRAM_UNIVERSE: usize = 1 << 24;

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub(crate) fn requested() -> bool {
    super::query_local::requested()
        && std::env::var_os("FXI_GENERATION_ROUTING").is_some_and(|value| value == "1")
}

// Sparse radix directory over the fixed 24-bit gram universe. Twelve high bits
// select a branch; the remaining two groups of six bits select a leaf and row.
// This avoids hashing each segment occurrence and sorting the final dictionary.
// Only populated branches/leaves allocate storage, keeping small builds small.
// All sentinel/index casts are bounded by the 24-bit universe validated by add().
struct GramRows {
    directory: Vec<u32>,
    branches: Vec<[u32; 64]>,
    leaves: Vec<[u32; 64]>,
    len: usize,
}

impl GramRows {
    fn new() -> Self {
        Self {
            directory: vec![u32::MAX; 4096],
            branches: Vec::new(),
            leaves: Vec::new(),
            len: 0,
        }
    }

    fn insert(&mut self, gram: u32) -> (usize, bool) {
        let gram = gram as usize;
        let branch = &mut self.directory[gram >> 12];
        if *branch == u32::MAX {
            *branch = self.branches.len() as u32;
            self.branches.push([u32::MAX; 64]);
        }
        let leaf = &mut self.branches[*branch as usize][(gram >> 6) & 63];
        if *leaf == u32::MAX {
            *leaf = self.leaves.len() as u32;
            self.leaves.push([u32::MAX; 64]);
        }
        let row = &mut self.leaves[*leaf as usize][gram & 63];
        let added = *row == u32::MAX;
        if added {
            *row = self.len as u32;
            self.len += 1;
        }
        (*row as usize, added)
    }

    fn row(&self, gram: u32) -> usize {
        let gram = gram as usize;
        let branch = self.directory[gram >> 12] as usize;
        let leaf = self.branches[branch][(gram >> 6) & 63] as usize;
        self.leaves[leaf][gram & 63] as usize
    }

    fn ordered_keys(&self) -> Vec<u32> {
        let mut keys = Vec::with_capacity(self.len);
        for (high, &branch) in self.directory.iter().enumerate() {
            if branch == u32::MAX {
                continue;
            }
            for (middle, &leaf) in self.branches[branch as usize].iter().enumerate() {
                if leaf == u32::MAX {
                    continue;
                }
                for (low, &row) in self.leaves[leaf as usize].iter().enumerate() {
                    if row != u32::MAX {
                        keys.push(((high << 12) | (middle << 6) | low) as u32);
                    }
                }
            }
        }
        keys
    }
}

/// Each `add` input must have passed strict segment validation, including posting
/// membership, before it reaches this builder. The caller also strictly validates
/// the complete generation's document/path tables before `write`.
pub(crate) struct Builder {
    segment_ids: Vec<SegmentId>,
    added: usize,
    mask_words: usize,
    rows: GramRows,
    // A single arena avoids one allocation per distinct gram.
    masks: Vec<u64>,
}

impl Builder {
    pub(crate) fn new(segment_ids: &[SegmentId]) -> Result<Self> {
        let mut seen = AHashSet::new();
        ensure!(
            segment_ids.len() <= usize::from(u16::MAX) + 1
                && segment_ids.iter().all(|id| seen.insert(*id)),
            "Duplicate or excessive generation routing segments"
        );
        Ok(Self {
            segment_ids: segment_ids.to_vec(),
            added: 0,
            mask_words: segment_ids.len().div_ceil(64),
            rows: GramRows::new(),
            masks: Vec::new(),
        })
    }

    pub(crate) fn add(&mut self, segment_id: SegmentId, dictionary: &[u8]) -> Result<()> {
        ensure!(
            self.segment_ids.get(self.added) == Some(&segment_id),
            "Generation routing segment order/coverage mismatch"
        );
        ensure!(dictionary.len() >= 4, "Truncated routing source dictionary");
        let count = u32_at(dictionary, 0) as usize;
        ensure!(
            dictionary.len() - 4 == count.saturating_mul(20),
            "Invalid routing source dictionary length"
        );
        let word = self.added / 64;
        let bit = 1u64 << (self.added % 64);
        let mut previous = None;
        for record in dictionary[4..].as_chunks::<20>().0 {
            let gram = u32_at(record, 0);
            ensure!(
                gram < GRAM_UNIVERSE as u32 && previous.is_none_or(|old| old < gram),
                "Invalid routing source dictionary ordering"
            );
            previous = Some(gram);
            let (row, added) = self.rows.insert(gram);
            let offset = row * self.mask_words;
            if added {
                self.masks.resize(offset + self.mask_words, 0);
            }
            self.masks[offset + word] |= bit;
        }
        self.added += 1;
        Ok(())
    }

    /// Always rebuild from the supplied strictly validated dictionaries. An older
    /// routing artifact is neither inherited nor used to bless source evidence;
    /// replacing it repairs a derived index from independently validated inputs.
    pub(crate) fn write(self, index: &Path) -> Result<()> {
        ensure!(
            self.added == self.segment_ids.len(),
            "Incomplete generation routing segment coverage"
        );
        let metadata = std::fs::read(index.join("meta.json"))?;
        let meta: IndexMeta = serde_json::from_slice(&metadata)?;
        meta.validate_format()?;
        ensure!(
            meta.base_segment
                .into_iter()
                .chain(meta.delta_segments.iter().copied())
                .eq(self.segment_ids.iter().copied()),
            "Generation routing metadata coverage mismatch"
        );
        let grams = self.rows.ordered_keys();
        let pages = grams.len().div_ceil(PAGE_ENTRIES);
        let directory = HEADER + self.segment_ids.len() * 2;
        let root_end = directory + pages * PAGE_RECORD;
        let record_bytes = 4 + self.mask_words * 8;
        let total = grams
            .len()
            .checked_mul(record_bytes)
            .and_then(|len| len.checked_add(root_end))
            .context("Generation routing size overflow")?;
        let mut bytes = vec![0u8; total];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[16..20].copy_from_slice(&EPOCH.to_le_bytes());
        bytes[20..24].copy_from_slice(&(self.segment_ids.len() as u32).to_le_bytes());
        bytes[24..28].copy_from_slice(&(grams.len() as u32).to_le_bytes());
        bytes[28..32].copy_from_slice(&(PAGE_ENTRIES as u32).to_le_bytes());
        bytes[32..40].copy_from_slice(&(metadata.len() as u64).to_le_bytes());
        bytes[40..48].copy_from_slice(&xxh3_64(&metadata).to_le_bytes());
        for (slot, name) in [(48, "docs.bin"), (64, "paths.bin")] {
            let contents = MappedBytes::open(&index.join(name))?;
            bytes[slot..slot + 8].copy_from_slice(&(contents.len() as u64).to_le_bytes());
            bytes[slot + 8..slot + 16].copy_from_slice(&xxh3_64(&contents).to_le_bytes());
        }
        for (slot, id) in self.segment_ids.iter().enumerate() {
            let at = HEADER + slot * 2;
            bytes[at..at + 2].copy_from_slice(&id.to_le_bytes());
        }
        for (slot, gram) in grams.iter().enumerate() {
            let at = root_end + slot * record_bytes;
            bytes[at..at + 4].copy_from_slice(&gram.to_le_bytes());
            let offset = self.rows.row(*gram) * self.mask_words;
            for (word, bits) in self.masks[offset..offset + self.mask_words]
                .iter()
                .enumerate()
            {
                let at = at + 4 + word * 8;
                bytes[at..at + 8].copy_from_slice(&bits.to_le_bytes());
            }
        }
        for page in 0..pages {
            let first = page * PAGE_ENTRIES;
            let end = (first + PAGE_ENTRIES).min(grams.len());
            let hash =
                xxh3_64(&bytes[root_end + first * record_bytes..root_end + end * record_bytes]);
            let at = directory + page * PAGE_RECORD;
            bytes[at..at + 4].copy_from_slice(&grams[first].to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&grams[end - 1].to_le_bytes());
            bytes[at + 8..at + 16].copy_from_slice(&hash.to_le_bytes());
        }
        let hash = xxh3_64(&bytes[16..root_end]);
        bytes[8..16].copy_from_slice(&hash.to_le_bytes());
        std::fs::write(index.join(NAME), bytes)?;
        Ok(())
    }
}

struct Routes {
    bytes: MappedBytes,
    segment_count: usize,
    gram_count: usize,
    mask_words: usize,
    record_bytes: usize,
    directory: usize,
    root_end: usize,
}

impl Routes {
    fn open(index: &Path) -> Result<Self> {
        let bytes = MappedBytes::open(&index.join(NAME))?;
        ensure!(
            bytes.len() >= HEADER && &bytes[..8] == MAGIC,
            "Invalid generation routing header"
        );
        ensure!(
            u32_at(&bytes, 16) == EPOCH && u32_at(&bytes, 28) == PAGE_ENTRIES as u32,
            "Unsupported generation routing epoch/page size"
        );
        let segment_count = u32_at(&bytes, 20) as usize;
        let gram_count = u32_at(&bytes, 24) as usize;
        ensure!(
            segment_count <= usize::from(u16::MAX) + 1
                && gram_count <= GRAM_UNIVERSE
                && (segment_count != 0 || gram_count == 0),
            "Invalid generation routing counts"
        );
        let mask_words = segment_count.div_ceil(64);
        let record_bytes = 4 + mask_words * 8;
        let directory = HEADER + segment_count * 2;
        let pages = gram_count.div_ceil(PAGE_ENTRIES);
        let root_end = directory + pages * PAGE_RECORD;
        let expected_len = gram_count
            .checked_mul(record_bytes)
            .and_then(|len| len.checked_add(root_end))
            .context("Generation routing length overflow")?;
        // Check actual file bounds before allocating from any on-disk count.
        ensure!(
            bytes.len() == expected_len,
            "Generation routing size mismatch"
        );
        ensure!(
            u64_at(&bytes, 8) == xxh3_64(&bytes[16..root_end]),
            "Generation routing root checksum mismatch"
        );
        let mut seen = AHashSet::with_capacity(segment_count);
        ensure!(
            bytes[HEADER..directory]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|id| seen.insert(u16::from_le_bytes(*id))),
            "Duplicate generation routing segments"
        );
        let mut previous = None;
        for page in bytes[directory..root_end].as_chunks::<PAGE_RECORD>().0 {
            let first = u32_at(page, 0);
            let last = u32_at(page, 4);
            ensure!(
                first <= last
                    && last < GRAM_UNIVERSE as u32
                    && previous.is_none_or(|old| old < first),
                "Invalid generation routing page directory"
            );
            previous = Some(last);
        }
        Ok(Self {
            bytes,
            segment_count,
            gram_count,
            mask_words,
            record_bytes,
            directory,
            root_end,
        })
    }

    fn check_bytes(&self, contents: &[u8], slot: usize) -> Result<()> {
        ensure!(
            contents.len() as u64 == u64_at(&self.bytes, slot)
                && xxh3_64(contents) == u64_at(&self.bytes, slot + 8),
            "Generation routing core content mismatch"
        );
        Ok(())
    }

    fn metadata(&self, index: &Path) -> Result<IndexMeta> {
        let bytes = std::fs::read(index.join("meta.json"))?;
        self.check_bytes(&bytes, 32)?;
        let meta: IndexMeta = serde_json::from_slice(&bytes)?;
        meta.validate_format()?;
        ensure!(
            meta.base_segment
                .into_iter()
                .chain(meta.delta_segments.iter().copied())
                .eq(self.bytes[HEADER..self.directory]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|id| u16::from_le_bytes(*id))),
            "Generation routing metadata coverage mismatch"
        );
        Ok(meta)
    }

    fn check_core_tables(&self, index: &Path) -> Result<()> {
        for (slot, name) in [(48, "docs.bin"), (64, "paths.bin")] {
            self.check_bytes(&MappedBytes::open(&index.join(name))?, slot)?;
        }
        Ok(())
    }

    fn page(&self, number: usize) -> Result<&[u8]> {
        let directory = self.directory + number * PAGE_RECORD;
        let first = number * PAGE_ENTRIES;
        let end = (first + PAGE_ENTRIES).min(self.gram_count);
        let records = &self.bytes
            [self.root_end + first * self.record_bytes..self.root_end + end * self.record_bytes];
        ensure!(
            xxh3_64(records) == u64_at(&self.bytes, directory + 8),
            "Generation routing page checksum mismatch"
        );
        let mut previous = None;
        for record in records.chunks_exact(self.record_bytes) {
            let gram = u32_at(record, 0);
            ensure!(
                gram < GRAM_UNIVERSE as u32 && previous.is_none_or(|old| old < gram),
                "Invalid generation routing page ordering"
            );
            previous = Some(gram);
            ensure!(
                record[4..].iter().any(|byte| *byte != 0),
                "Empty routing mask"
            );
            let used = self.segment_count % 64;
            if used != 0 {
                ensure!(
                    u64_at(record, self.record_bytes - 8) >> used == 0,
                    "Invalid generation routing mask bits"
                );
            }
        }
        ensure!(
            u32_at(records, 0) == u32_at(&self.bytes, directory)
                && previous == Some(u32_at(&self.bytes, directory + 4)),
            "Generation routing page boundaries mismatch"
        );
        Ok(records)
    }

    fn lookup(&self, gram: u32) -> Result<Option<&[u8]>> {
        let directory = self.bytes[self.directory..self.root_end]
            .as_chunks::<PAGE_RECORD>()
            .0;
        let page = directory.partition_point(|record| u32_at(record, 4) < gram);
        if page == directory.len() || gram < u32_at(&directory[page], 0) {
            return Ok(None);
        }
        // Keys and masks must be checked before binary search can prove absence.
        let records = self.page(page)?;
        let mut lo = 0;
        let mut hi = records.len() / self.record_bytes;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let at = mid * self.record_bytes;
            match u32_at(records, at).cmp(&gram) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return Ok(Some(&records[at + 4..at + self.record_bytes]));
                }
            }
        }
        Ok(None)
    }
}

/// Complete optional-artifact validation for strict integrity checks. Missing
/// evidence is compatible with older generations; malformed evidence is an error.
pub(crate) fn validate(index: &Path) -> Result<()> {
    let routes = match Routes::open(index) {
        Ok(routes) => routes,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    routes.metadata(index)?;
    routes.check_core_tables(index)?;
    for number in 0..routes.gram_count.div_ceil(PAGE_ENTRIES) {
        routes.page(number)?;
    }
    Ok(())
}

/// A proof failure is never an empty result: the caller falls back to the normal
/// checked reader. A successful result is tied to one leased immutable generation.
#[allow(dead_code)] // The CLI and library compile this module separately.
pub(crate) fn preflight(root: &Path, query: &crate::query::Query) -> Option<IndexMeta> {
    if !requested() {
        return None;
    }
    let literal = super::negative_routing::exact_literal(query)?;
    let (index, _lease) = super::generation::pin(root).ok()?;
    prove_absent(&index, &literal).ok().flatten()
}

fn prove_absent(index: &Path, literal: &[u8]) -> Result<Option<IndexMeta>> {
    let routes = Routes::open(index)?;
    let meta = routes.metadata(index)?;
    let stop: AHashSet<_> = meta.stop_grams.iter().copied().collect();
    let mut grams: Vec<_> = literal
        .windows(3)
        .map(|gram| super::types::bytes_to_trigram(gram[0], gram[1], gram[2]))
        .filter(|gram| !stop.contains(gram))
        .collect();
    grams.sort_unstable();
    grams.dedup();
    if grams.is_empty() {
        return Ok(None);
    }
    let mut possible = vec![u64::MAX; routes.mask_words];
    if let Some(last) = possible.last_mut() {
        let used = routes.segment_count % 64;
        if used != 0 {
            *last = (1u64 << used) - 1;
        }
    }
    for gram in grams {
        let Some(mask) = routes.lookup(gram)? else {
            possible.fill(0);
            break;
        };
        for (word, possible) in possible.iter_mut().enumerate() {
            *possible &= u64_at(mask, word * 8);
        }
        if possible.iter().all(|word| *word == 0) {
            break;
        }
    }
    if possible.iter().any(|word| *word != 0) {
        return Ok(None);
    }
    routes.check_core_tables(index)?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dictionary(grams: &[u32]) -> Vec<u8> {
        let mut bytes = (grams.len() as u32).to_le_bytes().to_vec();
        for gram in grams {
            bytes.extend_from_slice(&gram.to_le_bytes());
            bytes.extend_from_slice(&[0u8; 16]);
        }
        bytes
    }

    fn grams(literal: &[u8]) -> Vec<u32> {
        let mut grams: Vec<_> = literal
            .windows(3)
            .map(|gram| super::super::types::bytes_to_trigram(gram[0], gram[1], gram[2]))
            .collect();
        grams.sort_unstable();
        grams.dedup();
        grams
    }

    fn fixture(keys: &[Vec<u32>], stop_grams: Vec<u32>) -> tempfile::TempDir {
        let index = tempfile::tempdir().unwrap();
        let ids: Vec<_> = (0..keys.len()).map(|id| id as u16).collect();
        let meta = IndexMeta {
            delta_segments: ids.clone(),
            segment_count: keys.len() as u16,
            stop_grams,
            ..IndexMeta::default()
        };
        fs::write(
            index.path().join("meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        for name in ["docs.bin", "paths.bin"] {
            fs::write(index.path().join(name), 0u32.to_le_bytes()).unwrap();
        }
        let mut builder = Builder::new(&ids).unwrap();
        for (id, keys) in ids.into_iter().zip(keys) {
            builder.add(id, &dictionary(keys)).unwrap();
        }
        builder.write(index.path()).unwrap();
        index
    }

    #[test]
    fn radix_rows_preserve_identity_order_and_sparse_allocation() {
        let mut rows = GramRows::new();
        assert!(rows.branches.is_empty() && rows.leaves.is_empty());
        assert_eq!(rows.insert(0), (0, true));
        assert_eq!(rows.insert(0), (0, false));
        assert_eq!((rows.branches.len(), rows.leaves.len()), (1, 1));
        let keys: std::collections::BTreeSet<_> = (0..GRAM_UNIVERSE as u32)
            .step_by(1023)
            .chain([0, 63, 64, 4095, 4096, 0x00ff_ffff])
            .collect();
        let mut expected = std::collections::BTreeMap::from([(0, 0)]);
        for &gram in keys.iter().rev() {
            let next = expected.len();
            let wanted = *expected.entry(gram).or_insert(next);
            assert_eq!(rows.insert(gram).0, wanted);
            assert_eq!(rows.insert(gram), (wanted, false));
        }
        assert_eq!(rows.ordered_keys(), keys.into_iter().collect::<Vec<_>>());
        for (gram, row) in expected {
            assert_eq!(rows.row(gram), row);
        }
    }

    #[test]
    fn routing_builder_matches_independent_masks_across_radix_and_word_boundaries() {
        let mut expected = std::collections::BTreeMap::<u32, Vec<u64>>::new();
        let mut keys = Vec::new();
        let mut random = 0x92ab_15c7u32;
        for segment in 0..129 {
            let mut grams = std::collections::BTreeSet::new();
            for _ in 0..64 {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                grams.insert(random >> 8);
            }
            for (offset, gram) in [0, 63, 64, 4095, 4096, 0x00ff_ffff].into_iter().enumerate() {
                if (segment + offset) % 3 != 0 {
                    grams.insert(gram);
                }
            }
            for &gram in &grams {
                expected.entry(gram).or_insert_with(|| vec![0; 3])[segment / 64] |=
                    1u64 << (segment % 64);
            }
            keys.push(grams.into_iter().collect());
        }
        let index = fixture(&keys, Vec::new());
        let routes = Routes::open(index.path()).unwrap();
        let actual_keys: Vec<_> = (0..routes.gram_count)
            .map(|row| u32_at(&routes.bytes, routes.root_end + row * routes.record_bytes))
            .collect();
        assert_eq!(actual_keys, expected.keys().copied().collect::<Vec<_>>());
        for (gram, expected_mask) in expected {
            let actual = routes.lookup(gram).unwrap().unwrap();
            for (word, mask) in expected_mask.into_iter().enumerate() {
                assert_eq!(u64_at(actual, word * 8), mask, "gram {gram}, word {word}");
            }
        }
        validate(index.path()).unwrap();
    }

    #[test]
    fn masks_cover_zero_one_64_and_65_segments() {
        for segments in [0, 1, 64, 65] {
            let mut keys = vec![Vec::new(); segments];
            if let Some(last) = keys.last_mut() {
                *last = grams(b"xyz");
            }
            let index = fixture(&keys, Vec::new());
            validate(index.path()).unwrap();
            assert!(prove_absent(index.path(), b"absent").unwrap().is_some());
            assert_eq!(
                prove_absent(index.path(), b"xyz").unwrap().is_none(),
                segments > 0
            );
            if segments > 0 {
                let routes = Routes::open(index.path()).unwrap();
                let mask = routes.lookup(grams(b"xyz")[0]).unwrap().unwrap();
                for word in 0..routes.mask_words {
                    assert_eq!(
                        u64_at(mask, word * 8),
                        if word == (segments - 1) / 64 {
                            1 << ((segments - 1) % 64)
                        } else {
                            0
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn conjunction_rejects_even_when_every_gram_occurs_globally() {
        let keys = vec![grams(b"abc"), grams(b"bcd")];
        let index = fixture(&keys, Vec::new());
        assert!(prove_absent(index.path(), b"abcd").unwrap().is_some());
        for literal in [b"abc", b"bcd"] {
            assert!(prove_absent(index.path(), literal).unwrap().is_none());
        }
        let stopped = fixture(&keys, grams(b"abcd"));
        assert!(prove_absent(stopped.path(), b"abcd").unwrap().is_none());
        assert!(prove_absent(index.path(), b"ab").unwrap().is_none());
    }

    #[test]
    fn pages_cover_boundaries_internal_absence_and_directory_gaps() {
        let keys: Vec<_> = (0..1025).map(|index| 1000 + index * 2).collect();
        let index = fixture(std::slice::from_ref(&keys), Vec::new());
        let routes = Routes::open(index.path()).unwrap();
        for gram in keys {
            assert!(routes.lookup(gram).unwrap().is_some());
            assert!(routes.lookup(gram + 1).unwrap().is_none());
        }
        assert!(routes.lookup(999).unwrap().is_none());
        validate(index.path()).unwrap();
    }

    fn rehash_page_and_root(bytes: &mut [u8], page: usize) {
        let segments = u32_at(bytes, 20) as usize;
        let count = u32_at(bytes, 24) as usize;
        let directory = HEADER + segments * 2;
        let root_end = directory + count.div_ceil(PAGE_ENTRIES) * PAGE_RECORD;
        let record_bytes = 4 + segments.div_ceil(64) * 8;
        let first = page * PAGE_ENTRIES;
        let end = (first + PAGE_ENTRIES).min(count);
        let hash = xxh3_64(&bytes[root_end + first * record_bytes..root_end + end * record_bytes]);
        let at = directory + page * PAGE_RECORD + 8;
        bytes[at..at + 8].copy_from_slice(&hash.to_le_bytes());
        let hash = xxh3_64(&bytes[16..root_end]);
        bytes[8..16].copy_from_slice(&hash.to_le_bytes());
    }

    #[test]
    fn damaged_root_selected_page_and_lengths_are_rejected() {
        let index = fixture(&[grams(b"abc")], Vec::new());
        let path = index.path().join(NAME);
        let original = fs::read(&path).unwrap();
        for at in [
            0,
            8,
            16,
            20,
            24,
            28,
            40,
            HEADER,
            HEADER + 2,
            original.len() - 1,
        ] {
            let mut changed = original.clone();
            changed[at] ^= 1;
            fs::write(&path, changed).unwrap();
            assert!(prove_absent(index.path(), b"abc").is_err(), "byte {at}");
        }
        for len in [0, HEADER - 1, HEADER, original.len() - 1] {
            fs::write(&path, &original[..len]).unwrap();
            assert!(Routes::open(index.path()).is_err(), "length {len}");
        }
        let mut changed = original.clone();
        changed[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, changed).unwrap();
        assert!(Routes::open(index.path()).is_err());
        fs::write(path, original).unwrap();
        validate(index.path()).unwrap();
    }

    #[test]
    fn unused_mask_bits_and_empty_masks_fail_even_with_matching_checksums() {
        let keys = vec![grams(b"abc"); 65];
        let index = fixture(&keys, Vec::new());
        let path = index.path().join(NAME);
        let original = fs::read(&path).unwrap();
        let root_end = Routes::open(index.path()).unwrap().root_end;
        let mut changed = original.clone();
        changed[root_end + 12] |= 2; // Only bit zero is valid in the second word.
        rehash_page_and_root(&mut changed, 0);
        fs::write(&path, changed).unwrap();
        assert!(prove_absent(index.path(), b"abc").is_err());
        let mut changed = original;
        changed[root_end + 4..].fill(0);
        rehash_page_and_root(&mut changed, 0);
        fs::write(path, changed).unwrap();
        assert!(prove_absent(index.path(), b"abc").is_err());
    }

    #[test]
    fn actual_metadata_documents_and_paths_are_bound() {
        let index = fixture(&[grams(b"abc")], Vec::new());
        for name in ["meta.json", "docs.bin", "paths.bin"] {
            let path = index.path().join(name);
            let original = fs::read(&path).unwrap();
            let mut changed = original.clone();
            if name == "meta.json" {
                changed.push(b' ');
            } else {
                changed[0] ^= 1;
            }
            fs::write(&path, changed).unwrap();
            assert!(prove_absent(index.path(), b"xyz").is_err(), "{name}");
            fs::write(path, original).unwrap();
        }
    }

    #[test]
    fn independent_evidence_skips_unrelated_segments_and_routing_pages() {
        let keys: Vec<_> = (0..1025).map(|index| 1000 + index * 2).collect();
        let index = fixture(&[keys], Vec::new());
        let segment = index.path().join("segments/seg_0000");
        fs::create_dir_all(&segment).unwrap();
        for name in ["grams.checks", "grams.dict", "grams.postings", "bloom.bin"] {
            fs::write(segment.join(name), b"unrelated corruption").unwrap();
        }
        assert!(prove_absent(index.path(), b"xyz").unwrap().is_some());
        let path = index.path().join(NAME);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        let routes = Routes::open(index.path()).unwrap();
        assert!(routes.lookup(1000).unwrap().is_some());
        assert!(routes.lookup(3048).is_err());
        assert!(prove_absent(index.path(), b"xyz").unwrap().is_some());
        assert!(validate(index.path()).is_err());
    }

    #[test]
    fn builder_requires_exact_complete_ordered_segment_coverage() {
        assert!(Builder::new(&[1, 1]).is_err());
        let mut builder = Builder::new(&[1, 2]).unwrap();
        assert!(builder.add(2, &dictionary(&[])).is_err());
        builder.add(1, &dictionary(&[])).unwrap();
        assert!(builder.add(1, &dictionary(&[])).is_err());
        let index = tempfile::tempdir().unwrap();
        assert!(builder.write(index.path()).is_err());
        assert!(validate(index.path()).is_ok());
    }
}
