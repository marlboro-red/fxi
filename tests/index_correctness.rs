use fxi::index::{build::build_index_with_options, reader::IndexReader};
use fxi::utils::app_data::{get_index_dir, remove_index};
use std::fs;
use std::path::PathBuf;

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        fs::write(fixture.0.path().join("a.txt"), "vector::start\n").unwrap();
        build_index_with_options(fixture.0.path(), true, true, Some(2)).unwrap();
        fixture
    }
    fn index(&self) -> PathBuf {
        get_index_dir(self.0.path()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = remove_index(self.0.path());
    }
}

#[test]
fn opening_reader_preserves_in_progress_writer_files() {
    let fixture = Fixture::new();
    let index = fixture.index();
    let _writer_lock = fxi::utils::IndexLock::acquire(fixture.0.path()).unwrap();
    for name in ["docs.bin.tmp", "paths.bin.tmp", "meta.json.tmp"] {
        fs::write(index.join(name), b"writer data").unwrap();
    }
    let segment = index.join("segments/seg_9999.tmp");
    fs::create_dir_all(&segment).unwrap();
    fs::write(segment.join("grams.postings"), b"postings").unwrap();
    let reader = IndexReader::open(fixture.0.path()).unwrap();
    assert_eq!(reader.meta.doc_count, 1);
    for name in ["docs.bin.tmp", "paths.bin.tmp", "meta.json.tmp"] {
        assert_eq!(fs::read(index.join(name)).unwrap(), b"writer data");
    }
    assert_eq!(
        fs::read(segment.join("grams.postings")).unwrap(),
        b"postings"
    );
}

#[test]
fn repeated_compaction_preserves_omitted_gram_coverage() {
    use fxi::index::{build::update_index, compact::merge_segments};
    use fxi::query::{QueryExecutor, parse_query};
    let fixture = Fixture::new();
    for name in ["b.txt", "c.txt"] {
        fs::write(fixture.0.path().join(name), "vector::start\n").unwrap();
    }
    fs::write(fixture.0.path().join("filler.txt"), "unrelated content\n").unwrap();
    build_index_with_options(fixture.0.path(), true, true, Some(2)).unwrap();
    // Simulate a legacy index that elected to omit these common grams.
    // Fresh indexes retain all grams by default now.
    let meta_path = fixture.index().join("meta.json");
    let mut meta: fxi::index::types::IndexMeta =
        serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta.stop_grams = fxi::utils::query_trigrams("r::st");
    fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    for iteration in 0..4 {
        merge_segments(fixture.0.path()).unwrap();
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        let files = QueryExecutor::new(&reader)
            .execute_files_only(&parse_query("\"r::st\""), 0)
            .unwrap();
        assert_eq!(
            files,
            vec![
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.txt")
            ],
            "compaction {iteration}"
        );
        drop(reader);
        fs::write(
            fixture.0.path().join(format!("new{iteration}.txt")),
            "other text\n",
        )
        .unwrap();
        update_index(fixture.0.path()).unwrap();
    }
}

#[test]
fn incomplete_segments_fail_open_instead_of_losing_matches() {
    for file in [
        "grams.dict",
        "grams.postings",
        "tokens.dict",
        "tokens.postings",
        "tokens.positions",
    ] {
        let fixture = Fixture::new();
        fs::remove_file(fixture.index().join("segments/seg_0001").join(file)).unwrap();
        assert!(
            IndexReader::open(fixture.0.path()).is_err(),
            "missing {file}"
        );
    }
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.index().join("segments/seg_0001")).unwrap();
    assert!(IndexReader::open(fixture.0.path()).is_err());
}

#[test]
fn empty_postings_are_valid_without_placeholder_files() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("a.txt"), "x").unwrap();
    build_index_with_options(fixture.0.path(), true, true, None).unwrap();
    let reader = IndexReader::open(fixture.0.path()).unwrap();
    assert_eq!(reader.meta.doc_count, 1);
    assert!(
        !fixture
            .index()
            .join("segments/seg_0001/.empty_postings")
            .exists()
    );
}

#[test]
fn truncated_postings_fail_open() {
    for name in ["grams.postings", "tokens.postings", "tokens.positions"] {
        let fixture = Fixture::new();
        fs::write(fixture.index().join("segments/seg_0001").join(name), []).unwrap();
        assert!(IndexReader::open(fixture.0.path()).is_err(), "{name}");
    }
}

