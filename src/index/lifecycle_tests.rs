//! Process-kill validation. This module and all its call sites are absent from
//! non-test builds. The child signals an exact boundary and the parent kills it;
//! unlike panic injection, no Rust destructor gets an opportunity to clean up.
use super::{reader::IndexReader, types::IndexProfile};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub(crate) fn checkpoint(point: &str) {
    if std::env::var("FXI_TEST_CRASH_AT").as_deref() != Ok(point) {
        return;
    }
    let marker = std::env::var_os("FXI_TEST_CRASH_READY").expect("child marker required");
    let mut file = File::create(marker).unwrap();
    file.write_all(point.as_bytes()).unwrap();
    file.sync_all().unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "helper executed only by the process-crash parent test"]
fn crash_writer_process() {
    let root = PathBuf::from(std::env::var_os("FXI_TEST_CRASH_ROOT").expect("child root required"));
    let _writer = crate::utils::IndexLock::acquire(&root).unwrap();
    match std::env::var("FXI_TEST_CRASH_OPERATION").unwrap().as_str() {
        "update" => {
            super::build::reconcile_index(&root, None, 100).unwrap();
        }
        "compact" => super::compact::merge_segments(&root).unwrap(),
        "rebuild" | "initial" => {
            let profile = if std::env::var("FXI_TEST_PROFILE").unwrap() == "lean" {
                IndexProfile::Lean
            } else {
                IndexProfile::Full
            };
            super::build::build_index_with_profile(&root, true, true, Some(8), profile).unwrap();
        }
        operation => panic!("unknown child operation {operation}"),
    }
}

