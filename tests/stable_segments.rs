//! Opt-in storage experiment: real CLI, private indexes, no global environment changes.
use fs2::FileExt;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    indexes: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        fxi::utils::isolate_test_storage().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("source");
        fs::create_dir_all(root.join(".git")).unwrap();
        for id in 0..32 {
            fs::write(
                root.join(format!("file{id}.rs")),
                format!("fn symbol_{id}() {{}}\nshared original\n"),
            )
            .unwrap();
        }
        let indexes = base.join("indexes");
        Self {
            _temp: temp,
            base,
            root,
            indexes,
        }
    }
    fn command(&self, stable: bool) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fxi"));
        c.current_dir(&self.root)
            .env("FXI_APP_DATA", self.base.join("app"))
            .env("FXI_INDEXES", &self.indexes)
            .env("FXI_SOCKET", self.base.join("unused.sock"))
            .env("FXI_STABLE_SEGMENTS", if stable { "1" } else { "0" })
            .env("FXI_QUERY_LOCAL", "0")
            .env("FXI_GENERATION_ROUTING", "0")
            .env("FXI_NEGATIVE_ROUTING", "0")
            .env("FXI_SOURCE_PACK", "0");
        c
    }
    fn ok(&self, stable: bool, args: &[&str]) -> String {
        let output = self.command(stable).args(args).output().unwrap();
        success(output)
    }
    fn container(&self) -> PathBuf {
        fs::read_dir(&self.indexes)
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.file_type().unwrap().is_dir())
            .unwrap()
            .path()
    }
    fn generation(&self) -> PathBuf {
        let c = self.container();
        c.join("generations")
            .join(fs::read_to_string(c.join("CURRENT")).unwrap().trim())
    }
    fn meta(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.generation().join("meta.json")).unwrap()).unwrap()
    }
    fn matches(&self, pattern: &str) -> Vec<String> {
        let mut names: Vec<_> = self
            .ok(false, &["-l", "--color=never", pattern])
            .lines()
            .map(|p| {
                PathBuf::from(p)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        names.sort();
        names
    }
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn stable_updates_preserve_objects_and_compaction_respects_old_reader_leases() {
    for profile in ["full", "lean"] {
        let f = Fixture::new();
        f.ok(
            true,
            &[
                "index",
                "--force",
                "--profile",
                profile,
                "--chunk-size",
                "8",
            ],
        );
        let before = f.meta();
        assert_eq!(before["version"], if profile == "full" { 4 } else { 5 });
        let old_generation = f.generation();
        let lease = fs::File::open(old_generation.join("lease")).unwrap();
        FileExt::lock_shared(&lease).unwrap();
        let names: Vec<_> = before["segment_objects"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        let bytes: Vec<_> = names
            .iter()
            .map(|n| fs::read(f.container().join("objects").join(n).join("grams.dict")).unwrap())
            .collect();
        fs::write(f.root.join("new.rs"), "newMarker shared\n").unwrap();
        f.ok(false, &["index"]); // Persisted layout survives removal of the experiment flag.
        for (name, expected) in names.iter().zip(&bytes) {
            assert_eq!(
                fs::read(f.container().join("objects").join(name).join("grams.dict")).unwrap(),
                *expected
            );
            assert!(
                f.meta()["segment_objects"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|v| v == name)
            );
        }
        assert_eq!(f.matches("newMarker"), ["new.rs"]);
        assert_eq!(f.matches("shared").len(), 33);
        fs::remove_file(f.root.join("file0.rs")).unwrap();
        f.ok(false, &["index"]);
        assert_eq!(f.matches("shared").len(), 32);
        fs::write(f.root.join("file1.rs"), "replacementMarker\n").unwrap();
        f.ok(false, &["index"]);
        assert_eq!(f.matches("replacementMarker"), ["file1.rs"]);
        f.ok(false, &["compact"]);
        assert_eq!(f.matches("shared").len(), 31);
        assert_eq!(f.meta()["segment_objects"].as_object().unwrap().len(), 1);
        for name in &names {
            assert!(f.container().join("objects").join(name).is_dir());
        }
        assert!(old_generation.exists());
        drop(lease);
        fs::write(f.root.join("another.rs"), "anotherMarker\n").unwrap();
        f.ok(false, &["index"]);
        assert!(!old_generation.exists());
        for name in &names {
            assert!(!f.container().join("objects").join(name).exists());
        }
    }
}

#[test]
fn migration_preserves_legacy_readers_and_prune_cleans_the_object_store() {
    let f = Fixture::new();
    f.ok(false, &["index", "--chunk-size", "8"]);
    let old = f.generation();
    let lease = fs::File::open(old.join("lease")).unwrap();
    FileExt::lock_shared(&lease).unwrap();
    fs::write(f.root.join("new.rs"), "migrationMarker\n").unwrap();
    f.ok(true, &["index"]);
    assert!(old.join("segments/seg_0001/grams.dict").is_file());
    assert_eq!(f.matches("migrationMarker"), ["new.rs"]);
    fs::remove_dir_all(&f.root).unwrap();
    let mut command = f.command(false);
    command.current_dir(&f.base);
    let busy = success(command.args(["prune", "--dry-run"]).output().unwrap());
    assert!(busy.contains("busy writer or generation reader"));
    drop(lease);
    let mut command = f.command(false);
    command.current_dir(&f.base);
    let preview = success(command.args(["prune", "--dry-run"]).output().unwrap());
    assert!(preview.contains("1 eligible indexes"), "{preview}");
    let mut command = f.command(false);
    command.current_dir(&f.base);
    let removed = success(command.arg("prune").output().unwrap());
    assert!(removed.contains("1 removed"), "{removed}");
}

#[test]
fn broken_object_references_fail_without_publishing_and_routing_is_explicitly_rejected() {
    let f = Fixture::new();
    f.ok(true, &["index"]);
    let current = fs::read(f.container().join("CURRENT")).unwrap();
    fs::write(f.root.join("new.rs"), "newMarker\n").unwrap();
    let rejected = f
        .command(false)
        .env("FXI_QUERY_LOCAL", "1")
        .arg("index")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("strict validation"));
    assert_eq!(fs::read(f.container().join("CURRENT")).unwrap(), current);
    let metadata_path = f.generation().join("meta.json");
    let original = fs::read(&metadata_path).unwrap();
    for bad in ["../../outside", "gen-missing-seg-0001"] {
        let mut meta = f.meta();
        *meta["segment_objects"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap() = bad.into();
        fs::write(&metadata_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(
            !f.command(false)
                .args(["-l", "shared"])
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            !f.command(false)
                .arg("index")
                .output()
                .unwrap()
                .status
                .success()
        );
        assert_eq!(fs::read(f.container().join("CURRENT")).unwrap(), current);
        fs::write(&metadata_path, &original).unwrap();
    }
    let mut meta = f.meta();
    meta["segment_objects"].as_object_mut().unwrap().clear();
    fs::write(&metadata_path, serde_json::to_vec(&meta).unwrap()).unwrap();
    assert!(
        !f.command(false)
            .args(["-l", "shared"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn captured_sources_survive_stable_updates_and_compaction() {
    let f = Fixture::new();
    success(
        f.command(true)
            .env("FXI_SOURCE_PACK", "1")
            .env("FXI_SOURCE_PACK_COMPRESSION", "1")
            .args(["index", "--chunk-size", "8"])
            .output()
            .unwrap(),
    );
    fs::write(f.root.join("new.rs"), "packedMarker\n").unwrap();
    f.ok(false, &["index"]);
    f.ok(false, &["compact"]);
    assert_eq!(f.matches("packedMarker"), ["new.rs"]);
    let meta = f.meta();
    let name = meta["segment_objects"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(
        f.container()
            .join("objects")
            .join(name)
            .join("source.table")
            .is_file()
    );
}