#[test]
fn published_readers_remain_valid_across_compaction_and_rebuild() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("b.txt"), "other text\n").unwrap();
    build_index_with_options(fixture.0.path(), true, true, Some(1)).unwrap();
    let old_path = fixture.index();
    let reader = IndexReader::open(fixture.0.path()).unwrap();
    let postings = reader.get_token_docs("vector");
    let old_bytes = fs::read(old_path.join("segments/seg_0001/grams.postings")).unwrap();
    fxi::index::compact::merge_segments(fixture.0.path()).unwrap();
    assert_ne!(old_path, fixture.index());
    assert_eq!(reader.get_token_docs("vector"), postings);
    assert_eq!(
        fs::read(old_path.join("segments/seg_0001/grams.postings")).unwrap(),
        old_bytes
    );
    for id in postings.iter() {
        assert!(reader.get_line_map(id).is_some());
    }
    build_index_with_options(fixture.0.path(), true, true, None).unwrap();
    assert_eq!(reader.get_token_docs("vector"), postings);
    assert!(old_path.exists(), "active reader must pin its generation");
    drop(reader);
    build_index_with_options(fixture.0.path(), true, true, None).unwrap();
    assert!(
        !old_path.exists(),
        "unleased generations should be reclaimed"
    );
}

#[test]
fn failed_rebuild_does_not_replace_published_index() {
    let fixture = Fixture::new();
    let published = fixture.index();
    let mut writer = fxi::index::writer::ChunkedIndexWriter::new(
        fixture.0.path(),
        fxi::index::types::IndexConfig::default(),
    )
    .unwrap();
    let staging = writer.index_path().to_path_buf();
    fs::create_dir(staging.join("meta.json")).unwrap();
    assert!(writer.finalize().is_err());
    assert_eq!(fixture.index(), published);
    assert_eq!(
        IndexReader::open(fixture.0.path()).unwrap().meta.doc_count,
        1
    );
    drop(writer);
    assert!(!staging.exists());
}

#[test]
fn deletion_only_delta_publishes_no_missing_segment() {
    let fixture = Fixture::new();
    let mut meta = IndexReader::open(fixture.0.path()).unwrap().meta.clone();
    let mut writer = fxi::index::writer::DeltaSegmentWriter::new(fixture.0.path(), 2).unwrap();
    writer.mark_tombstone(std::path::Path::new("a.txt"));
    writer.finalize(&mut meta).unwrap();
    let reader = IndexReader::open(fixture.0.path()).unwrap();
    assert!(reader.valid_doc_ids().is_empty());
    assert!(reader.meta.delta_segments.is_empty());
}

#[test]
fn concurrent_opens_observe_complete_rebuild_generations() {
    let fixture = Fixture::new();
    let root = fixture.0.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        for _ in 0..8 {
            build_index_with_options(&root, true, true, None).unwrap();
        }
    });
    for _ in 0..100 {
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        assert_eq!(reader.meta.doc_count, 1);
        let docs = reader.get_token_docs("vector");
        assert_eq!(docs.len(), 1);
        for id in docs {
            assert_eq!(
                reader.get_path(reader.get_document(id).unwrap()).unwrap(),
                std::path::Path::new("a.txt")
            );
        }
    }
    worker.join().unwrap();
}

#[test]
fn incremental_scan_detects_subsecond_and_same_stamp_size_changes() {
    use std::time::{Duration, UNIX_EPOCH};
    let fixture = Fixture::new();
    let path = fixture.0.path().join("a.txt");
    for i in 0..10 {
        fs::write(fixture.0.path().join(format!("filler{i}.txt")), "other").unwrap();
    }
    let stamp = UNIX_EPOCH + Duration::new(1_700_000_000, 100_000_000);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(stamp))
        .unwrap();
    build_index_with_options(fixture.0.path(), true, true, None).unwrap();
    for (text, nanos) in [
        ("changed content", 200_000_000),
        ("much longer changed content", 200_000_000),
    ] {
        fs::write(&path, text).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::new(1_700_000_000, nanos)),
            )
            .unwrap();
        fxi::index::build::update_index(fixture.0.path()).unwrap();
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        let docs = reader.get_token_docs("changed") & reader.valid_doc_ids();
        assert_eq!(docs.len(), 1);
        let doc = reader.get_document(docs.min().unwrap()).unwrap();
        assert_eq!(doc.size, text.len() as u64);
        assert_eq!(doc.mtime_seconds(), 1_700_000_000);
        assert_eq!(doc.mtime, 1_700_000_000_000_000_000 + nanos as u64);
    }
}