struct KilledChild(Child);
impl Drop for KilledChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    runtime: PathBuf,
    profile: &'static str,
}
impl Fixture {
    fn new(profile: &'static str, initial: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join(".git")).unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        for id in 0..32 {
            fs::write(
                root.join(format!("file{id}.txt")),
                format!("steadyMarker unique{id}\noriginal content\n"),
            )
            .unwrap();
        }
        let root = root.canonicalize().unwrap();
        let fixture = Self {
            _temp: temp,
            root,
            runtime,
            profile,
        };
        if !initial {
            fixture.run_child("rebuild", None);
        }
        fixture
    }
    fn command(&self, operation: &str, point: Option<&str>) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "index::lifecycle_tests::crash_writer_process",
                "--ignored",
                "--nocapture",
            ])
            .env("FXI_TEST_CRASH_ROOT", &self.root)
            .env("FXI_TEST_CRASH_OPERATION", operation)
            .env("FXI_TEST_PROFILE", self.profile)
            .env(
                "FXI_INDEXES",
                crate::utils::app_data::indexes_path().unwrap(),
            )
            .env("FXI_STABLE_SEGMENTS", "1")
            .env("FXI_QUERY_LOCAL", "0")
            .env("FXI_NEGATIVE_ROUTING", "0")
            .env("FXI_GENERATION_ROUTING", "0")
            .env("FXI_SOURCE_PACK", "0")
            .env("TMPDIR", &self.runtime)
            .env("TMP", &self.runtime)
            .env("TEMP", &self.runtime);
        command
            .env_remove("FXI_TEST_CRASH_AT")
            .env_remove("FXI_TEST_CRASH_READY");
        if let Some(point) = point {
            command
                .env("FXI_TEST_CRASH_AT", point)
                .env("FXI_TEST_CRASH_READY", self.runtime.join("ready"));
        }
        command
    }
    fn run_child(&self, operation: &str, point: Option<&str>) {
        let mut command = self.command(operation, point);
        let log = self.runtime.join("child.log");
        let output = File::create(&log).unwrap();
        command
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output));
        let mut child = KilledChild(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(point) = point
                && fs::read_to_string(self.runtime.join("ready")).is_ok_and(|ready| ready == point)
            {
                assert_eq!(
                    fs::read_to_string(self.runtime.join("ready")).unwrap(),
                    point
                );
                child.0.kill().unwrap();
                assert!(!child.0.wait().unwrap().success());
                fs::remove_file(self.runtime.join("ready")).unwrap();
                break;
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(
                    point.is_none() && status.success(),
                    "{operation} {point:?}: {}",
                    fs::read_to_string(&log).unwrap()
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child timeout at {point:?}: {}",
                fs::read_to_string(&log).unwrap()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn container(&self) -> PathBuf {
        crate::utils::app_data::get_index_container(&self.root).unwrap()
    }
    fn mutate(&self) {
        fs::write(
            self.root.join("file0.txt"),
            "steadyMarker replacementMarker longer revision\n",
        )
        .unwrap();
        fs::remove_file(self.root.join("file1.txt")).unwrap();
        fs::rename(self.root.join("file2.txt"), self.root.join("renamed.txt")).unwrap();
        fs::write(self.root.join("added.txt"), "steadyMarker addedMarker\n").unwrap();
    }
    fn verify_source(&self) -> usize {
        let reader = IndexReader::open(&self.root).unwrap();
        let patterns = [
            "steadyMarker",
            "original",
            "replacementMarker",
            "addedMarker",
            "absentMarker",
        ];
        for pattern in patterns {
            let expected: BTreeSet<_> = fs::read_dir(&self.root)
                .unwrap()
                .map(|e| e.unwrap())
                .filter(|e| e.file_type().unwrap().is_file())
                .filter(|e| fs::read_to_string(e.path()).unwrap().contains(pattern))
                .map(|e| PathBuf::from(e.file_name()))
                .collect();
            let observed: BTreeSet<_> = crate::query::QueryExecutor::new(&reader)
                .execute_files_only(&crate::query::parse_query(pattern), 0)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(observed, expected, "{} {pattern}", self.profile);
        }
        patterns.len()
    }
    fn verify_reclaimed(&self) {
        let container = self.container();
        let generations: Vec<_> = fs::read_dir(container.join("generations"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(
            generations.len(),
            1,
            "abandoned generations survived recovery: {generations:?}"
        );
        let meta: super::types::IndexMeta =
            serde_json::from_slice(&fs::read(generations[0].join("meta.json")).unwrap()).unwrap();
        let expected: BTreeSet<_> = meta.segment_objects.into_values().collect();
        let actual: BTreeSet<_> = fs::read_dir(container.join("objects"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(actual, expected, "unreachable objects survived recovery");
    }
}

#[test]
fn process_kills_preserve_a_complete_generation_and_recover_orphans() {
    let mut cases = Vec::new();
    let before = [
        "generation_created",
        "object_installed",
        "objects_synced",
        "manifest_written",
        "generation_synced",
        "current_file_synced",
    ];
    let after = [
        "current_renamed",
        "current_directory_synced",
        "generations_retired",
        "objects_marked",
    ];
    for profile in ["full", "lean"] {
        for operation in ["initial", "update", "compact", "rebuild"] {
            let mut points = before.to_vec();
            points.extend(after);
            if operation == "compact" {
                points.push("object_deleted");
            }
            for point in points {
                let fixture = Fixture::new(profile, operation == "initial");
                let pointer = fs::read(fixture.container().join("CURRENT")).ok();
                if operation == "update" {
                    fixture.mutate();
                }
                fixture.run_child(operation, Some(point));
                let published = fs::read(fixture.container().join("CURRENT")).ok();
                if before.contains(&point) {
                    assert_eq!(published, pointer, "{operation} {point}");
                } else {
                    assert!(published.is_some());
                    assert_ne!(published, pointer, "{operation} {point}");
                }
                // Check the interrupted writer's published data before a
                // successful rebuild can hide a lost update or wrong result.
                let pre_recovery_queries = if published != pointer {
                    fixture.verify_source()
                } else {
                    0
                };
                if published.is_some() {
                    let reader = IndexReader::open(&fixture.root).unwrap();
                    assert_eq!(reader.valid_doc_ids().len(), 32, "{operation} {point}");
                } else {
                    assert!(IndexReader::open(&fixture.root).is_err());
                }
                fixture.run_child("rebuild", None);
                let recovery_queries = fixture.verify_source();
                fixture.verify_reclaimed();
                cases.push(serde_json::json!({
                    "profile": profile,
                    "operation": operation,
                    "boundary": point,
                    "published_new": published != pointer,
                    "pre_recovery_source_queries_verified": pre_recovery_queries,
                    "recovery_source_queries_verified": recovery_queries,
                    "source_queries_verified": pre_recovery_queries + recovery_queries,
                    "reclamation_verified": true,
                }));
                crate::utils::remove_index(&fixture.root).unwrap();
            }
        }
    }
    if let Some(path) = std::env::var_os("FXI_LIFECYCLE_REPORT") {
        let report = serde_json::json!({"kind":"process_termination_not_power_loss", "os":std::env::consts::OS, "arch":std::env::consts::ARCH, "cases":cases});
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}

#[test]
fn concurrent_readers_and_pinned_snapshots_survive_two_root_publications() {
    struct StopReaders(Arc<AtomicBool>);
    impl Drop for StopReaders {
        fn drop(&mut self) {
            // Also release readers when the writer panics or its child fails.
            self.0.store(true, Ordering::Release);
        }
    }

    let full = Fixture::new("full", false);
    let lean = Fixture::new("lean", false);
    let pinned_full = IndexReader::open(&full.root).unwrap();
    let pinned_lean = IndexReader::open(&lean.root).unwrap();
    let old_full = pinned_full.generation_path().to_path_buf();
    let old_lean = pinned_lean.generation_path().to_path_buf();
    std::thread::scope(|scope| {
        for fixture in [&full, &lean] {
            let stop = Arc::new(AtomicBool::new(false));
            let writer_stop = Arc::clone(&stop);
            let (observed_tx, observed_rx) = mpsc::channel::<(usize, PathBuf)>();
            let initial = super::generation::resolve(&fixture.container()).unwrap();
            scope.spawn(move || {
                let _stop = StopReaders(writer_stop);
                let wait_for_readers = |expected: &PathBuf| {
                    let deadline = Instant::now() + Duration::from_secs(45);
                    let mut observed = [false; 2];
                    while !observed.iter().all(|seen| *seen) {
                        let (reader, generation) = observed_rx
                            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                            .unwrap_or_else(|error| {
                                panic!(
                                    "{} readers did not observe {}: {error}; acknowledgments={observed:?}",
                                    fixture.profile,
                                    expected.display(),
                                )
                            });
                        assert_eq!(&generation, expected, "reader {reader}");
                        assert!(!observed[reader], "duplicate reader acknowledgment");
                        observed[reader] = true;
                    }
                };
                // Both reader loops must query the original snapshot before
                // publication starts, and remain active until every subsequent
                // generation has been observed by both of them.
                wait_for_readers(&initial);
                let mut previous = initial;
                for step in 0..8 {
                    fixture.run_child(if step % 2 == 0 { "rebuild" } else { "compact" }, None);
                    let published = super::generation::resolve(&fixture.container()).unwrap();
                    assert_ne!(published, previous, "publication {step} was a no-op");
                    wait_for_readers(&published);
                    previous = published;
                }
            });
            for reader_id in 0..2 {
                let stop = Arc::clone(&stop);
                let observed_tx = observed_tx.clone();
                scope.spawn(move || {
                    let expected: BTreeSet<_> = (0..32)
                        .map(|id| PathBuf::from(format!("file{id}.txt")))
                        .collect();
                    let mut previous = None;
                    while !stop.load(Ordering::Acquire) {
                        let reader = IndexReader::open_for_search(&fixture.root).unwrap();
                        let matches = crate::query::QueryExecutor::new(&reader)
                            .execute_files_only(&crate::query::parse_query("steadyMarker"), 0)
                            .unwrap();
                        let actual: BTreeSet<_> = matches.into_iter().collect();
                        assert_eq!(actual, expected);
                        let generation = reader.generation_path();
                        if previous.as_deref() != Some(generation) {
                            if observed_tx
                                .send((reader_id, generation.to_path_buf()))
                                .is_err()
                            {
                                return;
                            }
                            previous = Some(generation.to_path_buf());
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                });
            }
            // A panic in both readers disconnects the channel immediately.
            // A single failed reader is caught by the bounded acknowledgment
            // wait; the writer's drop guard then stops the surviving reader.
            drop(observed_tx);
        }
    });
    assert!(old_full.is_dir() && old_lean.is_dir());
    assert_eq!(pinned_full.get_token_docs("original").unwrap().len(), 32);
    for doc in pinned_full.documents() {
        assert!(pinned_full.get_line_map(doc.doc_id).unwrap().is_some());
    }
    for reader in [&pinned_full, &pinned_lean] {
        assert_eq!(
            crate::query::QueryExecutor::new(reader)
                .execute_files_only(&crate::query::parse_query("steadyMarker"), 0)
                .unwrap()
                .len(),
            32
        );
    }
    drop((pinned_full, pinned_lean));
    for fixture in [&full, &lean] {
        fixture.run_child("rebuild", None);
        fixture.verify_source();
        fixture.verify_reclaimed();
        crate::utils::remove_index(&fixture.root).unwrap();
    }
    assert!(!old_full.exists() && !old_lean.exists());
}
