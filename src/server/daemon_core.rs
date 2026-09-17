//! Platform-independent daemon core.
//!
//! Owns the loaded indexes, request handling, result caches, and watcher
//! orchestration. The platform transports (`daemon_unix`: Unix socket,
//! `daemon_windows`: named pipe) only accept connections, frame messages,
//! and call [`IndexServer::handle_request`].

use crate::index::build::{
    UpdateOutcome, build_index_with_progress, reconcile_index,
    reconcile_index_paths_with_visibility,
};
use crate::index::reader::IndexReader;
use crate::index::types::IndexMeta;
#[cfg(test)]
use crate::query::parse_query;
use crate::query::{QueryExecutor, try_parse_query};
use crate::server::debouncer::EventDebouncer;
use crate::server::protocol::{
    ContentMatch, ContentSearchOptions, ContentSearchResponse, PROTOCOL_VERSION, Request, Response,
    SearchMatchData, SearchResponse, StatusResponse,
};
use crate::server::watcher::{
    ChangeBatch, ChangeKind, WatcherConfig, WatcherHandle, WatcherMessage,
};
#[cfg(test)]
use crate::utils::get_index_dir;
use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// Compaction threshold: trigger merge when tombstone ratio exceeds this
const COMPACTION_TOMBSTONE_THRESHOLD: f32 = 0.15; // 15%

/// Maximum results to return to avoid exceeding message size limits
/// This caps unbounded (limit=0) requests to prevent excessive memory/transfer
/// Set very high since the protocol already has a 100MB message limit
const MAX_RESULTS_CAP: usize = 10_000_000;

/// Cached index with its query cache and optional file watcher
struct CachedIndex {
    /// Current reader (swapped atomically via Mutex)
    reader: Mutex<Arc<IndexReader>>,
    /// Last committed reader. A live memory snapshot must never be mistaken
    /// for persisted metadata when computing retries or subsequent deltas.
    durable_reader: Mutex<Arc<IndexReader>>,
    /// Last access time
    last_used: Mutex<Instant>,
    /// File watcher handle (if watching is active)
    watcher_handle: Mutex<Option<WatcherHandle>>,
}

impl CachedIndex {
    fn new(reader: IndexReader) -> Self {
        let reader = Arc::new(reader);
        Self {
            reader: Mutex::new(reader.clone()),
            durable_reader: Mutex::new(reader),
            last_used: Mutex::new(Instant::now()),
            watcher_handle: Mutex::new(None),
        }
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_used.lock() {
            *last = Instant::now();
        }
    }

    /// Get the current reader
    fn get_reader(&self) -> Arc<IndexReader> {
        self.reader.lock().unwrap().clone()
    }

    fn get_durable_reader(&self) -> Arc<IndexReader> {
        self.durable_reader.lock().unwrap().clone()
    }

    fn set_live_reader(&self, reader: IndexReader) {
        *self.reader.lock().unwrap() = Arc::new(reader);
    }

    /// Swap in a new reader, clearing caches
    fn set_pending_reader(&self, reader: IndexReader) {
        if self.is_watching() {
            reader.prepare_watched_paths();
        }
        let new_reader = Arc::new(reader);
        *self.durable_reader.lock().unwrap() = new_reader.clone();
        // Swap the reader
        if let Ok(mut current) = self.reader.lock() {
            *current = new_reader;
        }
    }

    /// Check if file watching is active
    fn is_watching(&self) -> bool {
        if let Ok(handle) = self.watcher_handle.lock() {
            handle.as_ref().is_some_and(|h| h.is_running())
        } else {
            false
        }
    }

    /// Stop the file watcher if running
    fn stop_watcher(&self) {
        if let Ok(mut handle) = self.watcher_handle.lock()
            && let Some(mut h) = handle.take()
        {
            h.stop();
        }
    }
}

/// Statistics for the server
struct ServerStats {
    start_time: Instant,
    queries_served: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
}

