//! Exercise the normal library build linked by integration tests, and verify
//! process-exit cleanup without changing this test process's environment.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD: &str = "FXI_TEST_STORAGE_PROBE";

#[test]
fn storage_probe() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let workers: Vec<_> = (0..16)
        .map(|_| std::thread::spawn(|| fxi::utils::app_data::isolate_test_storage().unwrap()))
        .collect();
    let paths: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let data = &paths[0];
    assert!(paths.iter().all(|path| path == data));
    assert_eq!(data.parent(), Some(std::env::temp_dir().as_path()));
    assert_eq!(fxi::utils::get_app_data_dir().unwrap(), *data);
    let expected = std::env::var_os("FXI_INDEXES")
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("indexes"));
    let source = tempfile::tempdir().unwrap();
    fs::write(
        source.path().join("fixture.rs"),
        "fn private_fixture() {}\n",
    )
    .unwrap();
    fxi::index::build::build_index_with_options(source.path(), true, true, Some(2)).unwrap();
    let container = fxi::utils::get_index_container(source.path()).unwrap();
    assert_eq!(container.parent(), Some(expected.as_path()));
    assert!(
        fxi::utils::get_index_dir(source.path())
            .unwrap()
            .join("meta.json")
            .is_file()
    );
    println!("\nFXI_TEST_STORAGE={}", data.display());
}

fn probe(explicit: Option<&Path>) -> PathBuf {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "storage_probe", "--nocapture"])
        .env(CHILD, "1")
        .env_remove("FXI_INDEXES");
    if let Some(path) = explicit {
        command.env("FXI_INDEXES", path);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    PathBuf::from(
        output
            .lines()
            .find_map(|line| line.strip_prefix("FXI_TEST_STORAGE="))
            .unwrap(),
    )
}

#[test]
fn integration_storage_is_private_and_cleaned_but_explicit_indexes_are_preserved() {
    let data = probe(None);
    assert!(!data.exists(), "normal exit must clean its private storage");
    let explicit = tempfile::tempdir().unwrap();
    fs::write(explicit.path().join("keep-me"), b"fixture-owned").unwrap();
    let data = probe(Some(explicit.path()));
    assert!(!data.exists());
    assert_eq!(
        fs::read(explicit.path().join("keep-me")).unwrap(),
        b"fixture-owned"
    );
    assert!(
        fs::read_dir(explicit.path())
            .unwrap()
            .any(|entry| entry.unwrap().path().is_dir())
    );
}