#[test]
fn legacy_seconds_and_watcher_nanoseconds_normalize_on_read() {
    for raw in [1_700_000_000_u64, 1_700_000_000_000_000_000_u64] {
        let fixture = Fixture::new();
        let index = fixture.index();
        let mut meta: serde_json::Value =
            serde_json::from_slice(&fs::read(index.join("meta.json")).unwrap()).unwrap();
        meta["version"] = 1.into();
        fs::write(index.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        let mut docs = fs::read(index.join("docs.bin")).unwrap();
        docs[20..28].copy_from_slice(&raw.to_le_bytes());
        fs::write(index.join("docs.bin"), docs).unwrap();
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        assert_eq!(reader.documents()[0].mtime, 1_700_000_000_000_000_000);
        assert_eq!(reader.documents()[0].mtime_seconds(), 1_700_000_000);
    }
}

#[test]
fn impossible_record_counts_and_path_lengths_fail_before_allocation() {
    for name in [
        "docs.bin",
        "paths.bin",
        "segments/seg_0001/grams.dict",
        "segments/seg_0001/tokens.dict",
    ] {
        let fixture = Fixture::new();
        let path = fixture.index().join(name);
        let mut bytes = fs::read(&path).unwrap();
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(IndexReader::open(fixture.0.path()).is_err(), "{name}");
    }
    let fixture = Fixture::new();
    let path = fixture.index().join("paths.bin");
    let mut bytes = fs::read(&path).unwrap();
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(path, bytes).unwrap();
    assert!(IndexReader::open(fixture.0.path()).is_err());
}

#[test]
fn mapped_dictionaries_reject_bad_lengths_and_token_encoding() {
    for corrupt_length in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.index().join("segments/seg_0001/tokens.dict");
        let mut bytes = fs::read(&path).unwrap();
        if corrupt_length {
            bytes[4..6].copy_from_slice(&u16::MAX.to_le_bytes());
        } else {
            bytes[6] = 0xff;
        }
        fs::write(path, bytes).unwrap();
        assert!(IndexReader::open(fixture.0.path()).is_err());
    }
}

#[test]
fn damaged_or_legacy_optional_blooms_cannot_hide_documents() {
    for corruption in 0..5 {
        let fixture = Fixture::new();
        let path = fixture.index().join("segments/seg_0001/bloom.bin");
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..6], b"\0\0\0\0\0\x01");
        match corruption {
            0 => {
                // A valid old-format filter with all bits unset must be ignored.
                bytes = vec![7, 1, 0, 0, 0];
                bytes.extend_from_slice(&0u64.to_le_bytes());
            }
            1 => bytes[11] ^= 1, // valid lengths, damaged bit payload
            2 => bytes[6] = 0,   // corrupt probe count
            3 => bytes[5] = 99,  // unknown hash version
            _ => {
                bytes.pop();
            } // truncated checksum
        }
        fs::write(path, bytes).unwrap();
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        let docs = reader
            .get_trigram_docs_with_bloom(&[fxi::index::types::bytes_to_trigram(b'v', b'e', b'c')]);
        assert_eq!(docs.iter().collect::<Vec<_>>(), vec![1]);
    }
}

#[test]
fn common_grams_remain_selective_after_repeated_compaction() {
    use fxi::index::compact::merge_segments;
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("b.txt"), "vector::start\n").unwrap();
    fs::write(fixture.0.path().join("c.txt"), "unrelated content\n").unwrap();
    build_index_with_options(fixture.0.path(), true, true, Some(1)).unwrap();
    let gram = fxi::index::types::bytes_to_trigram(b'v', b'e', b'c');
    for _ in 0..3 {
        let reader = IndexReader::open(fixture.0.path()).unwrap();
        assert!(!reader.is_stop_gram(gram));
        assert_eq!(reader.get_trigram_docs_with_bloom(&[gram]).len(), 2);
        drop(reader);
        merge_segments(fixture.0.path()).unwrap();
    }
}

#[test]
fn token_dictionary_order_and_overflowed_ranges_fail_open() {
    for corruption in 0..4 {
        let fixture = Fixture::new();
        let path = fixture.index().join("segments/seg_0001/tokens.dict");
        let original = fs::read(&path).unwrap();
        assert!(u32::from_le_bytes(original[..4].try_into().unwrap()) >= 2);
        let first_len = u16::from_le_bytes(original[4..6].try_into().unwrap()) as usize;
        let second = 4 + 30 + first_len;
        let second_len =
            u16::from_le_bytes(original[second..second + 2].try_into().unwrap()) as usize;
        let end = second + 30 + second_len;
        let mut bytes = original.clone();
        match corruption {
            0 | 3 => {
                bytes.truncate(4);
                if corruption == 0 {
                    bytes.extend_from_slice(&original[second..end]);
                } else {
                    bytes.extend_from_slice(&original[4..second]);
                }
                bytes.extend_from_slice(&original[4..second]);
                bytes.extend_from_slice(&original[end..]);
            }
            _ => {
                let fields = 6 + first_len + if corruption == 2 { 16 } else { 0 };
                bytes[fields..fields + 8].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
                bytes[fields + 8..fields + 12].copy_from_slice(&4u32.to_le_bytes());
            }
        }
        fs::write(path, bytes).unwrap();
        assert!(
            IndexReader::open(fixture.0.path()).is_err(),
            "corruption {corruption}"
        );
    }
}

