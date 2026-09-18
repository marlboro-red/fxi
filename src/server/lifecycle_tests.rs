//! Deterministic lifecycle regressions; no process-global environment changes.
use super::*;
use crate::server::watcher::FileChange;

struct Fixture {
    server: Arc<IndexServer>,
    root: PathBuf,
    _directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self::with_files(12, "baseline lifecycle content\n")
    }
    fn with_files(count: usize, content: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for i in 0..count {
            std::fs::write(root.join(format!("{i}.rs")), content).unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        Self {
            server,
            root,
            _directory: directory,
        }
    }
    fn change(&self, path: &str, content: &str) {
        std::fs::write(self.root.join(path), content).unwrap();
        self.server
            .accumulate_changes(self.root.clone(), changed(path));
    }
    fn disk_paths(&self, marker: &str) -> Vec<PathBuf> {
        QueryExecutor::new(&IndexReader::open(&self.root).unwrap())
            .execute_files_only(&crate::query::parse_query(marker), 0)
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.shutdown.store(true, Ordering::Release);
        self.server.stop_all_watchers();
        let _ = crate::utils::remove_index(&self.root);
    }
}
fn changed(path: &str) -> ChangeBatch {
    let mut batch = ChangeBatch::new();
    batch.add(FileChange {
        path: path.into(),
        kind: ChangeKind::Modified,
    });
    batch
}
fn shutdown_succeeded(server: &IndexServer) -> bool {
    matches!(&*server.shutdown_result.0.lock().unwrap(), Some(Ok(())))
}

#[test]
fn shutdown_joins_producers_and_persists_their_final_messages() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("final.rs"), "lastProducerMarker\n").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let producer_stop = stop.clone();
    let tx = fixture.server.watcher_tx.clone();
    let root = fixture.root.clone();
    let producer = thread::spawn(move || {
        while !producer_stop.load(Ordering::Acquire) {
            thread::yield_now();
        }
        tx.send(WatcherMessage::ChangesReady {
            root_path: root,
            batch: changed("final.rs"),
        })
        .unwrap();
    });
    let cached = fixture.server.indexes.read().unwrap()[&fixture.root].clone();
    *cached.watcher_handle.lock().unwrap() =
        Some(WatcherHandle::new(stop, producer, fixture.root.clone()));
    fixture.server.shutdown.store(true, Ordering::Release);
    fixture.server.run_watcher_processor();
    assert!(shutdown_succeeded(&fixture.server));
    assert!(!cached.is_watching());
    assert!(fixture.server.pending_changes.lock().unwrap().is_empty());
    assert_eq!(
        fixture.disk_paths("lastProducerMarker"),
        vec![PathBuf::from("final.rs")]
    );
}

