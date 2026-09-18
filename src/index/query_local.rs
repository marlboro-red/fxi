//! Experimental checked dictionary / query-local posting validation.
//!
//! Checksums detect accidental damage, not coherent rewriting by an untrusted
//! issuer. Metadata keeps its existing structural checks. Published mappings
//! must remain immutable for the reader's lifetime, as with the ordinary reader.
use super::reader::MappedBytes;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use xxhash_rust::xxh3::xxh3_64;

const NAME: &str = "grams.checks";
const MAGIC: &[u8; 8] = b"FXIGRAM1";
const PAGED_MAGIC: &[u8; 8] = b"FXIGRAM2";
const PAGE_ENTRIES: usize = 512;
const ROOT_HEADER: usize = 32;
const PAGE_RECORD: usize = 24;

struct Page {
    first: u32,
    last: u32,
    dictionary_hash: u64,
    postings_hash: u64,
    validated: AtomicU8,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
const HEADER: usize = 40;

pub(crate) fn requested() -> bool {
    std::env::var_os("FXI_QUERY_LOCAL").is_some_and(|v| v == "1")
}

pub(crate) struct PostingChecks {
    bytes: MappedBytes,
    hashes_offset: usize,
    pages: Option<Vec<Page>>,
    // Only successful checks are cached. Invalid payloads stay errors; concurrent
    // first readers may duplicate validation rather than synchronizing a lock.
    validated: Vec<AtomicU8>,
}

// Validate routing evidence without allocating per-posting or page cache state.
fn checked_root(bytes: &[u8], posting_len: usize, count: usize) -> Result<usize> {
    let page_count = count.div_ceil(PAGE_ENTRIES);
    let root_end = page_count
        .checked_mul(PAGE_RECORD)
        .and_then(|n| n.checked_add(ROOT_HEADER))
        .context("Gram root size overflow")?;
    let expected_len = count
        .checked_mul(8)
        .and_then(|n| n.checked_add(root_end))
        .context("Gram checks size overflow")?;
    anyhow::ensure!(bytes.len() == expected_len, "Gram checks size mismatch");
    anyhow::ensure!(
        u64_at(bytes, 8) == xxh3_64(&bytes[16..root_end]),
        "Gram root checksum mismatch"
    );
    anyhow::ensure!(
        u64_at(bytes, 16) == posting_len as u64 && u64_at(bytes, 24) == count as u64,
        "Gram checks length/count mismatch"
    );
    let mut previous = None;
    for entry in bytes[ROOT_HEADER..root_end].as_chunks::<PAGE_RECORD>().0 {
        let first = u32_at(entry, 0);
        let last = u32_at(entry, 4);
        anyhow::ensure!(
            first <= last && last <= 0x00ff_ffff && previous.is_none_or(|p| p < first),
            "Invalid gram page directory"
        );
        previous = Some(last);
    }
    Ok(root_end)
}

impl PostingChecks {
    pub(crate) fn open(
        path: &Path,
        dictionary: &[u8],
        posting_len: usize,
        count: usize,
    ) -> Result<Option<Self>> {
        let name = path.join(NAME);
        let bytes = match MappedBytes::open(&name) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if bytes.len() >= 8 && &bytes[..8] == PAGED_MAGIC {
            return Self::open_paged(bytes, posting_len, count).map(Some);
        }
        anyhow::ensure!(
            bytes.len() >= HEADER && &bytes[..8] == MAGIC,
            "Invalid gram checks header"
        );
        let read = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        anyhow::ensure!(
            read(8) == xxh3_64(&bytes[16..]),
            "Gram checks checksum mismatch"
        );
        anyhow::ensure!(
            read(16) == xxh3_64(dictionary),
            "Gram dictionary checksum mismatch"
        );
        anyhow::ensure!(
            read(24) == posting_len as u64,
            "Gram posting length mismatch"
        );
        anyhow::ensure!(
            read(32) == count as u64
                && (bytes.len() - HEADER) / 8 == count
                && (bytes.len() - HEADER).is_multiple_of(8),
            "Gram checks count mismatch"
        );
        Ok(Some(Self {
            bytes,
            hashes_offset: HEADER,
            pages: None,
            validated: (0..count).map(|_| AtomicU8::new(0)).collect(),
        }))
    }

