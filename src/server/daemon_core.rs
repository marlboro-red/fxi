//! Platform-independent daemon core.
//!
//! Owns the loaded indexes, request handling, result caches, and watcher
//! orchestration. The platform transports (`daemon_unix`: Unix socket,
//! `daemon_windows`: named pipe) only accept connections, frame messages,
//! and call [`IndexServer::handle_request`].

use crate::index::build::build_index_with_progress;
use crate::index::reader::IndexReader;
use crate::index::types::IndexMeta;
use crate::query::{QueryExecutor, parse_query};
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
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
    /// Last access time
    last_used: Mutex<Instant>,
    /// File watcher handle (if watching is active)
    watcher_handle: Mutex<Option<WatcherHandle>>,
}

impl CachedIndex {
    fn new(reader: IndexReader) -> Self {
        Self {
            reader: Mutex::new(Arc::new(reader)),
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

    /// Swap in a new reader, clearing caches
    fn set_pending_reader(&self, reader: IndexReader) {
        let new_reader = Arc::new(reader);
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
}

/// The index server daemon
pub struct IndexServer {
    /// Loaded indexes by canonical root path. Values are Arc'd so request
    /// handlers can clone out an index and release the map lock instead of
    /// holding it for the duration of a query.
    indexes: RwLock<HashMap<PathBuf, Arc<CachedIndex>>>,
    /// Server statistics
    stats: ServerStats,
    /// Shutdown flag
    pub(crate) shutdown: AtomicBool,
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
            stats: ServerStats::new(),
            shutdown: AtomicBool::new(false),
            watcher_tx,
            watcher_rx: Mutex::new(watcher_rx),
            watcher_config: config,
            pending_changes: Mutex::new(HashMap::new()),
            watch_enabled,
        })
    }

    /// Start the server (blocking)
    /// Run the watcher message processor
    pub(crate) fn run_watcher_processor(self: &Arc<Self>) {
        let flush_interval = self.watcher_config.delta_flush_duration();

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                // Flush any pending changes before shutting down
                self.flush_all_pending_changes();
                break;
            }

            // Try to receive messages with a timeout
            let msg = {
                if let Ok(rx) = self.watcher_rx.lock() {
                    rx.recv_timeout(Duration::from_millis(100)).ok()
                } else {
                    None
                }
            };