#[test]
fn shutdown_retries_writer_contention_before_acknowledging_success() {
    let fixture = Fixture::new();
    fixture.change("new.rs", "contentionShutdownMarker\n");
    let held = crate::utils::IndexLock::acquire(&fixture.root).unwrap();
    fixture.server.shutdown.store(true, Ordering::Release);
    let server = fixture.server.clone();
    let (done, completion) = mpsc::channel();
    let worker = thread::spawn(move || {
        server.run_watcher_processor();
        done.send(()).unwrap();
    });
    // A success response is impossible while the already pending work cannot
    // acquire its publication lock, regardless of worker scheduling.
    assert!(completion.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(!shutdown_succeeded(&fixture.server));
    drop(held);
    completion.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(shutdown_succeeded(&fixture.server));
    assert_eq!(
        fixture.disk_paths("contentionShutdownMarker"),
        vec![PathBuf::from("new.rs")]
    );
}

#[test]
fn shutdown_rebuild_does_not_register_another_watcher() {
    let mut fixture = Fixture::new();
    Arc::get_mut(&mut fixture.server).unwrap().watch_enabled = true;
    fixture.change("0.rs", "recoveryShutdownMarker\n");
    let generation = crate::utils::get_index_dir(&fixture.root).unwrap();
    std::fs::write(generation.join("meta.json"), "broken").unwrap();
    fixture.server.shutdown.store(true, Ordering::Release);
    fixture.server.run_watcher_processor();
    assert!(shutdown_succeeded(&fixture.server));
    let cached = fixture.server.indexes.read().unwrap()[&fixture.root].clone();
    assert!(!cached.is_watching());
    assert_eq!(
        fixture.disk_paths("recoveryShutdownMarker"),
        vec![PathBuf::from("0.rs")]
    );
}

#[test]
fn a_load_waiting_on_lifecycle_is_rejected_after_shutdown_starts() {
    let fixture = Fixture::new();
    fixture
        .server
        .indexes
        .write()
        .unwrap()
        .remove(&fixture.root);
    let gate = fixture.server.lifecycle_for(&fixture.root);
    let held = gate.lock().unwrap();
    let server = fixture.server.clone();
    let root = fixture.root.clone();
    let (started, ready) = mpsc::channel();
    let worker = thread::spawn(move || {
        started.send(()).unwrap();
        server.ensure_index_loaded(&root)
    });
    ready.recv().unwrap();
    fixture.server.shutdown.store(true, Ordering::Release);
    drop(held);
    assert!(worker.join().unwrap().is_err());
    assert!(
        !fixture
            .server
            .indexes
            .read()
            .unwrap()
            .contains_key(&fixture.root)
    );
}

#[test]
fn queued_notifications_cannot_recreate_a_removed_root() {
    let removed = Fixture::new();
    let other = Fixture::new();
    removed.server.ensure_index_loaded(&other.root).unwrap();
    let old_other = crate::utils::get_index_dir(&other.root).unwrap();
    assert!(matches!(
        removed.server.handle_remove(removed.root.clone()),
        Response::Reloaded { success: true, .. }
    ));
    let tx = &removed.server.watcher_tx;
    tx.send(WatcherMessage::RequestRebuild {
        root_path: removed.root.clone(),
        reason: "late producer".into(),
    })
    .unwrap();
    tx.send(WatcherMessage::ChangesReady {
        root_path: removed.root.clone(),
        batch: changed("0.rs"),
    })
    .unwrap();
    tx.send(WatcherMessage::Error {
        root_path: removed.root.clone(),
        message: "late error".into(),
    })
    .unwrap();
    std::fs::write(other.root.join("0.rs"), "otherRootContinuesMarker\n").unwrap();
    tx.send(WatcherMessage::ChangesReady {
        root_path: other.root.clone(),
        batch: changed("0.rs"),
    })
    .unwrap();
    let server = removed.server.clone();
    let worker = thread::spawn(move || server.run_watcher_processor());
    let deadline = Instant::now() + Duration::from_secs(5);
    while crate::utils::get_index_dir(&other.root).unwrap() == old_other
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(5));
    }
    removed.server.shutdown.store(true, Ordering::Release);
    worker.join().unwrap();
    assert_ne!(crate::utils::get_index_dir(&other.root).unwrap(), old_other);
    assert!(!crate::utils::is_indexed(&removed.root).unwrap());
    assert!(
        !removed
            .server
            .indexes
            .read()
            .unwrap()
            .contains_key(&removed.root)
    );
    assert!(
        !removed
            .server
            .pending_changes
            .lock()
            .unwrap()
            .contains_key(&removed.root)
    );
}

#[test]
fn reloading_retains_and_recomputes_an_already_visible_preview() {
    let fixture = Fixture::new();
    fixture.change("0.rs", "reloadPreviewMarker\n");
    fixture
        .server
        .flush_pending_changes_mode(&fixture.root, false);
    let cached = fixture.server.indexes.read().unwrap()[&fixture.root].clone();
    assert!(fixture.disk_paths("reloadPreviewMarker").is_empty());
    assert!(!fixture.server.pending_changes.lock().unwrap()[&fixture.root].needs_visibility);
    let response = fixture.server.handle_reload(Some(fixture.root.clone()));
    assert!(matches!(response, Response::Reloaded { success: true, .. }));
    let live = cached.get_reader();
    assert_eq!(
        QueryExecutor::new(&live)
            .execute_files_only(&crate::query::parse_query("reloadPreviewMarker"), 0)
            .unwrap(),
        vec![PathBuf::from("0.rs")]
    );
    assert_eq!(
        live.meta.doc_count,
        cached.get_durable_reader().meta.doc_count + 1
    );
    assert!(!fixture.server.pending_changes.lock().unwrap()[&fixture.root].needs_visibility);
}

