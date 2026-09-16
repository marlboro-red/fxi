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
