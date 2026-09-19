//! Opt-in storage experiment: real CLI, private indexes, no global environment changes.
use fs2::FileExt;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Clone, Copy, Debug)]
enum CheckedMode {
    Local,
    Generation,
    #[cfg(unix)]
    Negative,
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    bytes: Option<Vec<u8>>,
    #[cfg(unix)]
    times: [i64; 4],
}

type TreeSnapshot = BTreeMap<PathBuf, TreeEntry>;

fn snapshot_tree(root: &Path) -> TreeSnapshot {
    fn visit(root: &Path, path: &Path, entries: &mut TreeSnapshot) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(metadata.is_dir() || metadata.is_file());
        let bytes = metadata.is_file().then(|| fs::read(path).unwrap());
        #[cfg(unix)]
        let times = {
            use std::os::unix::fs::MetadataExt;
            [
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ]
        };
        entries.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            TreeEntry {
                bytes,
                #[cfg(unix)]
                times,
            },
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

fn assert_objects_unchanged(objects: &BTreeMap<PathBuf, TreeSnapshot>) {
    for (path, expected) in objects {
        assert_eq!(&snapshot_tree(path), expected, "object {}", path.display());
    }
}

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
            .env("FXI_SOURCE_PACK", "0")
            .env("FXI_SOURCE_PACK_COMPRESSION", "0");
        c
    }
    fn checked_command(&self, stable: bool, mode: CheckedMode) -> Command {
        let mut command = self.command(stable);
        command.env("FXI_QUERY_LOCAL", "1");
        match mode {
            CheckedMode::Local => {}
            CheckedMode::Generation => {
                command.env("FXI_GENERATION_ROUTING", "1");
            }
            #[cfg(unix)]
            CheckedMode::Negative => {
                command.env("FXI_NEGATIVE_ROUTING", "1");
            }
        }
        command
    }
    fn checked_ok(&self, stable: bool, mode: CheckedMode, args: &[&str]) -> String {
        success(
            self.checked_command(stable, mode)
                .args(args)
                .output()
                .unwrap(),
        )
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
    fn object_paths(&self) -> Vec<PathBuf> {
        self.meta()["segment_objects"]
            .as_object()
            .unwrap()
            .values()
            .map(|name| {
                self.container()
                    .join("objects")
                    .join(name.as_str().unwrap())
            })
            .collect()
    }
    fn snapshot_objects(&self) -> BTreeMap<PathBuf, TreeSnapshot> {
        self.object_paths()
            .into_iter()
            .map(|path| {
                let entries = snapshot_tree(&path);
                (path, entries)
            })
            .collect()
    }
    fn pin(&self) -> (PathBuf, fs::File) {
        let generation = self.generation();
        let lease = fs::File::open(generation.join("lease")).unwrap();
        FileExt::lock_shared(&lease).unwrap();
        (generation, lease)
    }
    fn assert_checked_evidence(&self, mode: CheckedMode) {
        for object in self.object_paths() {
            for name in ["grams.checks", "grams.bloom-check"] {
                assert!(object.join(name).is_file(), "{} {name}", object.display());
            }
        }
        let generation = self.generation();
        let certificate = fs::read(generation.join("query-routing.bin")).unwrap();
        assert_eq!(&certificate[..8], b"FXIROUT1");
        let manifest: serde_json::Value = serde_json::from_slice(&certificate[16..]).unwrap();
        // A certificate bound to pre-export metadata would silently fall back;
        // verify that publication certifies the final stable-object manifest.
        for (key, name) in [
            ("meta_hash", "meta.json"),
            ("docs_hash", "docs.bin"),
            ("paths_hash", "paths.bin"),
        ] {
            let bytes = fs::read(generation.join(name)).unwrap();
            assert_eq!(
                manifest[key].as_u64(),
                Some(xxhash_rust::xxh3::xxh3_64(&bytes))
            );
        }
        match mode {
            CheckedMode::Local => {}
            CheckedMode::Generation => {
                let certificate = fs::read(generation.join("generation-routing.bin")).unwrap();
                let metadata = fs::read(generation.join("meta.json")).unwrap();
                assert_eq!(
                    u64::from_le_bytes(certificate[40..48].try_into().unwrap()),
                    xxhash_rust::xxh3::xxh3_64(&metadata)
                );
            }
            #[cfg(unix)]
            CheckedMode::Negative => {
                let certificate = fs::read(generation.join("negative-routing.bin")).unwrap();
                let manifest: serde_json::Value =
                    serde_json::from_slice(&certificate[16..]).unwrap();
                let metadata = fs::read(generation.join("meta.json")).unwrap();
                assert_eq!(
                    manifest["metadata_hash"].as_u64(),
                    Some(xxhash_rust::xxh3::xxh3_64(&metadata))
                );
            }
        }
    }
    fn assert_source_queries(&self, mode: CheckedMode) {
        for pattern in [
            "shared",
            "original",
            "symbol_0",
            "newMarker",
            "replacementMarker",
            "renamedMarker",
            "packedMarker",
            "absentMarker94283",
        ] {
            let mut expected: Vec<_> = fs::read_dir(&self.root)
                .unwrap()
                .map(|entry| entry.unwrap())
                .filter(|entry| entry.file_type().unwrap().is_file())
                .filter(|entry| fs::read_to_string(entry.path()).unwrap().contains(pattern))
                .map(|entry| entry.file_name().into_string().unwrap())
                .collect();
            expected.sort();
            let commands = vec![
                ("strict", self.command(false)),
                ("checked", self.checked_command(false, mode)),
            ];
            #[cfg(unix)]
            let commands = {
                let mut commands = commands;
                if matches!(mode, CheckedMode::Negative) {
                    let mut command = self.command(false);
                    command.env("FXI_NEGATIVE_ROUTING", "1");
                    commands.push(("negative-only", command));
                }
                commands
            };
            for (policy, mut command) in commands {
                let output = success(
                    command
                        .args(["-l", "-F", "--color=never", pattern, "."])
                        .output()
                        .unwrap(),
                );
                let mut actual: Vec<_> = output
                    .lines()
                    .map(|name| {
                        Path::new(name)
                            .file_name()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned()
                    })
                    .collect();
                actual.sort();
                assert_eq!(actual, expected, "{mode:?} {policy} {pattern}");
            }
        }
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
fn broken_object_references_fail_without_publishing() {
    let f = Fixture::new();
    f.ok(true, &["index"]);
    let current = fs::read(f.container().join("CURRENT")).unwrap();
    fs::write(f.root.join("new.rs"), "newMarker\n").unwrap();
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

#[test]
fn checked_stable_publications_preserve_all_inherited_object_bytes_and_times() {
    for profile in ["full", "lean"] {
        for mode in [CheckedMode::Local, CheckedMode::Generation] {
            let f = Fixture::new();
            f.checked_ok(
                true,
                mode,
                &["index", "--profile", profile, "--chunk-size", "8"],
            );
            assert_eq!(f.meta()["version"], if profile == "full" { 4 } else { 5 });
            f.assert_checked_evidence(mode);
            f.assert_source_queries(mode);
            let (original_generation, original_lease) = f.pin();
            let original_objects = f.snapshot_objects();

            fs::write(f.root.join("new.rs"), "newMarker shared\n").unwrap();
            fs::write(
                f.root.join("file1.rs"),
                "replacementMarker shared revision\n",
            )
            .unwrap();
            fs::rename(f.root.join("file2.rs"), f.root.join("renamed.rs")).unwrap();
            fs::write(f.root.join("renamed.rs"), "renamedMarker original\n").unwrap();
            // The persisted object layout survives removing the writer flag.
            f.checked_ok(false, mode, &["index"]);
            assert_ne!(f.generation(), original_generation);
            let inherited = f.object_paths();
            for object in original_objects.keys() {
                assert!(inherited.contains(object));
            }
            assert_objects_unchanged(&original_objects);
            f.assert_checked_evidence(mode);
            f.assert_source_queries(mode);

            // Pin all objects, including the just-added delta, through both a
            // deletion-only publication and compaction.
            let (updated_generation, updated_lease) = f.pin();
            let updated_objects = f.snapshot_objects();
            let references = f.meta()["segment_objects"].clone();
            fs::remove_file(f.root.join("file0.rs")).unwrap();
            f.checked_ok(false, mode, &["index"]);
            assert_eq!(f.meta()["segment_objects"], references);
            assert_objects_unchanged(&updated_objects);
            f.assert_checked_evidence(mode);
            f.assert_source_queries(mode);

            f.checked_ok(false, mode, &["compact"]);
            assert_eq!(f.meta()["segment_objects"].as_object().unwrap().len(), 1);
            assert!(original_generation.is_dir() && updated_generation.is_dir());
            assert_objects_unchanged(&updated_objects);
            f.assert_checked_evidence(mode);
            f.assert_source_queries(mode);

            drop((original_lease, updated_lease));
            fs::write(f.root.join("another.rs"), "newMarker another revision\n").unwrap();
            f.checked_ok(false, mode, &["index"]);
            assert!(!original_generation.exists() && !updated_generation.exists());
            for object in updated_objects.keys() {
                assert!(!object.exists(), "retired object {}", object.display());
            }
            f.assert_source_queries(mode);
        }
    }
}

#[test]
fn checked_migration_from_legacy_layout_preserves_pinned_segment_contents() {
    for profile in ["full", "lean"] {
        let f = Fixture::new();
        f.ok(false, &["index", "--profile", profile, "--chunk-size", "8"]);
        assert!(f.meta()["version"].as_u64().unwrap() < 4);
        let (old_generation, lease) = f.pin();
        let legacy = snapshot_tree(&old_generation.join("segments"));
        fs::write(f.root.join("new.rs"), "newMarker shared\n").unwrap();
        f.checked_ok(true, CheckedMode::Generation, &["index"]);
        assert_eq!(f.meta()["version"], if profile == "full" { 4 } else { 5 });
        f.assert_checked_evidence(CheckedMode::Generation);
        f.assert_source_queries(CheckedMode::Generation);
        let after = snapshot_tree(&old_generation.join("segments"));
        // Legacy migration may add hard links, which changes inode ctime;
        // its original directory listing and every file's bytes remain intact.
        assert_eq!(
            legacy
                .into_iter()
                .map(|(path, entry)| (path, entry.bytes))
                .collect::<BTreeMap<_, _>>(),
            after
                .into_iter()
                .map(|(path, entry)| (path, entry.bytes))
                .collect::<BTreeMap<_, _>>()
        );
        assert!(old_generation.is_dir());
        drop(lease);
    }
}

#[test]
fn enabling_checked_mode_leaves_unchecked_stable_objects_unchanged_until_compaction() {
    for profile in ["full", "lean"] {
        let f = Fixture::new();
        f.ok(true, &["index", "--profile", profile, "--chunk-size", "8"]);
        let (old_generation, lease) = f.pin();
        let objects = f.snapshot_objects();
        for object in objects.keys() {
            assert!(!object.join("grams.checks").exists());
            assert!(!object.join("grams.bloom-check").exists());
        }
        fs::write(f.root.join("new.rs"), "newMarker shared\n").unwrap();
        f.checked_ok(false, CheckedMode::Local, &["index"]);
        assert_objects_unchanged(&objects);
        assert!(
            f.object_paths()
                .iter()
                .any(|object| object.join("grams.checks").is_file())
        );
        assert!(
            !f.generation().join("query-routing.bin").exists(),
            "incomplete segment proof coverage must use validation fallback"
        );
        f.assert_source_queries(CheckedMode::Local);

        fs::remove_file(f.root.join("file0.rs")).unwrap();
        f.checked_ok(false, CheckedMode::Local, &["index"]);
        assert_objects_unchanged(&objects);
        f.assert_source_queries(CheckedMode::Local);
        f.checked_ok(false, CheckedMode::Local, &["compact"]);
        f.assert_checked_evidence(CheckedMode::Local);
        f.assert_source_queries(CheckedMode::Local);
        assert!(old_generation.is_dir());
        assert_objects_unchanged(&objects);
        drop(lease);
    }
}

#[test]
fn missing_checked_sidecars_fall_back_without_mutating_inherited_objects() {
    for profile in ["full", "lean"] {
        let f = Fixture::new();
        let mode = CheckedMode::Generation;
        f.checked_ok(
            true,
            mode,
            &["index", "--profile", profile, "--chunk-size", "8"],
        );
        f.assert_checked_evidence(mode);
        for name in ["query-routing.bin", "generation-routing.bin"] {
            fs::remove_file(f.generation().join(name)).unwrap();
        }
        let first = f.object_paths().remove(0);
        fs::remove_file(first.join("grams.checks")).unwrap();
        fs::remove_file(first.join("grams.bloom-check")).unwrap();
        let (_generation, lease) = f.pin();
        let objects = f.snapshot_objects();
        f.assert_source_queries(mode);
        fs::write(f.root.join("new.rs"), "newMarker shared\n").unwrap();
        f.checked_ok(false, mode, &["index"]);
        assert_objects_unchanged(&objects);
        assert!(!first.join("grams.checks").exists());
        f.assert_source_queries(mode);
        f.checked_ok(false, mode, &["compact"]);
        assert_objects_unchanged(&objects);
        f.assert_checked_evidence(mode);
        f.assert_source_queries(mode);
        drop(lease);
    }
}

#[test]
fn corrupt_checked_stable_evidence_fails_without_changing_current() {
    for profile in ["full", "lean"] {
        for damaged in ["grams.checks", "grams.postings"] {
            let f = Fixture::new();
            f.checked_ok(
                true,
                CheckedMode::Local,
                &["index", "--profile", profile, "--chunk-size", "8"],
            );
            let current = fs::read(f.container().join("CURRENT")).unwrap();
            let object = f.object_paths().remove(0);
            fs::write(object.join(damaged), b"damaged").unwrap();
            fs::write(f.root.join("new.rs"), "newMarker\n").unwrap();
            for args in [vec!["index"], vec!["compact"], vec!["stats"]] {
                let output = f
                    .checked_command(false, CheckedMode::Local)
                    .args(&args)
                    .output()
                    .unwrap();
                assert!(!output.status.success(), "{profile} {damaged} {args:?}");
                assert_eq!(fs::read(f.container().join("CURRENT")).unwrap(), current);
            }
            // The independent absence proof need not read an unrelated posting
            // payload. Malformed gram roots, however, must reject both paths.
            let patterns: &[&str] = if damaged == "grams.checks" {
                &["shared", "absentMarker94283"]
            } else {
                &["shared"]
            };
            for pattern in patterns {
                let output = f
                    .checked_command(false, CheckedMode::Local)
                    .args(["-l", "-F", pattern])
                    .output()
                    .unwrap();
                assert!(!output.status.success(), "{profile} {damaged} {pattern}");
                assert!(output.stdout.is_empty());
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn checked_negative_routing_and_source_packs_preserve_stable_objects() {
    for profile in ["full", "lean"] {
        let f = Fixture::new();
        let mode = CheckedMode::Negative;
        success(
            f.checked_command(true, mode)
                .env("FXI_SOURCE_PACK", "1")
                .env("FXI_SOURCE_PACK_COMPRESSION", "1")
                .args(["index", "--profile", profile, "--chunk-size", "8"])
                .output()
                .unwrap(),
        );
        f.assert_checked_evidence(mode);
        let (_generation, lease) = f.pin();
        let objects = f.snapshot_objects();
        for object in objects.keys() {
            assert!(object.join("source.table").is_file());
        }
        f.assert_source_queries(mode);
        fs::write(f.root.join("new.rs"), "packedMarker shared\n").unwrap();
        f.checked_ok(false, mode, &["index"]);
        assert_objects_unchanged(&objects);
        f.assert_checked_evidence(mode);
        f.assert_source_queries(mode);
        fs::remove_file(f.root.join("file0.rs")).unwrap();
        f.checked_ok(false, mode, &["index"]);
        assert_objects_unchanged(&objects);
        f.assert_source_queries(mode);
        f.checked_ok(false, mode, &["compact"]);
        assert_objects_unchanged(&objects);
        f.assert_checked_evidence(mode);
        f.assert_source_queries(mode);
        for object in f.object_paths() {
            assert!(object.join("source.table").is_file());
        }
        drop(lease);
    }
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
