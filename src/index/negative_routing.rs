//! Optional, generation-bound proof that an exact literal has no candidates.
//!
//! A certificate reuses structural validation only while every relevant Unix
//! file identity/length/mtime/ctime remains unchanged. Existing segment Blooms
//! supply the negative proof. Missing, damaged, stale, or unsupported evidence
//! always falls back to ordinary opening and its existing corruption errors.
//!
//! Current limitation: collecting an older generation changes ctime on inherited
//! hard-linked files, including the new generation's links. A certificate can
//! therefore become ineligible immediately after delta publication. Retaining
//! ctime is essential for correctness; full builds do not share these inodes.
use crate::index::types::{IndexMeta, SegmentId};
use crate::query::parser::{Query, QueryNode};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

const NAME: &str = "negative-routing.bin";
const MAGIC: &[u8; 8] = b"FXINEG01";
// This is a validation epoch, not merely the certificate serialization format.
// Epoch 1 established dictionary/range checks only. Epoch 2 also validates
// complete gram payloads and document membership. Never reuse weaker evidence.
const VALIDATION_EPOCH: u32 = 2;

#[derive(Serialize, Deserialize)]
struct Certificate {
    version: u32,
    generation: String,
    metadata_hash: u64,
    // File names and ordering are derived from the exact metadata, never from
    // untrusted certificate paths. Token/source-pack/line-map payloads are not
    // dependencies of this files-only literal proof.
    stamps: Vec<[u64; 7]>,
}

fn requested() -> bool {
    cfg!(unix) && std::env::var_os("FXI_NEGATIVE_ROUTING").is_some_and(|value| value == "1")
}

fn stamp(path: &Path) -> Result<[u64; 7]> {
    metadata_stamp(&fs::metadata(path)?)
}

fn metadata_stamp(metadata: &fs::Metadata) -> Result<[u64; 7]> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.is_file(), "routing dependency is not a file");
        Ok([
            metadata.len(),
            metadata.dev(),
            metadata.ino(),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
        ])
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        anyhow::bail!("negative routing requires strong Unix file stamps")
    }
}

fn segments(meta: &IndexMeta) -> Vec<SegmentId> {
    meta.base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
        .collect()
}

fn dependencies(index: &Path, meta: &IndexMeta) -> Vec<PathBuf> {
    let mut paths = vec![
        index.join("meta.json"),
        index.join("docs.bin"),
        index.join("paths.bin"),
    ];
    for id in segments(meta) {
        let segment = index.join("segments").join(format!("seg_{id:04}"));
        for name in ["grams.dict", "grams.postings", "bloom.bin"] {
            paths.push(segment.join(name));
        }
    }
    paths
}

fn stamps(paths: &[PathBuf]) -> Result<Vec<[u64; 7]>> {
    paths.iter().map(|path| stamp(path)).collect()
}

fn generation_name(index: &Path) -> Result<&str> {
    index
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.starts_with("gen-"))
        .context("negative routing requires a published generation")
}

/// Called after all segment inheritance/writes and before atomic publication.
/// Certificates are freshly produced for this generation, never inherited.
pub(crate) fn write_if_requested(index: &Path) -> Result<()> {
    if requested() {
        write_certificate(index)?;
    }
    Ok(())
}

fn write_certificate(index: &Path) -> Result<()> {
    let metadata_path = index.join("meta.json");
    let initial_metadata_stamp = stamp(&metadata_path)?;
    let metadata = fs::read(&metadata_path)?;
    let meta: IndexMeta = serde_json::from_slice(&metadata)?;
    let paths = dependencies(index, &meta);
    let before = stamps(&paths)?;
    ensure!(
        before[0] == initial_metadata_stamp,
        "metadata changed while reading"
    );
    // Validate inherited core structures as well as newly written segments.
    // The validator also proves each Bloom covers its entire gram dictionary.
    crate::index::reader::validate_negative_routing_core(index, &meta)?;
    ensure!(
        stamps(&paths)? == before,
        "core files changed during validation"
    );
    let certificate = Certificate {
        version: VALIDATION_EPOCH,
        generation: generation_name(index)?.to_owned(),
        metadata_hash: xxh3_64(&metadata),
        stamps: before,
    };
    let payload = serde_json::to_vec(&certificate)?;
    let mut encoded = Vec::with_capacity(16 + payload.len());
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&xxh3_64(&payload).to_le_bytes());
    encoded.extend_from_slice(&payload);
    // This file is generation-owned, not hard-linked from any older reader.
    fs::write(index.join(NAME), encoded)?;
    Ok(())
}