#[test]
fn legacy_token_dictionaries_without_positions_remain_readable() {
    let fixture = Fixture::new();
    let index = fixture.index();
    let dict_path = index.join("segments/seg_0001/tokens.dict");
    let bytes = fs::read(&dict_path).unwrap();
    let count = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let mut legacy = bytes[..4].to_vec();
    let mut cursor = 4;
    for _ in 0..count {
        let len = u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().unwrap()) as usize;
        legacy.extend_from_slice(&bytes[cursor..cursor + 18 + len]);
        cursor += 30 + len;
    }
    fs::write(dict_path, legacy).unwrap();
    fs::remove_file(index.join("segments/seg_0001/tokens.positions")).unwrap();
    let meta_path = index.join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    meta["has_positions"] = serde_json::json!(false);
    fs::write(meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    let reader = IndexReader::open(fixture.0.path()).unwrap();
    assert_eq!(
        reader.get_token_docs("vector").iter().collect::<Vec<_>>(),
        vec![1]
    );
}

#[test]
fn index_payload_directories_are_rejected() {
    for name in ["grams.postings", "tokens.postings", "tokens.positions"] {
        let fixture = Fixture::new();
        let path = fixture.index().join("segments/seg_0001").join(name);
        fs::remove_file(&path).unwrap();
        fs::create_dir(path).unwrap();
        assert!(IndexReader::open(fixture.0.path()).is_err(), "{name}");
    }
}

#[test]
fn incremental_searches_exclude_tombstoned_documents_in_every_output_mode() {
    use fxi::index::build::update_index;
    use fxi::query::{QueryExecutor, parse_query};
    let fixture = Fixture::new();
    let root = fixture.0.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join("a.txt"), "needle original\n").unwrap();
    for i in 0..20 {
        fs::write(root.join(format!("filler-{i}.txt")), "unrelated text\n").unwrap();
    }
    build_index_with_options(root, true, true, Some(3)).unwrap();
    let patterns = [
        "re:/needle/",
        "needle",
        "re:/needle|missing/",
        "re:/^needle/",
        "needle ext:txt",
    ];
    for version in 0..3 {
        let content = format!("needle version {version} {}\n", "x".repeat(version));
        fs::write(root.join("a.txt"), &content).unwrap();
        {
            let _lock = fxi::utils::IndexLock::acquire(root).unwrap();
            assert!(update_index(root).unwrap());
        }
        let reader = IndexReader::open(root).unwrap();
        assert_eq!(reader.meta.tombstone_count, version as u32 + 1);
        let executor = QueryExecutor::new(&reader);
        for pattern in patterns {
            let query = parse_query(pattern);
            assert_eq!(
                executor.execute_files_only(&query, 0).unwrap(),
                vec![PathBuf::from("a.txt")],
                "{pattern}"
            );
            assert_eq!(
                executor.execute_match_counts(&query, 0).unwrap(),
                vec![(PathBuf::from("a.txt"), 1)],
                "{pattern}"
            );
            let matches = executor.execute_with_content(&query, 0, 0).unwrap();
            assert_eq!(matches.len(), 1, "{pattern}");
            assert_eq!(matches[0].line_content, content.strip_suffix('\n').unwrap());
            assert_eq!(executor.execute(&query).unwrap().len(), 1, "{pattern}");
        }
    }
    fs::remove_file(root.join("a.txt")).unwrap();
    fs::write(root.join(".gitignore"), "a.txt\n").unwrap();
    {
        let _lock = fxi::utils::IndexLock::acquire(root).unwrap();
        update_index(root).unwrap();
    }
    // Recreated but excluded content must not resurrect an old posting entry.
    fs::write(root.join("a.txt"), "needle recreated but ignored\n").unwrap();
    let reader = IndexReader::open(root).unwrap();
    let executor = QueryExecutor::new(&reader);
    for pattern in patterns {
        let query = parse_query(pattern);
        assert!(
            executor.execute_files_only(&query, 0).unwrap().is_empty(),
            "{pattern}"
        );
        assert!(
            executor.execute_match_counts(&query, 0).unwrap().is_empty(),
            "{pattern}"
        );
        assert!(
            executor
                .execute_with_content(&query, 0, 0)
                .unwrap()
                .is_empty(),
            "{pattern}"
        );
        assert!(executor.execute(&query).unwrap().is_empty(), "{pattern}");
    }
}