#[test]
fn ranked_wire_limits_override_defaults_and_intersect_explicit_top() {
    let fixture = Fixture::with_files(180, "rankLimitMarker\n");
    for (query, wire_limit, expected) in [
        ("rankLimitMarker", 150, 150),
        ("rankLimitMarker top:7", 150, 7),
        ("rankLimitMarker top:200", 150, 150),
        ("rankLimitMarker top:7", 0, 7),
        ("rankLimitMarker", 0, 180),
        ("rankLimitMarker top:0", 150, 150),
    ] {
        let response =
            fixture
                .server
                .handle_search(query.into(), Some(fixture.root.clone()), wire_limit);
        let Response::Search(response) = response else {
            panic!("{response:?}");
        };
        assert_eq!(
            response.matches.len(),
            expected,
            "query={query}, wire={wire_limit}"
        );
    }
}

#[test]
fn status_reports_live_documents_after_a_tombstoning_update() {
    let fixture = Fixture::new();
    fixture.change("0.rs", "statusUpdatedMarker\n");
    fixture.server.flush_pending_changes(&fixture.root);
    let reader = fixture.server.indexes.read().unwrap()[&fixture.root].get_reader();
    assert_eq!(reader.meta.doc_count, 13);
    assert_eq!(reader.meta.tombstone_count, 1);
    let Response::Status(status) = fixture.server.handle_status() else {
        panic!("expected status");
    };
    assert_eq!(status.total_docs, 12);
}

#[test]
fn requested_subtree_and_file_scopes_intersect_existing_query_filters() {
    let fixture = Fixture::new();
    for path in ["sub/a.rs", "sub/b.txt", "submarine/c.rs", "other/d.rs"] {
        let path = fixture.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "scopeApiMarker\n").unwrap();
    }
    build_index_with_progress(&fixture.root, true, true).unwrap();
    assert!(matches!(
        fixture.server.handle_reload(Some(fixture.root.clone())),
        Response::Reloaded { success: true, .. }
    ));
    for (scope, expected) in [
        ("sub", vec![PathBuf::from("sub/a.rs")]),
        ("sub/b.txt", vec![]),
    ] {
        let root = Some(fixture.root.join(scope));
        let ranked = fixture
            .server
            .handle_search("scopeApiMarker ext:rs".into(), root.clone(), 0);
        let Response::Search(ranked) = ranked else {
            panic!("{ranked:?}");
        };
        assert_eq!(ranked.resolved_root, Some(fixture.root.clone()));
        assert_eq!(
            ranked
                .matches
                .into_iter()
                .map(|hit| hit.path)
                .collect::<Vec<_>>(),
            expected
        );
        let content = fixture.server.handle_content_search(
            "scopeApiMarker ext:rs".into(),
            root,
            0,
            ContentSearchOptions {
                files_only: true,
                compact_files: true,
                ..Default::default()
            },
        );
        let Response::ContentSearch(content) = content else {
            panic!("{content:?}");
        };
        assert_eq!(content.file_paths.unwrap(), expected);
    }
}

#[cfg(unix)]
#[test]
fn reload_reports_unapplied_visibility_without_discarding_the_previous_preview() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.change("0.rs", "retainedReloadMarker\n");
    fixture
        .server
        .flush_pending_changes_mode(&fixture.root, false);
    let cached = fixture.server.indexes.read().unwrap()[&fixture.root].clone();
    let preview = cached.get_reader();
    let path = fixture.root.join("0.rs");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
    if std::fs::File::open(&path).is_ok() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        return; // privileged runners bypass file mode restrictions
    }
    let response = fixture.server.handle_reload(Some(fixture.root.clone()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        matches!(response, Response::Reloaded { success: false, .. }),
        "{response:?}"
    );
    assert!(Arc::ptr_eq(&preview, &cached.get_reader()));
    assert!(fixture.server.pending_changes.lock().unwrap()[&fixture.root].needs_visibility);
    let retry = fixture.server.handle_reload(Some(fixture.root.clone()));
    assert!(
        matches!(retry, Response::Reloaded { success: true, .. }),
        "{retry:?}"
    );
    assert!(!fixture.server.pending_changes.lock().unwrap()[&fixture.root].needs_visibility);
}