            if let Some(message) = msg {
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
                        // Clear pending changes for this index before rebuild
                        if let Ok(mut pending) = self.pending_changes.lock() {
                            pending.remove(&root_path);
                        }
                        self.trigger_rebuild(&root_path);
                    }
                    WatcherMessage::Error { root_path, message } => {
                        eprintln!(
                            "fxid: watcher error for {}: {}",
                            root_path.display(),
                            message
                        );
                    }
                }
            }

            // Check for indexes that need flushing
            self.flush_expired_changes(flush_interval);
        }
    }

    /// Accumulate changes for an index
    fn accumulate_changes(&self, root_path: PathBuf, batch: ChangeBatch) {
        if batch.is_empty() {
            return;
        }

        let mut pending = self.pending_changes.lock().unwrap();

        if let Some(existing) = pending.get_mut(&root_path) {
            // Merge into existing batch
            existing.batch.merge(batch);
        } else {
            // Create new pending entry
            pending.insert(
                root_path,
                PendingChanges {
                    batch,
                    first_change: Instant::now(),
                },
            );
        }
    }

    /// Flush changes for indexes where the flush interval has elapsed
    fn flush_expired_changes(&self, flush_interval: Duration) {
        // Collect indexes that need flushing
        let to_flush: Vec<PathBuf> = {
            let pending = self.pending_changes.lock().unwrap();
            pending
                .iter()
                .filter(|(_, changes)| changes.first_change.elapsed() >= flush_interval)
                .map(|(path, _)| path.clone())
                .collect()
        };

        // Flush each one
        for root_path in to_flush {
            self.flush_pending_changes(&root_path);
        }
    }

    /// Flush all pending changes (used during shutdown)
    pub(crate) fn flush_all_pending_changes(&self) {
        let paths: Vec<PathBuf> = {
            let pending = self.pending_changes.lock().unwrap();
            pending.keys().cloned().collect()
        };

        for root_path in paths {
            self.flush_pending_changes(&root_path);
        }
    }

    /// Flush pending changes for a specific index
    fn flush_pending_changes(&self, root_path: &PathBuf) {
        // Take the batch out
        let batch = {
            let mut pending = self.pending_changes.lock().unwrap();
            pending.remove(root_path).map(|p| p.batch)
        };

        if let Some(batch) = batch
            && !batch.is_empty()
        {
            self.handle_changes(root_path.clone(), batch);
        }
    }

    /// Handle a batch of file changes
    fn handle_changes(&self, root_path: PathBuf, batch: ChangeBatch) {
        let total = batch.total_changes();
        if total == 0 {
            return;
        }

        // Get current doc count for threshold calculation
        let doc_count = {
            let indexes = self.indexes.read().unwrap();
            indexes
                .get(&root_path)
                .map(|c| c.get_reader().meta.doc_count as usize)
                .unwrap_or(0)
        };

        // Calculate change percentage
        let change_percent = (total * 100) / doc_count.max(1);

        if change_percent > self.watcher_config.rebuild_threshold_percent {
            eprintln!(
                "fxid: {}% changes detected (>{} threshold), triggering rebuild for {}",
                change_percent,
                self.watcher_config.rebuild_threshold_percent,
                root_path.display()
            );
            self.trigger_rebuild(&root_path);
        } else {
            eprintln!(
                "fxid: applying {} changes to {} ({} created, {} modified, {} deleted)",
                total,
                root_path.display(),
                batch.created.len(),
                batch.modified.len(),
                batch.deleted.len()
            );
            self.apply_incremental_update(&root_path, batch);
        }
    }

    /// Apply an incremental update using delta segments
    fn apply_incremental_update(&self, root_path: &PathBuf, _batch: ChangeBatch) {
        let _lock = match crate::utils::IndexLock::acquire(root_path) {
            Ok(lock) => lock,
            Err(e) => {
                eprintln!("fxid: cannot lock index: {e}");
                return;
            }
        };
        // Notifications are hints, not an authoritative file list. Reconcile
        // through the same walker as CLI indexing so directory renames, nested
        // ignore rules, removals and symlinks have identical semantics.
        if let Err(e) = crate::index::build::update_index(root_path) {
            eprintln!("fxid: reconcile failed: {e}; rebuilding");
            self.rebuild_with_lock(root_path, &_lock);
            return;
        }
        let refreshed = IndexReader::open(root_path).and_then(|reader| {
            if should_compact(&reader.meta, self.watcher_config.merge_segment_threshold) {
                crate::index::compact::merge_segments(root_path)?;
                IndexReader::open(root_path)
            } else {
                Ok(reader)
            }
        });
        match refreshed {
            Ok(reader) => {
                if let Some(cached) = self.indexes.read().unwrap().get(root_path) {
                    cached.set_pending_reader(reader);
                }
            }
            Err(e) => eprintln!("fxid: cannot reload reconciled index: {e}"),
        }
    }

    /// Trigger a full index rebuild
    fn trigger_rebuild(&self, root_path: &PathBuf) {
        // Serialize against CLI indexers and other writers on this root
        let _lock = match crate::utils::IndexLock::acquire(root_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "fxid: failed to lock index for {}: {}",
                    root_path.display(),
                    e
                );
                return;
            }
        };

        self.rebuild_with_lock(root_path, &_lock);
    }

    /// Recovery from a failed delta already owns the mutation lock.
    fn rebuild_with_lock(&self, root_path: &PathBuf, _lock: &crate::utils::IndexLock) {
        // Stop the watcher during rebuild
        {
            let indexes = self.indexes.read().unwrap();
            if let Some(cached) = indexes.get(root_path) {
                cached.stop_watcher();
            }
        }

        // Rebuild
        if let Err(e) = build_index_with_progress(root_path, true, true) {
            eprintln!("fxid: failed to rebuild index: {}", e);
            return;
        }

        // Reload and restart watcher
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

                // Restart watching only for a watching server.
                if self.watch_enabled {
                    self.spawn_watcher(root_path);
                }
            }
            Err(e) => {
                eprintln!("fxid: failed to reload index after rebuild: {}", e);
            }
        }
    }

    /// Spawn a file watcher for the given root path
    fn spawn_watcher(&self, root_path: &PathBuf) {
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

        // Store the handle
        let indexes = self.indexes.read().unwrap();
        if let Some(cached) = indexes.get(root_path)
            && let Ok(mut watcher_handle) = cached.watcher_handle.lock()
        {
            *watcher_handle = Some(handle);
        }
    }

    /// Stop all active watchers
    pub(crate) fn stop_all_watchers(&self) {
        let indexes = self.indexes.read().unwrap();
        for cached in indexes.values() {
            cached.stop_watcher();
        }
    }

    pub(crate) fn handle_request(&self, request: Request) -> Response {
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

            Request::Shutdown => {
                self.shutdown.store(true, Ordering::Relaxed);
                Response::ShuttingDown
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

        // Resolve root path (canonicalize + walk up, or use single loaded index)
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
        let parsed = parse_query(&query);
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

        // Resolve root path (canonicalize + walk up, or use single loaded index)
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
        let mut parsed = parse_query(&pattern);
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
            .map(|idx| idx.get_reader().meta.doc_count)
            .sum();

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

    fn handle_reload(&self, root_path: Option<PathBuf>) -> Response {
        let root_path = match self.resolve_root(root_path) {
            Ok(p) => p,
            Err(resp) => return resp,
        };

        // Remove from cache to force reload
        {
            let mut indexes = self.indexes.write().unwrap();
            indexes.remove(&root_path);
        }

        // Load fresh
        match self.ensure_index_loaded(&root_path) {
            Ok(()) => {
                let indexes = self.indexes.read().unwrap();
                let doc_count = indexes
                    .get(&root_path)
                    .map(|c| c.get_reader().meta.doc_count)
                    .unwrap_or(0);
                Response::Reloaded {
                    success: true,
                    message: format!("Reloaded {} files", doc_count),
                    resolved_root: Some(root_path),
                }
            }
            Err(e) => Response::Reloaded {
                success: false,
                message: format!("Failed to reload: {}", e),
                resolved_root: Some(root_path),
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

        // Check if we need to load the index
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
                match crate::index::build::update_index(root_path) {
                    Ok(_) => {
                        // Swap in a fresh reader in case the scan changed it
                        if let Ok(reader) = IndexReader::open(root_path) {
                            let indexes = self.indexes.read().unwrap();
                            if let Some(cached) = indexes.get(root_path) {
                                cached.set_pending_reader(reader);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("fxid: reconcile failed for {}: {}", root_path.display(), e)
                    }
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
            break;
        }

        // Check for events with timeout
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                let needs_reconcile = match event {
                    Ok(event) => !matches!(event.kind, EventKind::Access(_)),
                    Err(_) => true,
                };
                if needs_reconcile {
                    // No existence/ignore check here: a removed path no longer
                    // exists, and changes to ignore rules alter membership.
                    debouncer.add_event(PathBuf::new(), ChangeKind::Modified);
                }
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
        server.apply_incremental_update(&root, ChangeBatch::default());
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
        server.apply_incremental_update(&root, ChangeBatch::default());
        assert!(paths().is_empty());
        std::fs::remove_file(root.join("new/.gitignore")).unwrap();
        server.apply_incremental_update(&root, ChangeBatch::default());
        assert_eq!(paths(), vec![PathBuf::from("new/a.txt")]);
        std::fs::remove_dir_all(root.join("new")).unwrap();
        server.apply_incremental_update(&root, ChangeBatch::default());
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
        assert_eq!(count(query()), 1);
        std::fs::write(&path, "absent\n").unwrap();
        assert_eq!(count(query()), 0);
        std::fs::write(&path, "needle\n").unwrap();
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
            server.apply_incremental_update(&worker_root, ChangeBatch::default());
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