    fn open_paged(bytes: MappedBytes, posting_len: usize, count: usize) -> Result<Self> {
        let root_end = checked_root(&bytes, posting_len, count)?;
        let pages = bytes[ROOT_HEADER..root_end]
            .as_chunks::<PAGE_RECORD>()
            .0
            .iter()
            .map(|entry| Page {
                first: u32_at(entry, 0),
                last: u32_at(entry, 4),
                dictionary_hash: u64_at(entry, 8),
                postings_hash: u64_at(entry, 16),
                validated: AtomicU8::new(0),
            })
            .collect();
        Ok(Self {
            bytes,
            hashes_offset: root_end,
            pages: Some(pages),
            validated: (0..count).map(|_| AtomicU8::new(0)).collect(),
        })
    }

    pub(crate) fn is_paged(&self) -> bool {
        self.pages.is_some()
    }

    fn validate_page(
        &self,
        page_index: usize,
        dictionary: &[u8],
        posting_len: usize,
    ) -> Result<std::ops::Range<usize>> {
        let page = &self.pages.as_ref().expect("paged checks")[page_index];
        let start = page_index * PAGE_ENTRIES;
        let end = (start + PAGE_ENTRIES).min(self.validated.len());
        if page.validated.load(Ordering::Acquire) != 0 {
            return Ok(start..end);
        }
        let records = &dictionary[4 + start * 20..4 + end * 20];
        anyhow::ensure!(
            xxh3_64(records) == page.dictionary_hash,
            "Gram dictionary page checksum mismatch"
        );
        anyhow::ensure!(
            xxh3_64(&self.bytes[self.hashes_offset + start * 8..self.hashes_offset + end * 8])
                == page.postings_hash,
            "Gram posting hash page checksum mismatch"
        );
        let mut previous = None;
        for entry in records.as_chunks::<20>().0 {
            let gram = u32_at(entry, 0);
            anyhow::ensure!(
                gram >= page.first && gram <= page.last && previous.is_none_or(|g| g < gram),
                "Invalid gram page ordering"
            );
            let offset = u64_at(entry, 4);
            let length = u32_at(entry, 12);
            anyhow::ensure!(
                offset
                    .checked_add(u64::from(length))
                    .is_some_and(|end| end <= posting_len as u64),
                "Invalid gram page posting range"
            );
            previous = Some(gram);
        }
        anyhow::ensure!(
            u32_at(records, 0) == page.first && previous == Some(page.last),
            "Gram page boundaries mismatch"
        );
        page.validated.store(1, Ordering::Release);
        Ok(start..end)
    }

    pub(crate) fn lookup_range(
        &self,
        gram: u32,
        dictionary: &[u8],
        posting_len: usize,
    ) -> Result<std::ops::Range<usize>> {
        let Some(pages) = &self.pages else {
            return Ok(0..self.validated.len());
        };
        let index = pages.partition_point(|page| page.last < gram);
        if index == pages.len() || gram < pages[index].first {
            return Ok(0..0);
        }
        self.validate_page(index, dictionary, posting_len)
    }

