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