impl ServerStats {
    fn new() -> Self {
        Self {
            start_time: Instant::now(),
            queries_served: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    fn cache_hit_rate(&self) -> f32 {
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let misses = self.cache_misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total == 0 {
            0.0
        } else {
            hits as f32 / total as f32
        }
    }
}

/// Accumulated changes for an index awaiting flush to delta segment
struct PendingChanges {
    /// Accumulated change batch
    batch: ChangeBatch,
    /// Time of the first change in this batch
    first_change: Instant,
    last_change: Instant,
    needs_visibility: bool,
    retry_after: Option<Instant>,
}

/// The index server daemon
pub struct IndexServer {
    /// Loaded indexes by canonical root path. Values are Arc'd so request
    /// handlers can clone out an index and release the map lock instead of
    /// holding it for the duration of a query.
    indexes: RwLock<HashMap<PathBuf, Arc<CachedIndex>>>,
    lifecycle: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Server statistics
    stats: ServerStats,
    /// Shutdown flag
    pub(crate) shutdown: AtomicBool,
    pub(crate) shutdown_reply_sent: AtomicBool,
    shutdown_result: (Mutex<Option<std::result::Result<(), String>>>, Condvar),
    /// Channel for watcher messages
    watcher_tx: Sender<WatcherMessage>,
    /// Receiver for watcher messages (wrapped for thread safety)
    watcher_rx: Mutex<Receiver<WatcherMessage>>,
    /// Watcher configuration
    watcher_config: WatcherConfig,
    /// Accumulated changes per index (root_path -> pending changes)
    pending_changes: Mutex<HashMap<PathBuf, PendingChanges>>,
    /// Whether file watching is enabled
    watch_enabled: bool,
}

impl IndexServer {
    /// Create a new index server wrapped in Arc
    pub fn new(watch_enabled: bool) -> Arc<Self> {
        let (watcher_tx, watcher_rx) = mpsc::channel();
        let config = WatcherConfig::from_env();
        eprintln!(
            "fxid: config: watch={}, debounce={}ms, delta_flush={}s, merge_segments={}, rebuild_threshold={}%",
            watch_enabled,
            config.debounce_ms,
            config.delta_flush_interval_secs,
            config.merge_segment_threshold,
            config.rebuild_threshold_percent
        );
        Arc::new(Self {
            indexes: RwLock::new(HashMap::new()),
            lifecycle: Mutex::new(HashMap::new()),
            stats: ServerStats::new(),
            shutdown: AtomicBool::new(false),
            shutdown_reply_sent: AtomicBool::new(false),
            shutdown_result: (Mutex::new(None), Condvar::new()),
            watcher_tx,
            watcher_rx: Mutex::new(watcher_rx),
            watcher_config: config,
            pending_changes: Mutex::new(HashMap::new()),
            watch_enabled,
        })
    }

    /// Drain a bounded backlog before reconciling. One slow scan must not turn
    /// already queued notifications into a sequence of redundant full scans.
    fn receive_watcher_messages(&self) -> Vec<WatcherMessage> {
        let rx = self.watcher_rx.lock().unwrap();
        let Ok(first) = rx.recv_timeout(Duration::from_millis(100)) else {
            return Vec::new();
        };
        let mut messages = vec![first];
        messages.extend(rx.try_iter().take(1023));
        messages
    }

    /// Start the server (blocking)
    /// Run the watcher message processor
    pub(crate) fn run_watcher_processor(self: &Arc<Self>) {
        let flush_interval = self.watcher_config.delta_flush_duration();

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                // Join producers before consuming their final debounced batches.
                self.stop_all_watchers();
                let final_messages: Vec<_> = self.watcher_rx.lock().unwrap().try_iter().collect();
                for message in final_messages {
                    match message {
                        WatcherMessage::ChangesReady { root_path, batch } => {
                            self.accumulate_changes(root_path, batch)
                        }
                        WatcherMessage::RequestRebuild { root_path, .. }
                        | WatcherMessage::Error { root_path, .. } => {
                            let mut batch = ChangeBatch::new();
                            batch.add(crate::server::watcher::FileChange {
                                path: PathBuf::new(),
                                kind: ChangeKind::Modified,
                            });
                            self.accumulate_changes(root_path, batch);
                        }
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(20);
                while !self.pending_changes.lock().unwrap().is_empty() && Instant::now() < deadline
                {
                    let roots: Vec<_> = self
                        .pending_changes
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(_, pending)| {
                            pending.retry_after.is_none_or(|at| Instant::now() >= at)
                        })
                        .map(|(root, _)| root.clone())
                        .collect();
                    for root in roots {
                        self.flush_pending_changes(&root);
                    }
                    if !self.pending_changes.lock().unwrap().is_empty() {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                let result = if self.pending_changes.lock().unwrap().is_empty() {
                    Ok(())
                } else {
                    Err("Shutdown could not persist all pending updates; restart with --watch to reconcile, or resolve writer contention/read errors".to_string())
                };
                *self.shutdown_result.0.lock().unwrap() = Some(result);
                self.shutdown_result.1.notify_all();
                break;
            }

            for message in self.receive_watcher_messages() {
                match message {
                    WatcherMessage::ChangesReady { root_path, batch } => {
                        self.accumulate_changes(root_path, batch);
                    }
                    WatcherMessage::RequestRebuild { root_path, reason } => {
                        eprintln!(
                            "fxid: rebuild requested for {}: {}",
                            root_path.display(),
                            reason
                        );
                        self.queue_reconciliation(root_path);
                    }
                    WatcherMessage::Error { root_path, message } => {
                        eprintln!(
                            "fxid: watcher error for {}: {}",
                            root_path.display(),
                            message
                        );
                        self.queue_reconciliation(root_path);
                    }
                }
            }

            // Check for indexes that need flushing
            self.flush_expired_changes(flush_interval);
        }
    }

    fn queue_reconciliation(&self, root: PathBuf) {
        let mut batch = ChangeBatch::new();
        batch.add(crate::server::watcher::FileChange {
            path: PathBuf::new(),
            kind: ChangeKind::Modified,
        });
        self.accumulate_changes(root, batch);
    }

    /// Accumulate changes for an index
    fn accumulate_changes(&self, root_path: PathBuf, batch: ChangeBatch) {
        let indexes = self.indexes.read().unwrap();
        if batch.is_empty() || !indexes.contains_key(&root_path) {
            return;
        }

        let mut pending = self.pending_changes.lock().unwrap();

        if let Some(existing) = pending.get_mut(&root_path) {
            // Merge into existing batch
            existing.batch.merge(batch);
            existing.last_change = Instant::now();
            existing.needs_visibility = true;
        } else {
            // Create new pending entry
            pending.insert(
                root_path,
                PendingChanges {
                    batch,
                    first_change: Instant::now(),
                    last_change: Instant::now(),
                    needs_visibility: true,
                    retry_after: None,
                },
            );
        }
    }

    /// Flush changes for indexes where the flush interval has elapsed
    fn flush_expired_changes(&self, flush_interval: Duration) {
        // Collect indexes that need flushing
        let to_flush: Vec<(PathBuf, bool)> = {
            let pending = self.pending_changes.lock().unwrap();
            pending
                .iter()
                .filter_map(|(path, changes)| {
                    if changes
                        .retry_after
                        .is_some_and(|deadline| Instant::now() < deadline)
                    {
                        return None;
                    }
                    // Searches see small changes immediately. Persist after
                    // typing settles, with a maximum age to bound recovery work.
                    // Explicit nonzero flush intervals keep their original
                    // first-event scheduling contract.
                    let durable_due = if !self.watch_enabled || !flush_interval.is_zero() {
                        changes.first_change.elapsed() >= flush_interval
                    } else {
                        changes.last_change.elapsed() >= Duration::from_millis(250)
                            || changes.first_change.elapsed() >= Duration::from_secs(10)
                    };
                    if durable_due || changes.needs_visibility {
                        Some((path.clone(), durable_due))
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Flush each one
        for (root_path, persist) in to_flush {
            self.flush_pending_changes_mode(&root_path, persist);
        }
    }

    /// Flush all pending changes (used during shutdown)
    #[cfg(test)]
    pub(crate) fn flush_all_pending_changes(&self) {
        let paths: Vec<PathBuf> = {
            let pending = self.pending_changes.lock().unwrap();
            pending.keys().cloned().collect()
        };

        for root_path in paths {
            self.flush_pending_changes(&root_path);
        }
    }

    fn retry_pending_changes(&self, root_path: &PathBuf) {
        if let Some(pending) = self.pending_changes.lock().unwrap().get_mut(root_path) {
            pending.retry_after = Some(Instant::now() + Duration::from_secs(1));
        }
    }

    /// Acquire before removing work: contention must not stall other roots or
    /// lose a batch. Failed publication is retained for a bounded-rate retry.
    fn flush_pending_changes(&self, root_path: &PathBuf) {
        self.flush_pending_changes_mode(root_path, true);
    }

    fn flush_pending_changes_mode(&self, root_path: &PathBuf, persist: bool) {
        let lock = match crate::utils::IndexLock::try_acquire(root_path) {
            Ok(Some(lock)) => lock,
            Ok(None) => return,
            Err(error) => {
                eprintln!("fxid: cannot lock index; retaining pending changes: {error:#}");
                self.retry_pending_changes(root_path);
                return;
            }
        };
        let pending = self.pending_changes.lock().unwrap().remove(root_path);
        if let Some(mut pending) = pending
            && !pending.batch.is_empty()
        {
            match self.handle_changes(root_path.clone(), &pending.batch, &lock, !persist) {
                Ok(true) => {}
                Ok(false) => {
                    pending.needs_visibility = false;
                    self.restore_pending_changes(root_path, pending);
                }
                Err(error) => {
                    eprintln!("fxid: update failed; retaining pending changes: {error:#}");
                    self.restore_pending_changes(root_path, pending);
                    self.retry_pending_changes(root_path);
                }
            }
        }
    }

    fn restore_pending_changes(&self, root_path: &PathBuf, pending: PendingChanges) {
        let mut all = self.pending_changes.lock().unwrap();
        if let Some(newer) = all.get_mut(root_path) {
            newer.batch.merge(pending.batch);
            newer.first_change = newer.first_change.min(pending.first_change);
            // Newer notifications must get another visibility pass.
            newer.needs_visibility = true;
        } else {
            all.insert(root_path.clone(), pending);
        }
    }

    /// Handle a batch of file changes
    fn handle_changes(
        &self,
        root_path: PathBuf,
        batch: &ChangeBatch,
        lock: &crate::utils::IndexLock,
        preview_only: bool,
    ) -> Result<bool> {
        let total = batch.total_changes();
        if total == 0 {
            return Ok(true);
        }

        // Watcher messages are hints, not a count of changed documents. A
        // directory or ignore-rule event can affect any number of files, and
        // the startup sentinel may affect none. Let reconciliation measure the
        // real diff before applying the configured rebuild threshold.
        let paths: Vec<PathBuf> = batch
            .created
            .iter()
            .chain(&batch.modified)
            .chain(&batch.deleted)
            .cloned()
            .collect();
        self.apply_incremental_update_paths(&root_path, lock, Some(&paths), preview_only)
    }

    /// Apply an incremental update using delta segments
    #[cfg(test)]
    fn apply_incremental_update(
        &self,
        root_path: &PathBuf,
        lock: &crate::utils::IndexLock,
    ) -> Result<()> {
        self.apply_incremental_update_paths(root_path, lock, None, false)
            .map(|_| ())
    }

    fn apply_incremental_update_paths(
        &self,
        root_path: &PathBuf,
        lock: &crate::utils::IndexLock,
        paths: Option<&[PathBuf]>,
        preview_only: bool,
    ) -> Result<bool> {
        let trace = std::env::var_os("FXI_TRACE_UPDATES").is_some();
        let started = Instant::now();
        // Notifications are hints, not an authoritative file list. Reconcile
        // through the same walker as CLI indexing so directory renames, nested
        // ignore rules, removals and symlinks have identical semantics.
        let current = self
            .indexes
            .read()
            .unwrap()
            .get(root_path)
            .map(|cached| cached.get_durable_reader());
        let mut publish_visible = |reader| {
            if let Some(cached) = self.indexes.read().unwrap().get(root_path) {
                cached.set_live_reader(reader);
            }
            if trace {
                eprintln!(
                    "fxid: memory update visible after {:.3}ms",
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
        };
        let outcome = match paths {
            Some(paths) => reconcile_index_paths_with_visibility(
                root_path,
                current.as_deref(),
                self.watcher_config.rebuild_threshold_percent,
                paths,
                &mut publish_visible,
                preview_only,
            ),
            None => reconcile_index(
                root_path,
                current.as_deref(),
                self.watcher_config.rebuild_threshold_percent,
            ),
        };
        match outcome {
            Ok(UpdateOutcome::Visible) => return Ok(false),
            Ok(UpdateOutcome::Unchanged(generation))
                if current.as_ref().is_some_and(|reader| {
                    reader.generation_path() == generation
                        && !should_compact(
                            &reader.meta,
                            self.watcher_config.merge_segment_threshold,
                        )
                }) =>
            {
                // A previous preview may now cancel out (e.g. create then
                // remove). Restore the actual durable snapshot as well.
                if let Some(cached) = self.indexes.read().unwrap().get(root_path)
                    && let Some(current) = current
                {
                    *cached.reader.lock().unwrap() = current;
                }
                return Ok(true);
            }
            Ok(_) => {}
            Err(error)
                if error
                    .downcast_ref::<crate::index::build::SourceReadError>()
                    .is_some() =>
            {
                return Err(error);
            }
            Err(error) => {
                eprintln!("fxid: reconcile failed: {error}; rebuilding");
                return self.rebuild_with_lock(root_path, lock).map(|_| true);
            }
        }
        let reconciled = Instant::now();
        let refreshed = IndexReader::open(root_path).and_then(|reader| {
            if should_compact(&reader.meta, self.watcher_config.merge_segment_threshold) {
                crate::index::compact::merge_segments(root_path)?;
                IndexReader::open(root_path)
            } else {
                Ok(reader)
            }
        });
        let reader = refreshed?;
        if let Some(cached) = self.indexes.read().unwrap().get(root_path) {
            cached.set_pending_reader(reader);
        }
        if trace {
            eprintln!(
                "fxid: update timings reconcile={:.3}ms reopen={:.3}ms total={:.3}ms",
                reconciled.duration_since(started).as_secs_f64() * 1000.0,
                reconciled.elapsed().as_secs_f64() * 1000.0,
                started.elapsed().as_secs_f64() * 1000.0,
            );
        }
        Ok(true)
    }

    /// A rebuild consumes only the batch captured before it starts. Failure
    /// restores that batch, including when acquiring the writer lock fails;
    /// notifications accumulated during the rebuild retain their own entry.
    #[cfg(test)]
    fn trigger_rebuild(&self, root_path: &PathBuf) {
        let captured = self.pending_changes.lock().unwrap().remove(root_path);
        let result = (|| {
            let lock = crate::utils::IndexLock::acquire(root_path)?;
            self.rebuild_with_lock(root_path, &lock)
        })();
        if let Err(error) = result {
            eprintln!("fxid: rebuild failed; retaining pending changes: {error:#}");
            if let Some(pending) = captured {
                self.restore_pending_changes(root_path, pending);
                self.retry_pending_changes(root_path);
            }
        }
    }

    /// Recovery from a failed delta already owns the mutation lock.
    fn rebuild_with_lock(
        &self,
        root_path: &PathBuf,
        _lock: &crate::utils::IndexLock,
    ) -> Result<()> {
        // Generations are built outside the source root and published
        // atomically. Keep the watcher registered so edits during a rebuild
        // remain queued and queries do not try to restart it under this lock.
        // Rebuild
        if let Err(e) = build_index_with_progress(root_path, true, true) {
            eprintln!("fxid: failed to rebuild index: {}", e);
            return Err(e);
        }

        // Reload and ensure a watcher exists
        match IndexReader::open(root_path) {
            Ok(reader) => {
                let doc_count = reader.meta.doc_count;
                {
                    let indexes = self.indexes.read().unwrap();
                    if let Some(cached) = indexes.get(root_path) {
                        cached.set_pending_reader(reader);
                    }
                }
                eprintln!("fxid: rebuilt index with {} files", doc_count);

                // The existing watcher stays registered; spawn is idempotent.
                if self.watch_enabled {
                    self.spawn_watcher(root_path);
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("fxid: failed to reload index after rebuild: {}", e);
                Err(e)
            }
        }
    }

    /// Spawn a file watcher for the given root path
    fn spawn_watcher(&self, root_path: &PathBuf) {
        let Some(cached) = self.indexes.read().unwrap().get(root_path).cloned() else {
            return;
        };
        let Ok(mut watcher_handle) = cached.watcher_handle.lock() else {
            return;
        };
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        if watcher_handle
            .as_ref()
            .is_some_and(WatcherHandle::is_running)
        {
            return;
        }
        let root = root_path.clone();
        let tx = self.watcher_tx.clone();
        let config = self.watcher_config.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let thread = thread::spawn(move || {
            if let Err(e) = run_watcher_thread(root.clone(), tx.clone(), config, shutdown_clone) {
                let _ = tx.send(WatcherMessage::Error {
                    root_path: root,
                    message: e.to_string(),
                });
            }
        });

        let handle = WatcherHandle::new(shutdown, thread, root_path.clone());

        *watcher_handle = Some(handle);
    }

    /// Stop all active watchers
    pub(crate) fn stop_all_watchers(&self) {
        let indexes = self.indexes.read().unwrap();
        for cached in indexes.values() {
            cached.stop_watcher();
        }
    }

    pub(crate) fn handle_request(&self, request: Request) -> Response {
        if self.shutdown.load(Ordering::Acquire)
            && !matches!(
                &request,
                Request::Shutdown | Request::Ping | Request::Status
            )
        {
            return Response::Error {
                message: "Daemon is shutting down".into(),
            };
        }
        match request {
            Request::Search {
                query,
                root_path,
                limit,
            } => self.handle_search(query, root_path, limit),

            Request::ContentSearch {
                pattern,
                root_path,
                limit,
                options,
            } => self.handle_content_search(pattern, root_path, limit, options),

            Request::Status => self.handle_status(),

            Request::Reload { root_path } => self.handle_reload(root_path),
            Request::Remove { root_path } => self.handle_remove(root_path),

            Request::Shutdown => {
                self.shutdown.store(true, Ordering::Release);
                let (result, _) = self
                    .shutdown_result
                    .1
                    .wait_timeout_while(
                        self.shutdown_result.0.lock().unwrap(),
                        Duration::from_secs(25),
                        |result| result.is_none(),
                    )
                    .unwrap();
                match result.as_ref() {
                    Some(Ok(())) => Response::ShuttingDown,
                    Some(Err(message)) => Response::Error {
                        message: message.clone(),
                    },
                    None => Response::Error {
                        message: "Shutdown persistence did not complete before the deadline".into(),
                    },
                }
            }

            Request::Ping => Response::Pong,

            Request::Hello {
                protocol_version: _,
            } => Response::Hello {
                protocol_version: PROTOCOL_VERSION,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
            },

            Request::WatchStatus { root_path } => self.handle_watch_status(root_path),
        }
    }

    /// Handle a search request
    fn handle_search(&self, query: String, root_path: Option<PathBuf>, limit: usize) -> Response {
        let start = Instant::now();

        let requested_path = root_path.as_ref().and_then(|path| path.canonicalize().ok());
        // Resolve index location independently of the requested search scope.
        let root_path = match self.resolve_root(root_path) {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        // Ensure index is loaded (and watcher started)
        if let Err(e) = self.ensure_index_loaded(&root_path) {
            return Response::Error {
                message: format!("Failed to load index: {}", e),
            };
        }

        // Access the index with read lock
        // Clone the index handle out of the map so the global lock is not
        // held for the duration of the query
        let cached = {
            let indexes = self.indexes.read().unwrap();
            match indexes.get(&root_path) {
                Some(c) => Arc::clone(c),
                None => {
                    return Response::Error {
                        message: "Index not found after loading".to_string(),
                    };
                }
            }
        };

        cached.touch();

        // Get the reader (handles pending swap)
        let reader = cached.get_reader();

        // Source verification is live. A generation-keyed result cache alone
        // cannot detect edits to candidates that produced no previous result.
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Parse and execute query
        let mut parsed = match try_parse_query(&query) {
            Ok(query) => query,
            Err(error) => {
                return Response::Error {
                    message: error.to_string(),
                };
            }
        };
        parsed.filters.search_scope = requested_path.and_then(|path| {
            path.strip_prefix(&root_path)
                .ok()
                .filter(|p| !p.as_os_str().is_empty())
                .map(PathBuf::from)
        });
        parsed.options.limit = if parsed.options.explicit_limit && parsed.options.limit != 0 {
            if limit == 0 {
                parsed.options.limit
            } else {
                parsed.options.limit.min(limit)
            }
        } else {
            limit
        };
        if parsed.is_empty() {
            return Response::Search(SearchResponse {
                matches: vec![],
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                cached: false,
                resolved_root: Some(root_path.clone()),
            });
        }

        let executor = QueryExecutor::new(&reader);
        let matches = match executor.execute(&parsed) {
            Ok(m) => m,
            Err(e) => {
                return Response::Error {
                    message: format!("Search failed: {}", e),
                };
            }
        };

        // The response owns its records; no result cache shares this vector.
        let mut match_data: Vec<SearchMatchData> = matches
            .into_iter()
            .map(|m| SearchMatchData {
                path: m.path,
                line_number: m.line_number,
                score: m.score,
            })
            .collect();
        if limit > 0 {
            match_data.truncate(limit);
        }
        self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

        Response::Search(SearchResponse {
            matches: match_data,
            duration_ms: start.elapsed().as_secs_f64() * 1000.0,
            cached: false,
            resolved_root: Some(root_path),
        })
    }

    /// Handle a content search request (ripgrep-like)
    fn handle_content_search(
        &self,
        pattern: String,
        root_path: Option<PathBuf>,
        limit: usize,
        options: ContentSearchOptions,
    ) -> Response {
        let start = Instant::now();

        let requested_path = root_path.as_ref().and_then(|path| path.canonicalize().ok());
        // Resolve index location independently of the requested search scope.
        let root_path = match self.resolve_root(root_path) {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        // Ensure index is loaded (and watcher started)
        if let Err(e) = self.ensure_index_loaded(&root_path) {
            return Response::Error {
                message: format!("Failed to load index: {}", e),
            };
        }

        // Clone the index handle out of the map so the global lock is not
        // held for the duration of the query
        let cached = {
            let indexes = self.indexes.read().unwrap();
            match indexes.get(&root_path) {
                Some(c) => Arc::clone(c),
                None => {
                    return Response::Error {
                        message: "Index not found after loading".to_string(),
                    };
                }
            }
        };

        cached.touch();

        // Get the reader (handles pending swap)
        let reader = cached.get_reader();

        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Parse and execute query. Case-insensitivity is applied at the plan
        // level: the planner derives sound Unicode-aware gram alternatives
        // and verifiers apply the requested case semantics.
        let mut parsed = match try_parse_query(&pattern) {
            Ok(query) => query,
            Err(error) => {
                return Response::Error {
                    message: error.to_string(),
                };
            }
        };
        parsed.filters.search_scope = requested_path.and_then(|path| {
            path.strip_prefix(&root_path)
                .ok()
                .filter(|p| !p.as_os_str().is_empty())
                .map(PathBuf::from)
        });
        if options.word_regexp
            && let Err(error) = parsed.apply_word_boundaries()
        {
            return Response::Error {
                message: error.to_string(),
            };
        }
        parsed.options.case_insensitive = options.case_insensitive;
        if parsed.is_empty() {
            return Response::ContentSearch(ContentSearchResponse {
                file_paths: None,
                file_counts: None,
                matches: vec![],
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: 0,
                resolved_root: Some(root_path.clone()),
            });
        }

        let executor = QueryExecutor::new(&reader);

        // Use optimized files-only path when requested
        if options.files_only {
            let effective_limit = if limit == 0 {
                MAX_RESULTS_CAP
            } else {
                limit.min(MAX_RESULTS_CAP)
            };
            let matching_files = match executor.execute_files_only(&parsed, effective_limit) {
                Ok(files) => files,
                Err(e) => {
                    return Response::Error {
                        message: format!("Search failed: {}", e),
                    };
                }
            };

            if options.compact_files {
                let file_count = matching_files.len();
                self.stats.queries_served.fetch_add(1, Ordering::Relaxed);
                return Response::ContentSearch(ContentSearchResponse {
                    matches: Vec::new(),
                    file_paths: Some(matching_files),
                    file_counts: None,
                    duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                    files_with_matches: file_count,
                    resolved_root: Some(root_path.clone()),
                });
            }

            // Convert to minimal ContentMatch (just path, no content)
            let file_count = matching_files.len();
            let match_data: Vec<ContentMatch> = matching_files
                .into_iter()
                .map(|path| ContentMatch {
                    path,
                    line_number: 1,
                    line_content: String::new(),
                    match_start: 0,
                    match_end: 0,
                    context_before: vec![],
                    context_after: vec![],
                })
                .collect();

            self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

            return Response::ContentSearch(ContentSearchResponse {
                file_paths: None,
                file_counts: None,
                matches: match_data,
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: file_count,
                resolved_root: Some(root_path.clone()),
            });
        }

        if options.counts_only {
            let counts = match executor.execute_match_counts(&parsed, limit) {
                Ok(counts) => counts,
                Err(e) => {
                    return Response::Error {
                        message: format!("Search failed: {e}"),
                    };
                }
            };
            self.stats.queries_served.fetch_add(1, Ordering::Relaxed);
            return Response::ContentSearch(ContentSearchResponse {
                files_with_matches: counts.len(),
                file_counts: Some(counts),
                file_paths: None,
                matches: Vec::new(),
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                resolved_root: Some(root_path),
            });
        }

        // Full content search path
        let matches = match executor.execute_with_content(
            &parsed,
            options.context_before,
            options.context_after,
        ) {
            Ok(m) => m,
            Err(e) => {
                return Response::Error {
                    message: format!("Search failed: {}", e),
                };
            }
        };

        // Count unique files (dedup by borrowed path, no clones)
        let unique_files: std::collections::HashSet<&std::path::Path> =
            matches.iter().map(|m| m.path.as_path()).collect();
        let file_count = unique_files.len();
        drop(unique_files);

        // Convert to protocol type and apply limit
        let effective_limit = if limit == 0 {
            MAX_RESULTS_CAP
        } else {
            limit.min(MAX_RESULTS_CAP)
        };
        let iter = matches.into_iter().take(effective_limit);
        let match_data: Vec<ContentMatch> = iter
            .map(|m| ContentMatch {
                path: m.path,
                line_number: m.line_number,
                line_content: m.line_content,
                match_start: m.match_start,
                match_end: m.match_end,
                context_before: m.context_before,
                context_after: m.context_after,
            })
            .collect();

        self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

        Response::ContentSearch(ContentSearchResponse {
            file_paths: None,
            file_counts: None,
            matches: match_data,
            duration_ms: start.elapsed().as_secs_f64() * 1000.0,
            files_with_matches: file_count,
            resolved_root: Some(root_path),
        })
    }

    /// Handle status request
    fn handle_status(&self) -> Response {
        let indexes = self.indexes.read().unwrap();

        let total_docs: u32 = indexes
            .values()
            .map(|idx| idx.get_reader().valid_doc_ids().len().min(u32::MAX as u64) as u32)
            .fold(0, u32::saturating_add);

        let loaded_roots: Vec<PathBuf> = indexes.keys().cloned().collect();

        // Estimate memory usage (rough)
        let memory_bytes: u64 = indexes
            .values()
            .map(|idx| {
                // Rough estimate: doc count * 100 bytes per doc + overhead
                (idx.get_reader().meta.doc_count as u64) * 100 + 1024 * 1024
            })
            .sum();

        Response::Status(StatusResponse {
            uptime_secs: self.stats.start_time.elapsed().as_secs(),
            indexes_loaded: indexes.len(),
            total_docs,
            queries_served: self.stats.queries_served.load(Ordering::Relaxed),
            cache_hit_rate: self.stats.cache_hit_rate(),
            memory_bytes,
            loaded_roots,
            protocol_version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            watch_enabled: self.watch_enabled,
            watched_roots: indexes
                .iter()
                .filter(|(_, index)| index.is_watching())
                .map(|(root, _)| root.clone())
                .collect(),
        })
    }

    /// Handle reload request
    /// Report whether a root is being watched and how many debounced
    /// changes await flushing. Does NOT load the index for unloaded roots:
    /// an unloaded root is by definition not watched.
    fn handle_watch_status(&self, root_path: Option<PathBuf>) -> Response {
        let root_path = match self.resolve_root(root_path) {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        let watching = {
            let indexes = self.indexes.read().unwrap();
            indexes.get(&root_path).is_some_and(|c| c.is_watching())
        };

        let pending_changes = if watching {
            let pending = self
                .pending_changes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending
                .get(&root_path)
                .map(|p| p.batch.created.len() + p.batch.modified.len() + p.batch.deleted.len())
                .unwrap_or(0)
        } else {
            0
        };

        Response::WatchStatus {
            watching,
            pending_changes,
            resolved_root: Some(root_path),
        }
    }

    fn lifecycle_for(&self, root: &Path) -> Arc<Mutex<()>> {
        self.lifecycle
            .lock()
            .unwrap()
            .entry(root.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn handle_remove(&self, requested: PathBuf) -> Response {
        let root = match self.resolve_root(Some(requested)) {
            Ok(root) => root,
            Err(error) => return error,
        };
        let lifecycle = self.lifecycle_for(&root);
        let _lifecycle = lifecycle.lock().unwrap();
        let result = (|| -> Result<()> {
            let _writer = crate::utils::IndexLock::acquire(&root)?;
            let removed = self.indexes.write().unwrap().remove(&root);
            if let Some(cached) = removed {
                cached.stop_watcher();
            }
            self.pending_changes.lock().unwrap().remove(&root);
            crate::utils::remove_index(&root)
        })();
        match result {
            Ok(()) => Response::Reloaded {
                success: true,
                message: "Removed index and unloaded daemon reader".into(),
                resolved_root: Some(root),
            },
            Err(error) => Response::Error {
                message: error.to_string(),
            },
        }
    }

    fn handle_reload(&self, root_path: Option<PathBuf>) -> Response {
        let root = match self.resolve_root(root_path) {
            Ok(root) => root,
            Err(error) => return error,
        };
        let lifecycle = self.lifecycle_for(&root);
        let guard = lifecycle.lock().unwrap();
        let cached = self.indexes.read().unwrap().get(&root).cloned();
        let result = if let Some(cached) = cached {
            (|| -> Result<()> {
                let writer = crate::utils::IndexLock::acquire(&root)?;
                let reader = IndexReader::open(&root)?;
                let previous_live = cached.get_reader();
                let pending = {
                    let mut pending = self.pending_changes.lock().unwrap();
                    if let Some(changes) = pending.get_mut(&root) {
                        changes.needs_visibility = true;
                        true
                    } else {
                        false
                    }
                };
                if pending {
                    // A reload refreshes the durable base, but must not hide
                    // an already searchable preview while dirty work remains.
                    if cached.is_watching() {
                        reader.prepare_watched_paths();
                    }
                    *cached.durable_reader.lock().unwrap() = Arc::new(reader);
                } else {
                    cached.set_pending_reader(reader);
                }
                drop(writer);
                if pending {
                    self.flush_pending_changes_mode(&root, false);
                    let still_dirty = self
                        .pending_changes
                        .lock()
                        .unwrap()
                        .get(&root)
                        .is_some_and(|changes| changes.needs_visibility);
                    // Another writer can own the removed pending batch. In
                    // that case absence from the map does not prove that the
                    // old live snapshot has been replaced yet.
                    anyhow::ensure!(
                        !still_dirty && !Arc::ptr_eq(&previous_live, &cached.get_reader()),
                        "Durable generation reloaded, but pending changes are not visible yet; resolve writer contention or source read errors and retry reload"
                    );
                }
                Ok(())
            })()
        } else {
            drop(guard);
            self.ensure_index_loaded(&root)
        };
        match result {
            Ok(()) => Response::Reloaded {
                success: true,
                message: "Reloaded current generation".into(),
                resolved_root: Some(root),
            },
            Err(error) => Response::Reloaded {
                success: false,
                message: format!("Failed to reload: {error}"),
                resolved_root: Some(root),
            },
        }
    }

    /// Resolve a root path from an optional client-provided path.
    /// - Some(path): canonicalize and walk up to find codebase root (.git / indexed parent)
    /// - None: if exactly one index is loaded, use it; otherwise error
    fn resolve_root(&self, root_path: Option<PathBuf>) -> Result<PathBuf, Response> {
        match root_path {
            Some(path) => {
                let canonical = path.canonicalize().map_err(|e| Response::Error {
                    message: format!("Invalid path: {}", e),
                })?;
                crate::utils::find_codebase_root(&canonical).map_err(|e| Response::Error {
                    message: format!("Could not resolve codebase root: {}", e),
                })
            }
            None => {
                let indexes = self.indexes.read().unwrap();
                match indexes.len() {
                    0 => Err(Response::Error {
                        message: "No indexes loaded; root_path is required".to_string(),
                    }),
                    1 => Ok(indexes.keys().next().unwrap().clone()),
                    n => Err(Response::Error {
                        message: format!(
                            "Ambiguous: {} indexes loaded; specify root_path. Loaded: {}",
                            n,
                            indexes
                                .keys()
                                .map(|k| k.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    }),
                }
            }
        }
    }

    /// Ensure an index is loaded and watcher is running (if enabled)
    fn ensure_index_loaded(&self, root_path: &PathBuf) -> Result<()> {
        // Check with read lock first
        let needs_load_or_watch = {
            let indexes = self.indexes.read().unwrap();
            if let Some(cached) = indexes.get(root_path) {
                // Index loaded, check if watcher needs to be started
                self.watch_enabled && !cached.is_watching()
            } else {
                // Index not loaded
                true
            }
        };

        // If index exists and watcher not needed (or already running), return
        if !needs_load_or_watch {
            return Ok(());
        }

        let lifecycle = self.lifecycle_for(root_path);
        let _lifecycle = lifecycle.lock().unwrap();
        anyhow::ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "Daemon is shutting down"
        );
        // Check again after serializing load/start/remove for this root.
        let index_loaded = {
            let indexes = self.indexes.read().unwrap();
            indexes.contains_key(root_path)
        };

        if !index_loaded {
            // Open the index BEFORE taking the write lock: a cold load takes
            // up to seconds, and holding the global write lock for it stalls
            // every search on already-loaded codebases. If two threads race
            // to load the same index, the loser's reader is simply dropped.
            eprintln!("fxid: loading index for {}", root_path.display());
            let reader = IndexReader::open(root_path)?;
            let doc_count = reader.meta.doc_count;

            let mut indexes = self.indexes.write().unwrap();
            if !indexes.contains_key(root_path) {
                indexes.insert(root_path.clone(), Arc::new(CachedIndex::new(reader)));
                eprintln!(
                    "fxid: loaded {} files from {}",
                    doc_count,
                    root_path.display()
                );
            }
        }

        // Start watcher if enabled and not already running
        if self.watch_enabled {
            let should_start_watcher = {
                let indexes = self.indexes.read().unwrap();
                indexes.get(root_path).is_some_and(|c| !c.is_watching())
            };

            if should_start_watcher {
                // Reconcile before watching: the watcher only sees events
                // from now on, so changes made while no watcher was running
                // must be picked up by one incremental scan or the index
                // would stay stale until a manual `fxi index`
                eprintln!("fxid: reconciling index for {}", root_path.display());
                let _lock = crate::utils::IndexLock::acquire(root_path)?;
                let current = self
                    .indexes
                    .read()
                    .unwrap()
                    .get(root_path)
                    .map(|cached| cached.get_durable_reader());
                match reconcile_index(
                    root_path,
                    current.as_deref(),
                    self.watcher_config.rebuild_threshold_percent,
                ) {
                    Ok(UpdateOutcome::Unchanged(generation))
                        if current
                            .as_ref()
                            .is_some_and(|reader| reader.generation_path() == generation) => {}
                    Ok(_) => {
                        // Swap in a fresh reader in case the scan changed it
                        {
                            let reader = IndexReader::open(root_path)?;
                            let indexes = self.indexes.read().unwrap();
                            if let Some(cached) = indexes.get(root_path) {
                                cached.set_pending_reader(reader);
                            }
                        }
                    }
                    Err(e) => {
                        return Err(e.context(format!(
                            "Could not reconcile {} before starting its watcher",
                            root_path.display()
                        )));
                    }
                }

                if let Some(cached) = self.indexes.read().unwrap().get(root_path) {
                    cached.get_durable_reader().prepare_watched_paths();
                }
                eprintln!("fxid: starting file watcher for {}", root_path.display());
                self.spawn_watcher(root_path);
            }
        }

        Ok(())
    }
}

/// Check if compaction should be triggered based on fragmentation metrics.
fn should_compact(meta: &IndexMeta, segment_threshold: usize) -> bool {
    // Check tombstone ratio
    if meta.doc_count > 0 {
        let ratio = meta.tombstone_count as f32 / meta.doc_count as f32;
        if ratio > COMPACTION_TOMBSTONE_THRESHOLD {
            return true;
        }
    }
    // Check segment count - only count NEW deltas added since creation/merge
    // This prevents chunked initial indexes from immediately triggering merge
    let new_deltas = meta
        .delta_segments
        .len()
        .saturating_sub(meta.delta_baseline);
    new_deltas >= segment_threshold
}

/// Preserve native paths as reconciliation hints. Every path is "modified"
/// deliberately: a create/delete pair can describe replacement of an already
/// indexed file, so cancelling it before inspecting the filesystem loses work.
/// Incomplete notifications always request the authoritative full scan.
fn accumulate_native_event(
    root: &std::path::Path,
    debouncer: &mut EventDebouncer,
    event: Result<Event, notify::Error>,
) {
    let Ok(event) = event else {
        debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
        return;
    };
    if event.need_rescan() {
        debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
        return;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    if event.paths.is_empty() || matches!(event.kind, EventKind::Any | EventKind::Other) {
        debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
        return;
    }
    for path in event.paths {
        match path.strip_prefix(root) {
            Ok(relative) => debouncer.add_event(relative.to_path_buf(), ChangeKind::Modified),
            Err(_) => debouncer.add_event(PathBuf::new(), ChangeKind::Modified),
        }
    }
}

/// Run the file watcher thread
fn run_watcher_thread(
    root_path: PathBuf,
    tx: Sender<WatcherMessage>,
    config: WatcherConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut debouncer = EventDebouncer::new(config.clone());
    let (event_tx, event_rx) = mpsc::channel();

    // Create the watcher
    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            // Errors/overflow also require reconciliation.
            let _ = event_tx.send(res);
        },
        notify::Config::default(),
    )?;

    // Start watching
    watcher.watch(&root_path, RecursiveMode::Recursive)?;

    // The initial scan predates watch registration. Reconcile once more now
    // that events are buffered, closing the startup notification gap.
    debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
    let mut last_reconcile = Instant::now();

    eprintln!("fxid: watching {} for changes", root_path.display());

    // Event processing loop
    loop {
        if shutdown.load(Ordering::Relaxed) {
            drop(watcher);
            for event in event_rx.try_iter() {
                accumulate_native_event(&root_path, &mut debouncer, event);
            }
            // Native delivery may still have been buffered in the OS at stop.
            // A final reconciliation closes that gap before acknowledging exit.
            debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
            if let Some(batch) = debouncer.flush() {
                let _ = tx.send(WatcherMessage::ChangesReady { root_path, batch });
            }
            return Ok(());
        }

        let timeout = debouncer
            .time_until_ready()
            .unwrap_or(Duration::from_millis(100))
            .min(Duration::from_millis(100));
        match event_rx.recv_timeout(timeout) {
            Ok(event) => {
                accumulate_native_event(&root_path, &mut debouncer, event);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Check if debounce window has elapsed
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }

        if last_reconcile.elapsed() >= Duration::from_secs(300) {
            debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
            last_reconcile = Instant::now();
        }

        // Check if we should flush the debouncer
        if debouncer.has_pending()
            && debouncer.is_ready()
            && let Some(batch) = debouncer.flush()
        {
            let _ = tx.send(WatcherMessage::ChangesReady {
                root_path: root_path.clone(),
                batch,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_reconciliation(server: &IndexServer, root: &std::path::Path) {
        let mut batch = ChangeBatch::new();
        batch.add(crate::server::watcher::FileChange {
            path: PathBuf::new(),
            kind: ChangeKind::Modified,
        });
        server.accumulate_changes(root.to_path_buf(), batch);
    }

    fn indexed_paths(server: &IndexServer, root: &PathBuf) -> Vec<PathBuf> {
        let reader = server
            .indexes
            .read()
            .unwrap()
            .get(root)
            .unwrap()
            .get_reader();
        let mut paths: Vec<_> = reader
            .valid_doc_ids()
            .iter()
            .map(|id| {
                reader
                    .get_path(reader.get_document(id).unwrap())
                    .unwrap()
                    .clone()
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn watcher_backlog_is_bounded_and_preserves_every_message() {
        let server = IndexServer::new(false);
        for i in 0..1300 {
            server
                .watcher_tx
                .send(WatcherMessage::ChangesReady {
                    root_path: PathBuf::from(i.to_string()),
                    batch: ChangeBatch::new(),
                })
                .unwrap();
        }
        let mut messages = server.receive_watcher_messages();
        assert_eq!(messages.len(), 1024);
        messages.extend(server.receive_watcher_messages());
        let roots: Vec<_> = messages
            .into_iter()
            .map(|message| {
                let WatcherMessage::ChangesReady { root_path, .. } = message else {
                    panic!("wrong message")
                };
                root_path
            })
            .collect();
        assert_eq!(
            roots,
            (0..1300)
                .map(|i| PathBuf::from(i.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rebuild_and_duplicate_registration_keep_the_existing_watcher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("file.rs"), "content\n").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        let mut server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        Arc::get_mut(&mut server).unwrap().watch_enabled = true;
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let thread = std::thread::spawn(move || {
            while !worker_stopped.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let cached = server.indexes.read().unwrap().get(&root).unwrap().clone();
        *cached.watcher_handle.lock().unwrap() =
            Some(WatcherHandle::new(stopped.clone(), thread, root.clone()));
        server.spawn_watcher(&root);
        assert!(!stopped.load(Ordering::SeqCst));
        {
            let lock = crate::utils::IndexLock::acquire(&root).unwrap();
            server.rebuild_with_lock(&root, &lock).unwrap();
        }
        assert!(
            !stopped.load(Ordering::SeqCst),
            "rebuild interrupted the watcher"
        );
        assert!(cached.is_watching());
        server.stop_all_watchers();
        assert!(stopped.load(Ordering::SeqCst));
        drop((cached, server));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn unchanged_reconciliation_still_performs_due_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..20 {
            std::fs::write(root.join(format!("{i}.rs")), "old content\n").unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        std::fs::write(root.join("0.rs"), "different fresh content\n").unwrap();
        {
            let _lock = crate::utils::IndexLock::acquire(&root).unwrap();
            crate::index::build::update_index(&root).unwrap();
        }
        let mut server = IndexServer::new(false);
        Arc::get_mut(&mut server)
            .unwrap()
            .watcher_config
            .merge_segment_threshold = 1;
        server.ensure_index_loaded(&root).unwrap();
        let original = server
            .indexes
            .read()
            .unwrap()
            .get(&root)
            .unwrap()
            .get_reader();
        assert!(should_compact(&original.meta, 1));
        queue_reconciliation(&server, &root);
        server.flush_expired_changes(Duration::ZERO);
        let compacted = server
            .indexes
            .read()
            .unwrap()
            .get(&root)
            .unwrap()
            .get_reader();
        assert_ne!(original.generation_path(), compacted.generation_path());
        assert_eq!(compacted.meta.tombstone_count, 0);
        assert_eq!(compacted.valid_doc_ids().len(), 20);
        drop((original, compacted, server));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn unchanged_reconciliation_reuses_reader_but_external_publication_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("old.rs"), "old content\n").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        let original = server
            .indexes
            .read()
            .unwrap()
            .get(&root)
            .unwrap()
            .get_reader();
        queue_reconciliation(&server, &root);
        server.flush_expired_changes(Duration::ZERO);
        let unchanged = server
            .indexes
            .read()
            .unwrap()
            .get(&root)
            .unwrap()
            .get_reader();
        assert!(Arc::ptr_eq(&original, &unchanged));
        std::fs::write(root.join("new.rs"), "new content\n").unwrap();
        {
            let _lock = crate::utils::IndexLock::acquire(&root).unwrap();
            build_index_with_progress(&root, true, true).unwrap();
        }
        queue_reconciliation(&server, &root);
        server.flush_expired_changes(Duration::ZERO);
        let refreshed = server
            .indexes
            .read()
            .unwrap()
            .get(&root)
            .unwrap()
            .get_reader();
        assert!(!Arc::ptr_eq(&original, &refreshed));
        assert_ne!(original.generation_path(), refreshed.generation_path());
        assert_eq!(
            indexed_paths(&server, &root),
            vec![PathBuf::from("new.rs"), PathBuf::from("old.rs")]
        );
        drop((original, unchanged, refreshed, server));
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn busy_writer_retains_work_without_blocking_another_root() {
        let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
        let roots = dirs
            .each_ref()
            .map(|dir| dir.path().canonicalize().unwrap());
        let server = IndexServer::new(false);
        for root in &roots {
            std::fs::write(root.join("old.rs"), "old content\n").unwrap();
            build_index_with_progress(root, true, true).unwrap();
            server.ensure_index_loaded(root).unwrap();
            std::fs::write(root.join("new.rs"), "new content\n").unwrap();
            queue_reconciliation(&server, root);
        }
        let held = crate::utils::IndexLock::acquire(&roots[0]).unwrap();
        server.flush_expired_changes(Duration::ZERO);
        assert!(
            server
                .pending_changes
                .lock()
                .unwrap()
                .contains_key(&roots[0])
        );
        assert_eq!(
            indexed_paths(&server, &roots[0]),
            vec![PathBuf::from("old.rs")]
        );
        assert_eq!(
            indexed_paths(&server, &roots[1]),
            vec![PathBuf::from("new.rs"), PathBuf::from("old.rs")]
        );
        drop(held);
        server.flush_expired_changes(Duration::ZERO);
        assert!(server.pending_changes.lock().unwrap().is_empty());
        assert_eq!(
            indexed_paths(&server, &roots[0]),
            indexed_paths(&server, &roots[1])
        );
        drop(server);
        for root in &roots {
            crate::utils::remove_index(root).unwrap();
        }
    }

    #[test]
    fn rebuild_lock_failure_retains_the_captured_pending_batch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("old.rs"), "old content\n").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        std::fs::write(root.join("new.rs"), "new content\n").unwrap();
        queue_reconciliation(&server, &root);
        let first_change = server.pending_changes.lock().unwrap()[&root].first_change;
        let lock_path = crate::utils::app_data::get_index_container(&root)
            .unwrap()
            .with_extension("lock");
        // A directory at the lock filename produces a genuine acquisition
        // error rather than ordinary writer contention.
        if lock_path.exists() {
            std::fs::remove_file(&lock_path).unwrap();
        }
        std::fs::create_dir(&lock_path).unwrap();
        server.trigger_rebuild(&root);
        {
            let pending = server.pending_changes.lock().unwrap();
            let retained = &pending[&root];
            assert_eq!(retained.first_change, first_change);
            assert!(retained.retry_after.is_some());
            assert!(retained.batch.modified.contains(&PathBuf::new()));
        }
        std::fs::remove_dir(lock_path).unwrap();
        server
            .pending_changes
            .lock()
            .unwrap()
            .get_mut(&root)
            .unwrap()
            .retry_after = None;
        server.flush_expired_changes(Duration::ZERO);
        assert!(server.pending_changes.lock().unwrap().is_empty());
        assert_eq!(
            indexed_paths(&server, &root),
            [PathBuf::from("new.rs"), PathBuf::from("old.rs")]
        );
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn rebuild_completion_preserves_newer_notifications_and_restores_failed_work() {
        for fail_publication in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            std::fs::write(root.join("old.rs"), "old content\n").unwrap();
            build_index_with_progress(&root, true, true).unwrap();
            let server = IndexServer::new(false);
            server.ensure_index_loaded(&root).unwrap();
            let queue = |path: &str| {
                let mut batch = ChangeBatch::new();
                batch.add(crate::server::watcher::FileChange {
                    path: path.into(),
                    kind: ChangeKind::Modified,
                });
                server.accumulate_changes(root.clone(), batch);
            };
            std::fs::write(root.join("first.rs"), "first new content\n").unwrap();
            queue("first.rs");
            let held = crate::utils::IndexLock::acquire(&root).unwrap();
            let temporary_manifest = crate::utils::app_data::get_index_container(&root)
                .unwrap()
                .join("CURRENT.tmp");
            if fail_publication {
                std::fs::create_dir(&temporary_manifest).unwrap();
            }
            let worker_server = Arc::clone(&server);
            let worker_root = root.clone();
            let worker = std::thread::spawn(move || worker_server.trigger_rebuild(&worker_root));
            // The held writer lock deterministically pauses the worker after
            // it captures the old batch, allowing another notification to arrive.
            let deadline = Instant::now() + Duration::from_secs(5);
            while server.pending_changes.lock().unwrap().contains_key(&root) {
                assert!(
                    Instant::now() < deadline,
                    "rebuild did not capture its pending batch"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            std::fs::write(root.join("second.rs"), "second new content\n").unwrap();
            queue("second.rs");
            drop(held);
            worker.join().unwrap();
            {
                let pending = server.pending_changes.lock().unwrap();
                let retained = &pending[&root];
                assert!(
                    retained
                        .batch
                        .modified
                        .contains(&PathBuf::from("second.rs"))
                );
                assert_eq!(
                    retained.batch.modified.contains(&PathBuf::from("first.rs")),
                    fail_publication
                );
                assert_eq!(retained.retry_after.is_some(), fail_publication);
            }
            if fail_publication {
                std::fs::remove_dir(temporary_manifest).unwrap();
            }
            server
                .pending_changes
                .lock()
                .unwrap()
                .get_mut(&root)
                .unwrap()
                .retry_after = None;
            server.flush_expired_changes(Duration::ZERO);
            assert!(server.pending_changes.lock().unwrap().is_empty());
            assert_eq!(
                indexed_paths(&server, &root),
                [
                    PathBuf::from("first.rs"),
                    PathBuf::from("old.rs"),
                    PathBuf::from("second.rs")
                ]
            );
            drop(server);
            crate::utils::remove_index(&root).unwrap();
        }
    }

    #[test]
    fn failed_publication_is_retried_without_another_notification() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("old.rs"), "old content\n").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        std::fs::write(root.join("new.rs"), "new content\n").unwrap();
        queue_reconciliation(&server, &root);
        let moved = dir.path().join("temporarily-unavailable");
        std::fs::rename(&root, &moved).unwrap();
        server.flush_expired_changes(Duration::ZERO);
        assert!(
            server
                .pending_changes
                .lock()
                .unwrap()
                .get(&root)
                .unwrap()
                .retry_after
                .is_some()
        );
        std::fs::rename(&moved, &root).unwrap();
        // Backoff prevents a persistent failure from creating a tight loop.
        server.flush_expired_changes(Duration::ZERO);
        assert_eq!(indexed_paths(&server, &root), vec![PathBuf::from("old.rs")]);
        server
            .pending_changes
            .lock()
            .unwrap()
            .get_mut(&root)
            .unwrap()
            .retry_after = None;
        server.flush_expired_changes(Duration::ZERO);
        assert!(server.pending_changes.lock().unwrap().is_empty());
        assert_eq!(
            indexed_paths(&server, &root),
            vec![PathBuf::from("new.rs"), PathBuf::from("old.rs")]
        );
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn visible_updates_persist_on_quiet_max_age_and_explicit_deadlines() {
        for mode in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            for i in 0..16 {
                std::fs::write(root.join(format!("{i}.rs")), "original content\n").unwrap();
            }
            build_index_with_progress(&root, true, true).unwrap();
            let mut server = IndexServer::new(false);
            server.ensure_index_loaded(&root).unwrap();
            Arc::get_mut(&mut server).unwrap().watch_enabled = true;
            let original = get_index_dir(&root).unwrap();
            std::fs::write(root.join("0.rs"), "deadlineMarker\n").unwrap();
            let mut batch = ChangeBatch::new();
            batch.add(crate::server::watcher::FileChange {
                path: "0.rs".into(),
                kind: ChangeKind::Modified,
            });
            server.accumulate_changes(root.clone(), batch);
            server.flush_pending_changes_mode(&root, false);
            assert_eq!(get_index_dir(&root).unwrap(), original);
            let cached = server.indexes.read().unwrap().get(&root).unwrap().clone();
            assert_eq!(
                QueryExecutor::new(&cached.get_reader())
                    .execute_files_only(&parse_query("deadlineMarker"), 0)
                    .unwrap(),
                [PathBuf::from("0.rs")]
            );
            let interval = if mode == 2 {
                Duration::from_secs(60)
            } else {
                Duration::ZERO
            };
            {
                let mut pending = server.pending_changes.lock().unwrap();
                let pending = pending.get_mut(&root).unwrap();
                assert!(!pending.needs_visibility);
                pending.first_change = Instant::now();
                pending.last_change = Instant::now();
            }
            server.flush_expired_changes(interval);
            assert_eq!(get_index_dir(&root).unwrap(), original);
            {
                let mut pending = server.pending_changes.lock().unwrap();
                let pending = pending.get_mut(&root).unwrap();
                let now = Instant::now();
                pending.first_change = now - Duration::from_secs(if mode == 2 { 61 } else { 11 });
                pending.last_change = if mode == 0 {
                    now - Duration::from_secs(1)
                } else {
                    now
                };
                if mode == 0 {
                    pending.first_change = now - Duration::from_secs(2);
                }
            }
            server.flush_expired_changes(interval);
            assert!(server.pending_changes.lock().unwrap().is_empty());
            assert_ne!(get_index_dir(&root).unwrap(), original);
            let disk = IndexReader::open(&root).unwrap();
            assert_eq!(
                QueryExecutor::new(&disk)
                    .execute_files_only(&parse_query("deadlineMarker"), 0)
                    .unwrap(),
                [PathBuf::from("0.rs")]
            );
            drop(cached);
            drop(disk);
            drop(server);
            crate::utils::remove_index(&root).unwrap();
        }
    }

    #[test]
    fn staged_updates_keep_a_durable_base_and_publish_the_whole_pending_union() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..32 {
            std::fs::write(root.join(format!("{i}.rs")), "original marker\n").unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        let original_generation = get_index_dir(&root).unwrap();
        let cached = server.indexes.read().unwrap().get(&root).unwrap().clone();
        let durable = cached.get_durable_reader();
        let queue = |names: &[&str]| {
            let mut batch = ChangeBatch::new();
            for name in names {
                batch.add(crate::server::watcher::FileChange {
                    path: PathBuf::from(name),
                    kind: ChangeKind::Modified,
                });
            }
            server.accumulate_changes(root.clone(), batch);
            server.flush_pending_changes_mode(&root, false);
        };
        let matching = |needle: &str| {
            QueryExecutor::new(&cached.get_reader())
                .execute_files_only(&parse_query(needle), 0)
                .unwrap()
        };
        std::fs::write(root.join("0.rs"), "previewFirstMarker\n").unwrap();
        std::fs::write(root.join("new.rs"), "previewFirstMarker\n").unwrap();
        queue(&["0.rs", "new.rs"]);
        assert_eq!(
            matching("previewFirstMarker"),
            [PathBuf::from("0.rs"), PathBuf::from("new.rs")]
        );
        assert_eq!(get_index_dir(&root).unwrap(), original_generation);
        assert!(Arc::ptr_eq(&durable, &cached.get_durable_reader()));
        assert!(!server.pending_changes.lock().unwrap().is_empty());

        std::fs::write(root.join("0.rs"), "previewLatestMarker\n").unwrap();
        std::fs::remove_file(root.join("new.rs")).unwrap();
        std::fs::write(root.join("another.rs"), "previewLatestMarker\n").unwrap();
        queue(&["0.rs", "new.rs", "another.rs"]);
        assert_eq!(
            matching("previewLatestMarker"),
            [PathBuf::from("0.rs"), PathBuf::from("another.rs")]
        );
        assert!(matching("previewFirstMarker").is_empty());
        assert_eq!(get_index_dir(&root).unwrap(), original_generation);

        server.flush_all_pending_changes();
        assert!(server.pending_changes.lock().unwrap().is_empty());
        assert_ne!(get_index_dir(&root).unwrap(), original_generation);
        assert!(Arc::ptr_eq(
            &cached.get_reader(),
            &cached.get_durable_reader()
        ));
        assert_eq!(
            matching("previewLatestMarker"),
            [PathBuf::from("0.rs"), PathBuf::from("another.rs")]
        );
        // A create followed by removal can cancel the entire uncommitted
        // update. Restore the durable reader instead of retaining ghost IDs.
        std::fs::write(root.join("cancel.rs"), "cancelledPreviewMarker\n").unwrap();
        queue(&["cancel.rs"]);
        assert_eq!(
            matching("cancelledPreviewMarker"),
            [PathBuf::from("cancel.rs")]
        );
        std::fs::remove_file(root.join("cancel.rs")).unwrap();
        queue(&["cancel.rs"]);
        assert!(matching("cancelledPreviewMarker").is_empty());
        assert!(server.pending_changes.lock().unwrap().is_empty());
        assert!(Arc::ptr_eq(
            &cached.get_reader(),
            &cached.get_durable_reader()
        ));
        std::fs::write(root.join("hidden.rs"), "ignorePreviewMarker\n").unwrap();
        queue(&["hidden.rs"]);
        assert_eq!(
            matching("ignorePreviewMarker"),
            [PathBuf::from("hidden.rs")]
        );
        std::fs::write(root.join(".ignore"), "hidden.rs\n").unwrap();
        queue(&[".ignore"]);
        assert!(matching("ignorePreviewMarker").is_empty());
        assert!(server.pending_changes.lock().unwrap().is_empty());
        std::fs::remove_file(root.join(".ignore")).unwrap();
        queue(&[".ignore"]);
        assert_eq!(
            matching("ignorePreviewMarker"),
            [PathBuf::from("hidden.rs")]
        );
        server.flush_all_pending_changes();
        drop(durable);
        drop(cached);
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn failed_persistence_does_not_turn_a_preview_into_the_durable_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..8 {
            std::fs::write(root.join(format!("{i}.rs")), "old content\n").unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        let cached = server.indexes.read().unwrap().get(&root).unwrap().clone();
        let durable = cached.get_durable_reader();
        let generation = get_index_dir(&root).unwrap();
        let meta = std::fs::read(generation.join("meta.json")).unwrap();
        std::fs::write(root.join("0.rs"), "uncommittedFreshMarker\n").unwrap();
        let lock = crate::utils::IndexLock::acquire(&root).unwrap();
        let result = reconcile_index_paths_with_visibility(
            &root,
            Some(&durable),
            30,
            &[PathBuf::from("0.rs")],
            &mut |reader| {
                cached.set_live_reader(reader);
                // Fail writer initialization after visibility, before publish.
                std::fs::write(generation.join("meta.json"), b"broken").unwrap();
            },
            false,
        );
        assert!(result.is_err());
        assert!(Arc::ptr_eq(&durable, &cached.get_durable_reader()));
        assert_eq!(
            QueryExecutor::new(&cached.get_reader())
                .execute_files_only(&parse_query("uncommittedFreshMarker"), 0)
                .unwrap(),
            [PathBuf::from("0.rs")]
        );
        std::fs::write(generation.join("meta.json"), meta).unwrap();
        drop(lock);
        let mut batch = ChangeBatch::new();
        batch.add(crate::server::watcher::FileChange {
            path: PathBuf::from("0.rs"),
            kind: ChangeKind::Modified,
        });
        server.accumulate_changes(root.clone(), batch);
        server.flush_all_pending_changes();
        assert_ne!(get_index_dir(&root).unwrap(), generation);
        assert!(Arc::ptr_eq(
            &cached.get_reader(),
            &cached.get_durable_reader()
        ));
        assert_eq!(
            QueryExecutor::new(&IndexReader::open(&root).unwrap())
                .execute_files_only(&parse_query("uncommittedFreshMarker"), 0)
                .unwrap(),
            [PathBuf::from("0.rs")]
        );
        drop(durable);
        drop(cached);
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn default_watcher_publishes_ready_batches_without_an_extra_delay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for i in 0..8 {
            std::fs::write(root.join(format!("{i}.rs")), "old content\n").unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        let mut server = IndexServer::new(false);
        Arc::get_mut(&mut server).unwrap().watcher_config = WatcherConfig::default();
        server.ensure_index_loaded(&root).unwrap();
        std::fs::write(root.join("0.rs"), "freshneedle\n").unwrap();
        std::fs::write(root.join("new.rs"), "freshneedle\n").unwrap();
        let mut batch = ChangeBatch::new();
        // Native notifications are reconciliation hints, including directory
        // and ignore-rule changes; an empty path intentionally means rescan.
        batch.add(crate::server::watcher::FileChange {
            path: PathBuf::new(),
            kind: ChangeKind::Modified,
        });
        server.accumulate_changes(root.clone(), batch);
        server.flush_expired_changes(server.watcher_config.delta_flush_duration());
        let response = server.handle_content_search(
            "freshneedle".into(),
            Some(root.clone()),
            0,
            ContentSearchOptions {
                files_only: true,
                compact_files: true,
                ..Default::default()
            },
        );
        let Response::ContentSearch(response) = response else {
            panic!("search failed")
        };
        assert_eq!(
            response.file_paths.unwrap(),
            vec![PathBuf::from("0.rs"), PathBuf::from("new.rs")]
        );
        assert!(!server.pending_changes.lock().unwrap().contains_key(&root));
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn compact_files_preserve_legacy_results_limits_and_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["with space.txt", "K.txt"] {
            std::fs::write(root.join(name), "needle\n").unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        let search = |compact, limit| {
            let response = server.handle_content_search(
                "needle".into(),
                Some(root.clone()),
                limit,
                ContentSearchOptions {
                    files_only: true,
                    compact_files: compact,
                    ..Default::default()
                },
            );
            let Response::ContentSearch(response) = response else {
                panic!("search failed")
            };
            assert_eq!(response.resolved_root.as_ref(), Some(&root));
            if compact {
                assert!(response.matches.is_empty());
                let paths = response.file_paths.unwrap();
                assert_eq!(response.files_with_matches, paths.len());
                paths
            } else {
                assert!(response.file_paths.is_none());
                response
                    .matches
                    .into_iter()
                    .map(|m| m.path)
                    .collect::<Vec<_>>()
            }
        };
        for limit in [0, 1] {
            assert_eq!(search(true, limit), search(false, limit));
        }
        std::fs::write(root.join("K.txt"), "absent\n").unwrap();
        assert_eq!(search(true, 0), vec![PathBuf::from("with space.txt")]);
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn count_responses_preserve_legacy_counts_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(root.join(name), "needle needle\nneedle\n").unwrap();
        }
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        for limit in [0, 1, 3] {
            let search = |counts_only| {
                let response = server.handle_content_search(
                    "re:/needle/".into(),
                    Some(root.clone()),
                    limit,
                    ContentSearchOptions {
                        counts_only,
                        ..Default::default()
                    },
                );
                let Response::ContentSearch(response) = response else {
                    panic!("search failed")
                };
                response
            };
            let legacy = search(false);
            assert!(legacy.file_counts.is_none());
            let mut expected = std::collections::BTreeMap::new();
            for m in legacy.matches {
                *expected.entry(m.path).or_insert(0usize) += 1;
            }
            let compact = search(true);
            assert!(compact.matches.is_empty());
            assert_eq!(compact.files_with_matches, expected.len());
            assert_eq!(
                compact.file_counts.unwrap(),
                expected.into_iter().collect::<Vec<_>>()
            );
        }
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn native_notifications_preserve_paths_and_replacements() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
        let root = std::path::Path::new("/root");
        let mut debouncer = EventDebouncer::new(WatcherConfig::default());
        // A create/delete sequence may replace an existing indexed path.
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
        ] {
            accumulate_native_event(
                root,
                &mut debouncer,
                Ok(Event::new(kind).add_path(root.join("a.rs"))),
            );
        }
        accumulate_native_event(
            root,
            &mut debouncer,
            Ok(
                Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(root.join("old.rs"))
                    .add_path(root.join("new.rs")),
            ),
        );
        let mut paths = debouncer.flush().unwrap().modified;
        paths.sort();
        assert_eq!(paths, ["a.rs", "new.rs", "old.rs"].map(PathBuf::from));
    }

    #[test]
    fn incomplete_native_notifications_always_request_full_reconciliation() {
        use notify::event::{AccessKind, Flag, ModifyKind};
        let root = std::path::Path::new("/root");
        let events = [
            Err(notify::Error::generic("lost notification")),
            Ok(Event::new(EventKind::Any).add_path(root.join("a.rs"))),
            Ok(Event::new(EventKind::Modify(ModifyKind::Any))),
            Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                .add_path(PathBuf::from("/outside/a.rs"))),
            // A rescan flag takes precedence even over an access event.
            Ok(Event::new(EventKind::Access(AccessKind::Any)).set_flag(Flag::Rescan)),
        ];
        for event in events {
            let mut debouncer = EventDebouncer::new(WatcherConfig::default());
            accumulate_native_event(root, &mut debouncer, event);
            assert_eq!(debouncer.flush().unwrap().modified, [PathBuf::new()]);
        }
        let mut debouncer = EventDebouncer::new(WatcherConfig::default());
        accumulate_native_event(
            root,
            &mut debouncer,
            Ok(Event::new(EventKind::Access(AccessKind::Read)).add_path(root.join("a.rs"))),
        );
        assert!(!debouncer.has_pending());
    }

    #[test]
    fn native_watcher_reports_deleted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = root.join("deleted.txt");
        std::fs::write(&file, "text").unwrap();
        let (tx, rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker = std::thread::spawn(move || {
            run_watcher_thread(
                root,
                tx,
                WatcherConfig {
                    debounce_ms: 20,
                    ..Default::default()
                },
                worker_shutdown,
            )
            .unwrap()
        });
        // The startup reconciliation is emitted only after registration.
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        std::fs::remove_file(file).unwrap();
        let received = rx.recv_timeout(Duration::from_secs(5));
        shutdown.store(true, Ordering::SeqCst);
        worker.join().unwrap();
        assert!(matches!(received, Ok(WatcherMessage::ChangesReady { .. })));
    }

    #[test]
    fn watcher_reconciliation_handles_subtrees_and_nested_ignore_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("old")).unwrap();
        std::fs::write(root.join("old/a.txt"), "needle").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        server.ensure_index_loaded(&root).unwrap();
        std::fs::rename(root.join("old"), root.join("new")).unwrap();
        server
            .apply_incremental_update(&root, &crate::utils::IndexLock::acquire(&root).unwrap())
            .unwrap();
        let paths = || {
            let cached = server.indexes.read().unwrap().get(&root).unwrap().clone();
            let reader = cached.get_reader();
            reader
                .valid_doc_ids()
                .iter()
                .map(|id| {
                    reader
                        .get_path(reader.get_document(id).unwrap())
                        .unwrap()
                        .clone()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(), vec![PathBuf::from("new/a.txt")]);
        std::fs::write(root.join("new/.gitignore"), "*.txt\n").unwrap();
        server
            .apply_incremental_update(&root, &crate::utils::IndexLock::acquire(&root).unwrap())
            .unwrap();
        assert!(paths().is_empty());
        std::fs::remove_file(root.join("new/.gitignore")).unwrap();
        server
            .apply_incremental_update(&root, &crate::utils::IndexLock::acquire(&root).unwrap())
            .unwrap();
        assert_eq!(paths(), vec![PathBuf::from("new/a.txt")]);
        std::fs::remove_dir_all(root.join("new")).unwrap();
        server
            .apply_incremental_update(&root, &crate::utils::IndexLock::acquire(&root).unwrap())
            .unwrap();
        assert!(paths().is_empty());
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn repeated_queries_reverify_changed_and_deleted_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        std::fs::write(&path, "needle\n").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        let server = IndexServer::new(false);
        let query = || {
            server.handle_content_search(
                "needle".into(),
                Some(root.clone()),
                0,
                ContentSearchOptions::default(),
            )
        };
        let count = |r| match r {
            Response::ContentSearch(r) => r.matches.len(),
            other => panic!("{other:?}"),
        };
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let rewrite = |text: &str| {
            std::fs::write(&path, text).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        };
        assert_eq!(count(query()), 1);
        rewrite("absent\n");
        assert_eq!(count(query()), 0);
        rewrite("needle\n");
        assert_eq!(count(query()), 1);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(count(query()), 0);
        drop(server);
        crate::utils::remove_index(&root).unwrap();
    }

    #[test]
    fn delta_recovery_reuses_held_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("test.txt"), "recovery needle\n").unwrap();
        build_index_with_progress(&root, true, true).unwrap();
        std::fs::write(get_index_dir(&root).unwrap().join("meta.json"), "broken").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            let server = IndexServer::new(false);
            server
                .apply_incremental_update(
                    &worker_root,
                    &crate::utils::IndexLock::acquire(&worker_root).unwrap(),
                )
                .unwrap();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("recovery deadlocked on its own lock");
        worker.join().unwrap();
        assert_eq!(IndexReader::open(&root).unwrap().meta.doc_count, 1);
        crate::utils::remove_index(&root).unwrap();
    }

    fn make_meta(
        delta_segments: Vec<u16>,
        delta_baseline: usize,
        tombstone_count: u32,
        doc_count: u32,
    ) -> IndexMeta {
        IndexMeta {
            delta_segments,
            delta_baseline,
            tombstone_count,
            doc_count,
            ..Default::default()
        }
    }

    #[test]
    fn test_should_compact_no_deltas() {
        // Fresh index with no new deltas
        let meta = make_meta(vec![], 0, 0, 1000);
        assert!(!should_compact(&meta, 15));
    }

    #[test]
    fn test_should_compact_below_threshold() {
        // 5 new deltas, threshold is 15
        let meta = make_meta(vec![1, 2, 3, 4, 5], 0, 0, 1000);
        assert!(!should_compact(&meta, 15));
    }

    #[test]
    fn test_should_compact_at_threshold() {
        // 15 new deltas, threshold is 15
        let meta = make_meta((1..=15).collect(), 0, 0, 1000);
        assert!(should_compact(&meta, 15));
    }

    #[test]
    fn test_should_compact_with_baseline() {
        // Chunked index: 20 initial segments (baseline=19), 5 new deltas
        // Total delta_segments = 24, but only 5 are new
        let meta = make_meta((1..=24).collect(), 19, 0, 100000);
        assert!(!should_compact(&meta, 15));

        // Now add more to reach 15 new deltas (19 baseline + 15 new = 34 total)
        let meta = make_meta((1..=34).collect(), 19, 0, 100000);
        assert!(should_compact(&meta, 15));
    }

    #[test]
    fn test_should_compact_after_merge() {
        // After merge: baseline reset to 0, no deltas
        let meta = make_meta(vec![], 0, 0, 100000);
        assert!(!should_compact(&meta, 15));

        // After merge + 15 new deltas
        let meta = make_meta((1..=15).collect(), 0, 0, 100000);
        assert!(should_compact(&meta, 15));
    }

    #[test]
    fn test_should_compact_tombstone_ratio() {
        // High tombstone ratio should trigger even with few deltas
        // 20% tombstones (200/1000) exceeds 15% threshold
        let meta = make_meta(vec![1, 2], 0, 200, 1000);
        assert!(should_compact(&meta, 15));

        // 10% tombstones (100/1000) does not exceed threshold
        let meta = make_meta(vec![1, 2], 0, 100, 1000);
        assert!(!should_compact(&meta, 15));
    }

    #[test]
    fn test_should_compact_zero_docs() {
        // Edge case: empty index
        let meta = make_meta(vec![], 0, 0, 0);
        assert!(!should_compact(&meta, 15));
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
