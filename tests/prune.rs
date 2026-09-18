//! Prune CLI contracts use only fixture-owned source, storage and daemon paths.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    _temporary: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    indexes: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        fxi::utils::isolate_test_storage().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().canonicalize().unwrap();
        let root = base.join("source");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("main.rs"), "fn prune_fixture() {}\n").unwrap();
        let indexes = base.join("indexes");
        Self {
            _temporary: temporary,
            base,
            root,
            indexes,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fxi"));
        command
            .current_dir(&self.base)
            .env("FXI_APP_DATA", self.base.join("app-data"))
            .env("FXI_INDEXES", &self.indexes)
            .env("FXI_SOCKET", self.base.join("isolated.sock"))
            .env_remove("FXI_QUERY_LOCAL")
            .env_remove("FXI_GENERATION_ROUTING")
            .env("NO_COLOR", "1");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn index(&self) -> PathBuf {
        let output = self
            .command()
            .arg("index")
            .arg(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::read_dir(&self.indexes)
            .unwrap()
            .map(|entry| entry.unwrap())
            .find(|entry| entry.file_type().unwrap().is_dir())
            .unwrap()
            .path()
    }

    fn legacy(&self, mixed: bool) -> PathBuf {
        let container = self.index();
        let generation = fs::read_to_string(container.join("CURRENT")).unwrap();
        let generation = container.join("generations").join(generation.trim());
        if mixed {
            // A valid older legacy snapshot can have a different segment set.
            let meta = fxi::index::types::IndexMeta {
                root_path: self.root.clone(),
                ..Default::default()
            };
            fs::write(
                container.join("meta.json"),
                serde_json::to_vec(&meta).unwrap(),
            )
            .unwrap();
            fs::write(container.join("docs.bin"), 0u32.to_le_bytes()).unwrap();
            fs::write(container.join("paths.bin"), 0u32.to_le_bytes()).unwrap();
            fs::create_dir(container.join("segments")).unwrap();
        } else {
            for name in ["meta.json", "docs.bin", "paths.bin", "segments"] {
                fs::rename(generation.join(name), container.join(name)).unwrap();
            }
            fs::remove_file(container.join("CURRENT")).unwrap();
            fs::remove_dir_all(container.join("generations")).unwrap();
        }
        container
    }
}

#[test]
fn prune_previews_and_removes_only_missing_roots_without_following_bad_current() {
    let fixture = Fixture::new();
    let container = fixture.index();
    assert!(
        fixture
            .ok(&["prune", "--dry-run"])
            .contains("0 eligible indexes")
    );
    assert!(container.exists());
    fs::remove_dir_all(&fixture.root).unwrap();
    let malformed = fixture.indexes.join("malformed-registration");
    fs::create_dir(&malformed).unwrap();
    fs::write(malformed.join("CURRENT"), "../outside").unwrap();
    let original = fs::read(container.join("CURRENT")).unwrap();
    let preview = fixture.ok(&["prune", "--dry-run", "--verbose"]);
    assert!(preview.contains("1 eligible indexes") && preview.contains("0 removed"));
    assert!(preview.contains("Invalid CURRENT"));
    assert_eq!(fs::read(container.join("CURRENT")).unwrap(), original);
    assert!(container.exists());
    let applied = fixture.ok(&["prune"]);
    assert!(applied.contains("1 removed"));
    assert!(!container.exists());
    assert!(container.with_extension("lock").is_file());
    assert!(malformed.exists());
    assert!(fixture.ok(&["prune"]).contains("0 eligible indexes"));
}

#[test]
fn prune_preview_does_not_create_default_or_overridden_storage() {
    let fixture = Fixture::new();
    for args in [
        vec!["prune", "--dry-run"],
        vec!["prune", "--dry-run", "--include-legacy"],
    ] {
        assert!(fixture.ok(&args).contains("0 entries scanned"));
        assert!(!fixture.indexes.exists());
        assert!(!fixture.base.join("app-data").exists());
        let output = fixture
            .command()
            .env_remove("FXI_INDEXES")
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.base.join("app-data").exists());
    }
}

#[test]
fn prune_cli_skips_an_active_generation_reader() {
    use fs2::FileExt;
    let fixture = Fixture::new();
    let container = fixture.index();
    let generation = fs::read_to_string(container.join("CURRENT")).unwrap();
    let lease = fs::File::open(
        container
            .join("generations")
            .join(generation.trim())
            .join("lease"),
    )
    .unwrap();
    FileExt::lock_shared(&lease).unwrap();
    fs::remove_dir_all(&fixture.root).unwrap();
    let result = fixture.ok(&["prune"]);
    assert!(result.contains("busy writer or generation reader"));
    assert!(result.contains("0 removed"));
    assert!(container.exists());
    drop(lease);
    assert!(fixture.ok(&["prune"]).contains("1 removed"));
}

#[test]
fn legacy_cleanup_requires_explicit_offline_flag_and_validates_mixed_tables_independently() {
    for mixed in [false, true] {
        let fixture = Fixture::new();
        let container = fixture.legacy(mixed);
        fs::remove_dir_all(&fixture.root).unwrap();
        assert!(
            fixture
                .ok(&["prune"])
                .contains("legacy layout has no reader leases")
        );
        assert!(container.exists());
        let preview = fixture.ok(&["prune", "--dry-run", "--include-legacy"]);
        assert!(preview.contains("1 eligible indexes") && preview.contains("0 removed"));
        assert!(preview.contains("all fxi readers/indexers are stopped"));
        assert!(container.exists());
        assert!(
            fixture
                .ok(&["prune", "--include-legacy"])
                .contains("1 removed")
        );
        assert!(!container.exists());
        assert!(container.with_extension("lock").is_file());
    }
}

#[cfg(unix)]
#[test]
fn legacy_opt_in_respects_existing_daemon_status_without_new_protocol() {
    use fxi::server::protocol::{
        Request, Response, StatusResponse, read_message_with_id, write_message_with_id,
    };
    use std::os::unix::net::UnixListener;
    let fixture = Fixture::new();
    let container = fixture.legacy(false);
    fs::remove_dir_all(&fixture.root).unwrap();
    let listener = UnixListener::bind(fixture.base.join("isolated.sock")).unwrap();
    let root = fixture.root.clone();
    let daemon = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        let (request, id): (Request, _) = read_message_with_id(&mut connection).unwrap();
        assert!(matches!(request, Request::Status));
        let response = Response::Status(StatusResponse {
            uptime_secs: 1,
            indexes_loaded: 1,
            total_docs: 1,
            queries_served: 0,
            cache_hit_rate: 0.0,
            memory_bytes: 0,
            loaded_roots: vec![root],
            protocol_version: fxi::server::protocol::PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
            watch_enabled: false,
            watched_roots: vec![],
        });
        write_message_with_id(&mut connection, &response, id.as_deref()).unwrap();
    });
    let result = fixture.ok(&["prune", "--include-legacy"]);
    daemon.join().unwrap();
    assert!(result.contains("legacy root is loaded by the daemon"));
    assert!(result.contains("0 removed"));
    assert!(container.exists());
}