#[test]
fn edits_arriving_during_publication_remain_pending_until_reconciled() {
    for persist in [false, true] {
        let fixture = Fixture::new();
        fixture.change("0.rs", "firstPublicationMarker\n");
        let cached = fixture.server.indexes.read().unwrap()[&fixture.root].clone();
        // Block the live-reader swap, so the publisher cannot finish after
        // capturing its batch and before we deliver the next notification.
        let held_reader = cached.reader.lock().unwrap();
        let server = fixture.server.clone();
        let root = fixture.root.clone();
        let worker = thread::spawn(move || server.flush_pending_changes_mode(&root, persist));
        let deadline = Instant::now() + Duration::from_secs(5);
        while fixture
            .server
            .pending_changes
            .lock()
            .unwrap()
            .contains_key(&fixture.root)
        {
            assert!(Instant::now() < deadline, "publisher did not capture batch");
            thread::yield_now();
        }
        // Include both a repeated path and a previously unseen path. The first
        // snapshot may have read either version; the final result must be latest.
        fixture.change("0.rs", "latestPublicationMarker\n");
        fixture.change("late.rs", "latestPublicationMarker\n");
        drop(held_reader);
        worker.join().unwrap();
        {
            let pending = fixture.server.pending_changes.lock().unwrap();
            let pending = &pending[&fixture.root];
            assert!(pending.needs_visibility);
            assert!(pending.batch.modified.contains(&PathBuf::from("0.rs")));
            assert!(pending.batch.modified.contains(&PathBuf::from("late.rs")));
        }
        fixture.server.flush_pending_changes(&fixture.root);
        assert!(fixture.server.pending_changes.lock().unwrap().is_empty());
        let expected = vec![PathBuf::from("0.rs"), PathBuf::from("late.rs")];
        assert_eq!(fixture.disk_paths("latestPublicationMarker"), expected);
        let live = cached.get_reader();
        assert_eq!(
            QueryExecutor::new(&live)
                .execute_files_only(&crate::query::parse_query("latestPublicationMarker"), 0)
                .unwrap(),
            expected
        );
        assert!(fixture.disk_paths("firstPublicationMarker").is_empty());
    }
}

#[test]
fn removal_waits_for_inflight_publication_and_discards_late_notifications() {
    let fixture = Fixture::new();
    fixture.change("0.rs", "removedPublicationMarker\n");
    let cached = fixture.server.indexes.read().unwrap()[&fixture.root].clone();
    let held_reader = cached.reader.lock().unwrap();
    let server = fixture.server.clone();
    let root = fixture.root.clone();
    let publisher = thread::spawn(move || server.flush_pending_changes(&root));
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture
        .server
        .pending_changes
        .lock()
        .unwrap()
        .contains_key(&fixture.root)
    {
        assert!(Instant::now() < deadline, "publisher did not capture batch");
        thread::yield_now();
    }
    let server = fixture.server.clone();
    let root = fixture.root.clone();
    let (done, completion) = mpsc::channel();
    let remover = thread::spawn(move || done.send(server.handle_remove(root)).unwrap());
    // Publication owns the writer lock: removal cannot acknowledge success
    // while its live-reader swap is blocked, even if the remover starts late.
    assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
    fixture.change("late.rs", "removedPublicationMarker\n");
    drop(held_reader);
    publisher.join().unwrap();
    assert!(matches!(
        completion.recv_timeout(Duration::from_secs(5)).unwrap(),
        Response::Reloaded { success: true, .. }
    ));
    remover.join().unwrap();
    fixture
        .server
        .accumulate_changes(fixture.root.clone(), changed("late.rs"));
    fixture.server.flush_pending_changes(&fixture.root);
    assert!(!crate::utils::is_indexed(&fixture.root).unwrap());
    assert!(
        !fixture
            .server
            .indexes
            .read()
            .unwrap()
            .contains_key(&fixture.root)
    );
    assert!(
        !fixture
            .server
            .pending_changes
            .lock()
            .unwrap()
            .contains_key(&fixture.root)
    );
}