/// Deliberately narrower than general regex planning: an entire exact literal
/// (possibly captured), with no assertions, alternatives, nullable surroundings,
/// case-folded characters, newline semantics, or additional query filters.
pub(super) fn exact_literal(query: &Query) -> Option<Vec<u8>> {
    if query.options.case_insensitive || query.filters.has_any() {
        return None;
    }
    let QueryNode::Regex(pattern) = &query.root else {
        return None;
    };
    // Match the verifier's syntax/size-limit acceptance. A declined preflight
    // must leave invalid-regex diagnostics to ordinary query execution.
    regex::Regex::new(pattern).ok()?;
    let hir = regex_syntax::Parser::new().parse(pattern).ok()?;
    let mut node = &hir;
    while let regex_syntax::hir::HirKind::Capture(capture) = node.kind() {
        node = &capture.sub;
    }
    let regex_syntax::hir::HirKind::Literal(literal) = node.kind() else {
        return None;
    };
    let bytes = &literal.0;
    (bytes.len() >= 3 && !bytes.contains(&b'\n') && !bytes.contains(&b'\r')).then(|| bytes.to_vec())
}

/// Return metadata only for a fully certified empty result. The generation
/// remains pinned until the proof is complete. Every failure is a normal-open
/// fallback; this function never converts index or query errors to emptiness.
#[allow(dead_code)] // Used by the CLI crate, not the public library API.
pub(crate) fn preflight(root: &Path, query: &Query) -> Option<IndexMeta> {
    if !requested() || crate::index::query_local::requested() {
        return None;
    }
    let literal = exact_literal(query)?;
    let (index, _lease) = crate::index::generation::pin(root).ok()?;
    prove_absent(&index, &literal).ok().flatten()
}

