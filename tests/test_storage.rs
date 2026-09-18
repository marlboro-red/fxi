//! Exercise the normal library build linked by integration tests, and verify
//! process-exit cleanup without changing this test process's environment.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD: &str = "FXI_TEST_STORAGE_PROBE";
const CONFIG_CHILD: &str = "FXI_TEST_CONFIG_PROBE";

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
        .env_remove("FXI_APP_DATA")
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

#[test]
fn config_probe() {
    let Ok(expected_debounce) = std::env::var(CONFIG_CHILD) else {
        return;
    };
    let expected_debounce: u64 = expected_debounce.parse().unwrap();
    let data = PathBuf::from(std::env::var_os("FXI_APP_DATA").unwrap());
    let existed_before = data.exists();
    // This is the ordinary library build, without the unit-test storage override.
    assert_eq!(fxi::utils::get_app_data_path().unwrap(), data);
    let config = fxi::server::watcher::WatcherConfig::load();
    assert_eq!(config.debounce_ms, expected_debounce);
    assert_eq!(data.exists(), existed_before);
    assert!(!data.join("indexes").exists());
}

fn probe_config(data: &Path, expected_debounce: u64) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "config_probe", "--nocapture"])
        .env(CONFIG_CHILD, expected_debounce.to_string())
        .env("FXI_APP_DATA", data)
        .env_remove("FXI_INDEXES");
    for name in [
        "FXI_DEBOUNCE_MS",
        "FXI_MAX_BATCH_AGE_MS",
        "FXI_DELTA_FLUSH_SECS",
        "FXI_MERGE_SEGMENTS",
        "FXI_REBUILD_THRESHOLD",
    ] {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn reading_missing_config_does_not_create_app_data() {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("missing-app-data");
    probe_config(&data, fxi::server::watcher::DEFAULT_DEBOUNCE_MS);
    assert!(!data.exists());
}

#[test]
fn explicit_app_data_supplies_configuration_without_creating_indexes() {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join("config.toml"),
        "[watcher]\ndebounce_ms = 873\n",
    )
    .unwrap();
    probe_config(fixture.path(), 873);
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}