    pub(crate) fn validate_directory(&self, dictionary: &[u8], posting_len: usize) -> Result<()> {
        if let Some(pages) = &self.pages {
            for index in 0..pages.len() {
                self.validate_page(index, dictionary, posting_len)?;
            }
        }
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        index: usize,
        bytes: &[u8],
        frequency: u32,
        validator: &crate::utils::encoding::DocumentPostingsValidator<'_>,
    ) -> Result<()> {
        if self.validated[index].load(Ordering::Acquire) != 0 {
            return Ok(());
        }
        let at = self.hashes_offset + index * 8;
        let expected = u64::from_le_bytes(self.bytes[at..at + 8].try_into().unwrap());
        anyhow::ensure!(xxh3_64(bytes) == expected, "Gram posting checksum mismatch");
        validator.validate(bytes, frequency)?;
        self.validated[index].store(1, Ordering::Release);
        Ok(())
    }
}

/// Called only on a fully validated staging segment. Never rewrite inherited
/// evidence: a malformed existing sidecar must fail validation, not be blessed.
pub(crate) fn write(path: &Path, dictionary: &[u8], postings: &[u8]) -> Result<()> {
    if path.join(NAME).try_exists()? {
        return Ok(());
    }
    let count = u32::from_le_bytes(dictionary[..4].try_into().unwrap()) as usize;
    let page_count = count.div_ceil(PAGE_ENTRIES);
    let root_end = ROOT_HEADER + page_count * PAGE_RECORD;
    let mut bytes = vec![0u8; root_end];
    bytes[..8].copy_from_slice(PAGED_MAGIC);
    bytes[16..24].copy_from_slice(&(postings.len() as u64).to_le_bytes());
    bytes[24..32].copy_from_slice(&(count as u64).to_le_bytes());
    for record in dictionary[4..].as_chunks::<20>().0 {
        let start = usize::try_from(u64_at(record, 4))?;
        let length = u32_at(record, 12) as usize;
        let end = start.checked_add(length).context("Gram range overflow")?;
        let payload = postings.get(start..end).context("Invalid gram range")?;
        bytes.extend_from_slice(&xxh3_64(payload).to_le_bytes());
    }
    for page in 0..page_count {
        let start = page * PAGE_ENTRIES;
        let end = (start + PAGE_ENTRIES).min(count);
        let records = &dictionary[4 + start * 20..4 + end * 20];
        let hashes = &bytes[root_end + start * 8..root_end + end * 8];
        let dictionary_hash = xxh3_64(records);
        let postings_hash = xxh3_64(hashes);
        let at = ROOT_HEADER + page * PAGE_RECORD;
        bytes[at..at + 4].copy_from_slice(&records[..4]);
        bytes[at + 4..at + 8].copy_from_slice(&records[records.len() - 20..records.len() - 16]);
        bytes[at + 8..at + 16].copy_from_slice(&dictionary_hash.to_le_bytes());
        bytes[at + 16..at + 24].copy_from_slice(&postings_hash.to_le_bytes());
    }
    let checksum = xxh3_64(&bytes[16..root_end]);
    bytes[8..16].copy_from_slice(&checksum.to_le_bytes());
    std::fs::write(path.join(NAME), bytes)?;
    Ok(())
}

const BLOOM_PROOF: &str = "grams.bloom-check";
const BLOOM_PROOF_MAGIC: &[u8; 8] = b"FXIBLM01";

// The legacy rotating-XOR Bloom checksum has word-permutation collisions.
// Bind ordered content independently instead of treating it as integrity proof.
fn bloom_digest(bloom: &crate::utils::BloomFilter) -> u64 {
    let mut bytes = Vec::with_capacity(9 + bloom.bits().len() * 8);
    bytes.push(bloom.num_hashes());
    bytes.extend_from_slice(&(bloom.bits().len() as u64).to_le_bytes());
    for &word in bloom.bits() {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    xxh3_64(&bytes)
}

/// Optional acceleration only: malformed evidence disables pruning, so the
/// checked dictionary remains the fallback source of routing completeness.
impl PostingChecks {
    pub(crate) fn proves_bloom(&self, path: &Path, bloom: &crate::utils::BloomFilter) -> bool {
        let Ok(proof) = std::fs::read(path.join(BLOOM_PROOF)) else {
            return false;
        };
        proof.len() == 32
            && &proof[..8] == BLOOM_PROOF_MAGIC
            && u64_at(&proof, 24) == xxh3_64(&proof[..24])
            && u64_at(&proof, 8) == u64_at(&self.bytes, 8)
            && u64_at(&proof, 16) == bloom_digest(bloom)
    }
}

/// Bind a coverage-checked Bloom to this exact checked dictionary root. No file
/// stamps are involved: the reader hashes its actual root and loaded filter.
pub(crate) fn write_bloom_proof(
    path: &Path,
    dictionary: &[u8],
    bloom: Option<&crate::utils::BloomFilter>,
) -> Result<()> {
    if path.join(BLOOM_PROOF).try_exists()? {
        return Ok(());
    }
    let Some(bloom) = bloom else {
        return Ok(());
    };
    if !dictionary[4..]
        .as_chunks::<20>()
        .0
        .iter()
        .all(|record| bloom.might_contain(u32_at(record, 0)))
    {
        return Ok(());
    }
    // The caller strictly validated any inherited checks before arriving here.
    let checks = std::fs::read(path.join(NAME))?;
    anyhow::ensure!(checks.len() >= 16, "Truncated gram checks");
    let mut proof = BLOOM_PROOF_MAGIC.to_vec();
    proof.extend_from_slice(&checks[8..16]);
    proof.extend_from_slice(&bloom_digest(bloom).to_le_bytes());
    let hash = xxh3_64(&proof);
    proof.extend_from_slice(&hash.to_le_bytes());
    std::fs::write(path.join(BLOOM_PROOF), proof)?;
    Ok(())
}

const ROUTING_NAME: &str = "query-routing.bin";
const ROUTING_MAGIC: &[u8; 8] = b"FXIROUT1";

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct RoutingSegment {
    id: super::types::SegmentId,
    count: usize,
    posting_len: usize,
    root_hash: u64,
    bloom_hash: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RoutingManifest {
    epoch: u32,
    meta_hash: u64,
    docs_hash: u64,
    paths_hash: u64,
    segments: Vec<RoutingSegment>,
}

pub(crate) fn routing_segment(
    path: &Path,
    id: super::types::SegmentId,
    dictionary: &[u8],
    postings: &[u8],
    bloom: Option<&crate::utils::BloomFilter>,
) -> Result<Option<RoutingSegment>> {
    let Some(bloom) = bloom else {
        return Ok(None);
    };
    let count = u32_at(dictionary, 0) as usize;
    let Some(checks) = PostingChecks::open(path, dictionary, postings.len(), count)? else {
        return Ok(None);
    };
    if !checks.is_paged() || !checks.proves_bloom(path, bloom) {
        return Ok(None);
    }
    Ok(Some(RoutingSegment {
        id,
        count,
        posting_len: postings.len(),
        root_hash: u64_at(&checks.bytes, 8),
        bloom_hash: bloom_digest(bloom),
    }))
}

/// Issued after strict validation of this complete generation's core evidence.
pub(crate) fn write_routing_manifest(
    index: &Path,
    segments: Option<Vec<RoutingSegment>>,
) -> Result<()> {
    let Some(segments) = segments else {
        return Ok(());
    };
    let manifest = RoutingManifest {
        epoch: 1,
        meta_hash: xxh3_64(&std::fs::read(index.join("meta.json"))?),
        docs_hash: xxh3_64(&MappedBytes::open(&index.join("docs.bin"))?),
        paths_hash: xxh3_64(&MappedBytes::open(&index.join("paths.bin"))?),
        segments,
    };
    let payload = serde_json::to_vec(&manifest)?;
    let mut bytes = ROUTING_MAGIC.to_vec();
    bytes.extend_from_slice(&xxh3_64(&payload).to_le_bytes());
    bytes.extend_from_slice(&payload);
    std::fs::write(index.join(ROUTING_NAME), bytes)?;
    Ok(())
}

/// Narrow, optional negative proof. Invalid or missing proof falls back to the
/// ordinary checked reader; no proof failure is interpreted as an empty answer.
#[allow(dead_code)] // CLI entry point; also compiled into the public library.
pub(crate) fn preflight(
    root: &Path,
    query: &crate::query::Query,
) -> Option<super::types::IndexMeta> {
    if !requested() {
        return None;
    }
    let literal = super::negative_routing::exact_literal(query)?;
    let (index, _lease) = super::generation::pin(root).ok()?;
    prove_absent(&index, &literal).ok().flatten()
}

fn prove_absent(index: &Path, literal: &[u8]) -> Result<Option<super::types::IndexMeta>> {
    let bytes = std::fs::read(index.join(ROUTING_NAME))?;
    anyhow::ensure!(
        bytes.len() >= 16 && &bytes[..8] == ROUTING_MAGIC,
        "Invalid checked routing header"
    );
    anyhow::ensure!(
        u64_at(&bytes, 8) == xxh3_64(&bytes[16..]),
        "Checked routing checksum mismatch"
    );
    let manifest: RoutingManifest = serde_json::from_slice(&bytes[16..])?;
    anyhow::ensure!(manifest.epoch == 1, "Unsupported checked routing epoch");
    let metadata = std::fs::read(index.join("meta.json"))?;
    anyhow::ensure!(
        xxh3_64(&metadata) == manifest.meta_hash,
        "Checked routing metadata changed"
    );
    let meta: super::types::IndexMeta = serde_json::from_slice(&metadata)?;
    meta.validate_format()?;
    let ids: Vec<_> = meta
        .base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
        .collect();
    anyhow::ensure!(
        ids.iter()
            .copied()
            .eq(manifest.segments.iter().map(|s| s.id)),
        "Checked routing segment coverage mismatch"
    );
    let stop: std::collections::HashSet<_> = meta.stop_grams.iter().copied().collect();
    let mut grams: Vec<_> = literal
        .windows(3)
        .map(|g| super::types::bytes_to_trigram(g[0], g[1], g[2]))
        .filter(|g| !stop.contains(g))
        .collect();
    grams.sort_unstable();
    grams.dedup();
    if grams.is_empty() {
        return Ok(None);
    }
    for segment in manifest.segments {
        let path = index
            .join("segments")
            .join(format!("seg_{:04}", segment.id));
        let checks = MappedBytes::open(&path.join(NAME))?;
        anyhow::ensure!(
            checks.len() >= 8 && &checks[..8] == PAGED_MAGIC,
            "Checked routing requires paged grams"
        );
        // Hash/validate the actual root, not just its stored digest field.
        checked_root(&checks, segment.posting_len, segment.count)?;
        anyhow::ensure!(
            u64_at(&checks, 8) == segment.root_hash,
            "Checked routing root changed"
        );
        let mapped = MappedBytes::open(&path.join("bloom.bin"))?;
        let bloom = crate::utils::bloom::CertifiedBloomView::from_prevalidated_bytes(&mapped)
            .context("Invalid checked routing Bloom")?;
        anyhow::ensure!(
            bloom.checksum() == u64_at(&mapped, mapped.len() - 8),
            "Bloom checksum mismatch"
        );
        anyhow::ensure!(
            bloom.content_digest() == segment.bloom_hash,
            "Checked routing Bloom changed"
        );
        if bloom.might_contain_all(&grams) {
            return Ok(None);
        }
    }
    anyhow::ensure!(
        xxh3_64(&MappedBytes::open(&index.join("docs.bin"))?) == manifest.docs_hash,
        "Checked routing documents changed"
    );
    anyhow::ensure!(
        xxh3_64(&MappedBytes::open(&index.join("paths.bin"))?) == manifest.paths_hash,
        "Checked routing paths changed"
    );
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("one.rs", "needle alpha\n"),
            ("two.rs", "needle beta\n"),
            ("three.rs", "gamma\n"),
        ] {
            fs::write(root.path().join(name), text).unwrap();
        }
        crate::index::build::build_index_with_options(root.path(), true, true, Some(1)).unwrap();
        let index = crate::utils::get_index_dir(root.path()).unwrap();
        super::super::reader::write_query_local_checks(&index).unwrap();
        (root, index)
    }
    fn rewrite_manifest(index: &Path, edit: impl FnOnce(&mut RoutingManifest)) {
        let bytes = fs::read(index.join(ROUTING_NAME)).unwrap();
        let mut manifest: RoutingManifest = serde_json::from_slice(&bytes[16..]).unwrap();
        edit(&mut manifest);
        let payload = serde_json::to_vec(&manifest).unwrap();
        let mut bytes = ROUTING_MAGIC.to_vec();
        bytes.extend_from_slice(&xxh3_64(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        fs::write(index.join(ROUTING_NAME), bytes).unwrap();
    }

    #[test]
    fn checked_absence_handles_an_empty_generation() {
        let index = tempfile::tempdir().unwrap();
        let meta = super::super::types::IndexMeta::default();
        fs::write(
            index.path().join("meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        for name in ["docs.bin", "paths.bin"] {
            fs::write(index.path().join(name), 0u32.to_le_bytes()).unwrap();
        }
        super::super::reader::write_query_local_checks(index.path()).unwrap();
        assert!(prove_absent(index.path(), b"absent").unwrap().is_some());
    }

    #[test]
    fn checked_absence_binds_metadata_and_complete_segment_coverage() {
        let (_root, index) = fixture();
        let absent = b"zzzDefinitelyAbsent94283";
        assert!(prove_absent(&index, absent).unwrap().is_some());
        assert!(prove_absent(&index, b"needle").unwrap().is_none());
        assert!(prove_absent(&index, b"ab").unwrap().is_none());
        let original = fs::read(index.join(ROUTING_NAME)).unwrap();
        for mutation in 0..5 {
            rewrite_manifest(&index, |manifest| match mutation {
                0 => {
                    manifest.segments.pop();
                }
                1 => manifest.segments.swap(0, 1),
                2 => manifest.segments[1].id = manifest.segments[0].id,
                3 => manifest.epoch += 1,
                _ => manifest.epoch = 0,
            });
            assert!(prove_absent(&index, absent).is_err(), "mutation {mutation}");
            fs::write(index.join(ROUTING_NAME), &original).unwrap();
        }
        for file in ["meta.json", "docs.bin", "paths.bin"] {
            let path = index.join(file);
            let original = fs::read(&path).unwrap();
            let mut changed = original.clone();
            if file == "meta.json" {
                changed.push(b' ');
            } else {
                *changed.last_mut().unwrap() ^= 1;
            }
            fs::write(&path, changed).unwrap();
            assert!(prove_absent(&index, absent).is_err(), "{file}");
            fs::write(path, original).unwrap();
        }
        // Stop-gram-only queries cannot establish absence.
        let metadata = index.join("meta.json");
        let mut meta: super::super::types::IndexMeta =
            serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
        meta.stop_grams = absent
            .windows(3)
            .map(|g| super::super::types::bytes_to_trigram(g[0], g[1], g[2]))
            .collect();
        fs::write(&metadata, serde_json::to_vec(&meta).unwrap()).unwrap();
        rewrite_manifest(&index, |manifest| {
            manifest.meta_hash = xxh3_64(&fs::read(&metadata).unwrap())
        });
        assert!(prove_absent(&index, absent).unwrap().is_none());
    }

    #[test]
    fn checked_absence_hashes_actual_roots_and_blooms() {
        let (_root, index) = fixture();
        let absent = b"zzzDefinitelyAbsent94283";
        let bytes = fs::read(index.join(ROUTING_NAME)).unwrap();
        let manifest: RoutingManifest = serde_json::from_slice(&bytes[16..]).unwrap();
        let segment = index
            .join("segments")
            .join(format!("seg_{:04}", manifest.segments[0].id));
        for name in [NAME, "bloom.bin"] {
            let path = segment.join(name);
            let original = fs::read(&path).unwrap();
            let mut changed = original.clone();
            let at = if name == NAME { ROOT_HEADER } else { 11 };
            changed[at] ^= 1;
            fs::write(&path, changed).unwrap();
            assert!(prove_absent(&index, absent).is_err(), "{name}");
            fs::write(&path, &original[..original.len() - 1]).unwrap();
            assert!(prove_absent(&index, absent).is_err(), "truncated {name}");
            fs::write(path, original).unwrap();
        }
        assert!(prove_absent(&index, absent).unwrap().is_some());
    }
}