fn prove_absent(index: &Path, literal: &[u8]) -> Result<Option<IndexMeta>> {
    let encoded = fs::read(index.join(NAME))?;
    ensure!(
        encoded.len() >= 16 && &encoded[..8] == MAGIC,
        "invalid routing header"
    );
    let payload = &encoded[16..];
    let expected = u64::from_le_bytes(encoded[8..16].try_into().unwrap());
    ensure!(xxh3_64(payload) == expected, "routing checksum mismatch");
    let certificate: Certificate = serde_json::from_slice(payload)?;
    ensure!(
        certificate.version == VALIDATION_EPOCH,
        "unsupported routing validation epoch"
    );
    ensure!(
        certificate.generation == generation_name(index)?,
        "routing generation mismatch"
    );
    let metadata = fs::read(index.join("meta.json"))?;
    ensure!(
        xxh3_64(&metadata) == certificate.metadata_hash,
        "routing metadata mismatch"
    );
    let meta: IndexMeta = serde_json::from_slice(&metadata)?;
    meta.validate_format()?;
    let stop: std::collections::HashSet<_> = meta.stop_grams.iter().copied().collect();
    let mut grams: Vec<_> = literal
        .windows(3)
        .map(|g| crate::index::types::bytes_to_trigram(g[0], g[1], g[2]))
        .filter(|gram| !stop.contains(gram))
        .collect();
    grams.sort_unstable();
    grams.dedup();
    if grams.is_empty() {
        return Ok(None);
    }
    let paths = dependencies(index, &meta);
    ensure!(
        certificate.stamps.len() == paths.len(),
        "routing dependency count mismatch"
    );
    // Try the cheap negative proof first. Positive/unknown queries return to
    // ordinary opening before paying the certificate's full metadata walk.
    for (number, _) in segments(&meta).iter().enumerate() {
        // Dependencies are meta/docs/paths followed by dict/postings/Bloom for
        // each segment. Bind the OPEN HANDLE to its certified stamp before
        // mapping, so a different file reached through this path is unknown.
        let slot = 5 + number * 3;
        let file = fs::File::open(&paths[slot])?;
        ensure!(
            metadata_stamp(&file.metadata()?)? == certificate.stamps[slot],
            "routing Bloom changed"
        );
        // The generation lease pins immutable index files. This maps a proven
        // index payload, never editable source content. Its checksum and full
        // dictionary coverage were validated when the certificate was issued.
        let mapped = unsafe { memmap2::Mmap::map(&file)? };
        let bloom = crate::utils::bloom::CertifiedBloomView::from_prevalidated_bytes(&mapped)
            .context("invalid certified Bloom header")?;
        if bloom.might_contain_all(&grams) {
            return Ok(None);
        }
    }
    ensure!(
        stamps(&paths)? == certificate.stamps,
        "routing dependencies changed"
    );
    Ok(Some(meta))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::index::{build::build_index_with_options, reader::IndexReader};
    use crate::query::{QueryExecutor, parse_query};

    struct Fixture(tempfile::TempDir);
    impl Fixture {
        fn new(filler: usize) -> Self {
            let result = Self(tempfile::tempdir().unwrap());
            fs::write(result.0.path().join("a.txt"), "abc\n").unwrap();
            fs::write(result.0.path().join("b.txt"), "bcd\n").unwrap();
            for number in 0..filler {
                fs::write(
                    result.0.path().join(format!("filler-{number}.txt")),
                    "unrelated words\n",
                )
                .unwrap();
            }
            build_index_with_options(result.0.path(), true, true, Some(1)).unwrap();
            write_certificate(&result.index()).unwrap();
            result
        }
        fn index(&self) -> PathBuf {
            crate::utils::get_index_dir(self.0.path()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = crate::utils::remove_index(self.0.path());
        }
    }
    fn proven(index: &Path, literal: &[u8]) -> bool {
        prove_absent(index, literal).ok().flatten().is_some()
    }

    #[test]
    fn obsolete_validation_evidence_falls_back_even_when_its_stamps_match() {
        let fixture = Fixture::new(0);
        let index = fixture.index();
        let encoded = fs::read(index.join(NAME)).unwrap();
        let mut certificate: Certificate = serde_json::from_slice(&encoded[16..]).unwrap();
        assert_eq!(certificate.version, VALIDATION_EPOCH);
        certificate.version = 1;
        let write_old = |certificate: &Certificate| {
            let payload = serde_json::to_vec(certificate).unwrap();
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&xxh3_64(&payload).to_le_bytes());
            bytes.extend_from_slice(&payload);
            fs::write(index.join(NAME), bytes).unwrap();
        };
        write_old(&certificate);
        assert!(
            prove_absent(&index, b"abcd")
                .unwrap_err()
                .to_string()
                .contains("validation epoch")
        );
        assert!(!proven(&index, b"abcd"));
        assert!(IndexReader::open_for_search_uncached(fixture.0.path()).is_ok());

        // Simulate evidence emitted by the former structural-only validator:
        // dictionaries/ranges and stamps agree, but an inherited payload is
        // malformed. New readers must not bypass their stronger checks.
        let posting_path = index.join("segments/seg_0001/grams.postings");
        let mut postings = fs::read(&posting_path).unwrap();
        postings.fill(0x80);
        fs::write(posting_path, postings).unwrap();
        let meta: IndexMeta =
            serde_json::from_slice(&fs::read(index.join("meta.json")).unwrap()).unwrap();
        certificate.stamps = stamps(&dependencies(&index, &meta)).unwrap();
        write_old(&certificate);
        assert!(
            prove_absent(&index, b"abcd")
                .unwrap_err()
                .to_string()
                .contains("validation epoch")
        );
        assert!(!proven(&index, b"abcd"));
        assert!(IndexReader::open_for_search_uncached(fixture.0.path()).is_err());
    }

    #[test]
    fn separate_segments_prove_absence_without_a_globally_absent_gram() {
        let fixture = Fixture::new(0);
        assert!(proven(&fixture.index(), b"abcd"));
        assert!(!proven(&fixture.index(), b"abc"));
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        for gram in crate::utils::query_trigrams("abcd") {
            assert!(!reader.get_trigram_docs(gram).unwrap().is_empty());
        }
        assert!(
            QueryExecutor::new(&reader)
                .execute_files_only(&parse_query("re:/abcd/"), 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn same_length_core_corruption_with_restored_mtime_falls_back_to_errors() {
        let fixture = Fixture::new(0);
        let index = fixture.index();
        for name in ["docs.bin", "paths.bin", "segments/seg_0001/grams.dict"] {
            write_certificate(&index).unwrap();
            let path = index.join(name);
            let original = fs::read(&path).unwrap();
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            let mut corrupt = original.clone();
            corrupt[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            fs::write(&path, corrupt).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(modified))
                .unwrap();
            assert!(!proven(&index, b"abcd"), "{name}");
            assert!(
                IndexReader::open_for_search_uncached(fixture.0.path()).is_err(),
                "{name}"
            );
            fs::write(path, original).unwrap();
        }
    }

    #[test]
    fn damaged_or_missing_proof_never_proves_empty_and_stop_grams_are_unknown() {
        let fixture = Fixture::new(0);
        let index = fixture.index();
        let certificate = fs::read(index.join(NAME)).unwrap();
        let mut damaged = certificate.clone();
        *damaged.last_mut().unwrap() ^= 1;
        fs::write(index.join(NAME), damaged).unwrap();
        assert!(!proven(&index, b"abcd"));
        fs::remove_file(index.join(NAME)).unwrap();
        assert!(!proven(&index, b"abcd"));
        fs::write(index.join(NAME), certificate).unwrap();
        let bloom_path = index.join("segments/seg_0001/bloom.bin");
        let bloom = fs::read(&bloom_path).unwrap();
        fs::write(&bloom_path, b"broken").unwrap();
        assert!(!proven(&index, b"abcd"));
        // An intact checksum alone does not prove a Bloom belongs to this
        // dictionary: certificate issuance must reject missing gram coverage.
        crate::index::segment_io::write_bloom_file(
            bloom_path.parent().unwrap(),
            &crate::utils::BloomFilter::new(10000, 0.01),
        )
        .unwrap();
        assert!(write_certificate(&index).is_err());
        fs::write(bloom_path, bloom).unwrap();
        let metadata_path = index.join("meta.json");
        let mut meta: IndexMeta =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        meta.stop_grams = crate::utils::query_trigrams("abcd");
        fs::write(metadata_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        write_certificate(&index).unwrap();
        assert!(!proven(&index, b"abcd"));
    }

    #[test]
    fn delta_and_compaction_require_their_own_generation_certificate() {
        let fixture = Fixture::new(20);
        let root = fixture.0.path();
        let (old, lease) = crate::index::generation::pin(root).unwrap();
        let old_certificate = fs::read(old.join(NAME)).unwrap();
        assert!(proven(&old, b"abcd"));
        fs::write(root.join("new.txt"), "abcd\n").unwrap();
        {
            let _lock = crate::utils::IndexLock::acquire(root).unwrap();
            assert!(crate::index::build::update_index(root).unwrap());
        }
        let delta = fixture.index();
        assert_ne!(old, delta);
        assert!(old.exists(), "reader lease must pin the older generation");
        let meta: IndexMeta =
            serde_json::from_slice(&fs::read(delta.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.segment_count, 23);
        write_certificate(&delta).unwrap();
        assert!(!proven(&delta, b"abcd"));
        assert!(proven(&delta, b"neverFoundXYZ"));
        fs::write(delta.join(NAME), old_certificate).unwrap();
        assert!(!proven(&delta, b"neverFoundXYZ"));
        crate::index::compact::merge_segments(root).unwrap();
        let compacted = fixture.index();
        assert_ne!(delta, compacted);
        write_certificate(&compacted).unwrap();
        assert!(!proven(&compacted, b"abcd"));
        assert!(proven(&compacted, b"neverFoundXYZ"));
        drop(lease);
    }

    #[test]
    fn collecting_inherited_hardlinks_safely_invalidates_the_new_certificate() {
        let fixture = Fixture::new(0);
        let old = fixture.index();
        let mut next = crate::index::generation::Generation::new(fixture.0.path()).unwrap();
        next.inherit_segments(&old).unwrap();
        for name in ["docs.bin", "paths.bin", "meta.json"] {
            fs::copy(old.join(name), next.path.join(name)).unwrap();
        }
        write_certificate(&next.path).unwrap();
        assert!(proven(&next.path, b"abcd"));
        next.publish().unwrap();
        assert!(!old.exists(), "the unpinned older generation is collected");
        assert_eq!(fixture.index(), next.path);
        // Removing the old hardlinks changes the shared inode's ctime. This
        // remains unknown evidence, even though its contents did not change.
        assert!(!proven(&next.path, b"abcd"));
        let reader = IndexReader::open_for_search_uncached(fixture.0.path()).unwrap();
        assert!(
            QueryExecutor::new(&reader)
                .execute_files_only(&parse_query("re:/abcd/"), 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unsupported_queries_and_invalid_regexes_cannot_enter_preflight() {
        assert_eq!(
            exact_literal(&parse_query("re:/abcd/")),
            Some(b"abcd".to_vec())
        );
        for input in [
            "abcd",
            "re:/[/",
            "re:/(?i:abcd)/",
            "re:/^abcd/",
            "re:/ab|cd/",
            "re:/ab/",
            "re:/abcd/ ext:rs",
        ] {
            assert!(exact_literal(&parse_query(input)).is_none(), "{input}");
        }
        let fixture = Fixture::new(0);
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        assert!(
            QueryExecutor::new(&reader)
                .execute_files_only(&parse_query("re:/[/"), 0)
                .is_err()
        );
    }
}
