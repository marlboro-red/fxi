//! Unix index server daemon
//!
//! Keeps indexes loaded in memory and serves search requests over Unix socket.
//! Supports live file watching for automatic index updates.

use crate::index::build::{build_index_with_progress, is_known_binary_ext, ProcessedFile};
use crate::index::reader::IndexReader;
use crate::index::types::{DocFlags, IndexMeta, Language};
use crate::index::writer::DeltaSegmentWriter;
use crate::query::{parse_query, QueryExecutor};
use crate::utils::{
    extract_tokens_and_positions, extract_trigrams, get_index_dir, is_binary,
    is_minified,
};
use crate::server::debouncer::EventDebouncer;
use crate::server::protocol::{
    read_message_with_id, write_message_with_id, ContentMatch, ContentSearchOptions,
    ContentSearchResponse, Request, Response, SearchMatchData, SearchResponse, StatusResponse,
    PROTOCOL_VERSION,
};
use crate::server::watcher::{
    build_gitignore_matcher, should_ignore_path, ChangeBatch, ChangeKind, WatcherConfig,
    WatcherHandle, WatcherMessage,
};
use crate::server::{get_pid_path, get_socket_path};
use anyhow::{Context, Result};
use lru::LruCache;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::num::NonZeroUsize;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// LRU cache size for search results per index
const CACHE_SIZE: usize = 128;

/// Compaction threshold: trigger merge when tombstone ratio exceeds this
const COMPACTION_TOMBSTONE_THRESHOLD: f32 = 0.15; // 15%

/// Connection timeout
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum concurrent connection handlers
const MAX_CONCURRENT_CONNECTIONS: u64 = 64;

/// Default maximum pipelined requests per connection
const DEFAULT_MAX_PIPELINED: usize = 32;

/// Maximum results to return to avoid exceeding message size limits
/// This caps unbounded (limit=0) requests to prevent excessive memory/transfer
/// Set very high since the protocol already has a 100MB message limit
const MAX_RESULTS_CAP: usize = 10_000_000;

/// Cached index with its query cache and optional file watcher
struct CachedIndex {
    /// Current reader (swapped atomically via Mutex)
    reader: Mutex<Arc<IndexReader>>,
    /// Query result cache (cleared on reader swap). Entries are Arc'd so a
    /// cache hit clones only the (limit-truncated) response, not the full
    /// uncapped result set.
    query_cache: Mutex<LruCache<String, Arc<Vec<SearchMatchData>>>>,
    /// Content search result cache (cleared on reader swap)
    content_cache: Mutex<LruCache<String, (Arc<Vec<ContentMatch>>, usize)>>,
    /// Last access time
    last_used: Mutex<Instant>,
    /// Reader version for cache invalidation
    reader_version: AtomicU64,
    /// File watcher handle (if watching is active)
    watcher_handle: Mutex<Option<WatcherHandle>>,
}

impl CachedIndex {
    fn new(reader: IndexReader) -> Self {
        Self {
            reader: Mutex::new(Arc::new(reader)),
            query_cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
            content_cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
            last_used: Mutex::new(Instant::now()),
            reader_version: AtomicU64::new(0),
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
        // Clear caches since index changed
        if let Ok(mut cache) = self.query_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.content_cache.lock() {
            cache.clear();
        }
        // Increment version
        self.reader_version.fetch_add(1, Ordering::SeqCst);
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
        if let Ok(mut handle) = self.watcher_handle.lock() {
            if let Some(mut h) = handle.take() {
                h.stop();
            }
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

/// Read per-connection pipelining limit from env or use default
fn max_pipelined() -> usize {
    std::env::var("FXI_MAX_PIPELINED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_PIPELINED)
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
    shutdown: AtomicBool,
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
            watch_enabled, config.debounce_ms, config.delta_flush_interval_secs,
            config.merge_segment_threshold, config.rebuild_threshold_percent
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
    pub fn run(self: &Arc<Self>) -> Result<()> {
        let socket_path = get_socket_path();
        let pid_path = get_pid_path();

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Remove stale socket file
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }

        // Write PID file
        fs::write(&pid_path, format!("{}", std::process::id()))?;

        // Bind to socket
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind to {}", socket_path.display()))?;

        // Set socket permissions (user only)
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        }

        eprintln!("fxid: listening on {}", socket_path.display());

        // Start watcher processor thread
        let server_for_watcher = Arc::clone(self);
        let watcher_processor = thread::spawn(move || {
            server_for_watcher.run_watcher_processor();
        });

        // Accept connections with concurrency limit
        let active_connections = Arc::new(AtomicU64::new(0));
        for stream in listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            match stream {
                Ok(stream) => {
                    // Check connection limit
                    if active_connections.load(Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
                        eprintln!("fxid: too many connections, rejecting");
                        continue;
                    }

                    // Set timeout
                    let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

                    // Handle in new thread
                    let server = Arc::clone(self);
                    let conn_count = Arc::clone(&active_connections);
                    conn_count.fetch_add(1, Ordering::Relaxed);
                    thread::spawn(move || {
                        if let Err(e) = server.handle_connection(stream) {
                            eprintln!("fxid: connection error: {}", e);
                        }
                        conn_count.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(e) => {
                    eprintln!("fxid: accept error: {}", e);
                }
            }
        }

        // Stop all watchers
        self.stop_all_watchers();

        // Wait for watcher processor to finish
        let _ = watcher_processor.join();

        // Cleanup
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(&pid_path);

        Ok(())
    }

    /// Run the watcher message processor
    fn run_watcher_processor(self: &Arc<Self>) {
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
                        eprintln!("fxid: rebuild requested for {}: {}", root_path.display(), reason);
                        // Clear pending changes for this index before rebuild
                        if let Ok(mut pending) = self.pending_changes.lock() {
                            pending.remove(&root_path);
                        }
                        self.trigger_rebuild(&root_path);
                    }
                    WatcherMessage::Error { root_path, message } => {
                        eprintln!("fxid: watcher error for {}: {}", root_path.display(), message);
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
    fn flush_all_pending_changes(&self) {
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

        if let Some(batch) = batch {
            if !batch.is_empty() {
                self.handle_changes(root_path.clone(), batch);
            }
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

        if doc_count == 0 {
            return;
        }

        // Calculate change percentage
        let change_percent = (total * 100) / doc_count;

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
    fn apply_incremental_update(&self, root_path: &PathBuf, batch: ChangeBatch) {
        // Load current meta to check delta segment count
        let index_path = match get_index_dir(root_path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("fxid: failed to get index dir: {}", e);
                return;
            }
        };

        let meta_path = index_path.join("meta.json");
        let mut meta: IndexMeta = match std::fs::File::open(&meta_path)
            .map_err(anyhow::Error::from)
            .and_then(|f| serde_json::from_reader(f).map_err(anyhow::Error::from))
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("fxid: failed to read meta.json: {}", e);
                self.trigger_rebuild(root_path);
                return;
            }
        };

        // Calculate next segment ID
        let next_segment_id = meta.delta_segments.iter().max().copied()
            .unwrap_or(meta.base_segment.unwrap_or(0)) + 1;

        // Create delta writer
        let mut writer = match DeltaSegmentWriter::new(root_path, next_segment_id) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("fxid: failed to create delta writer: {}", e);
                self.trigger_rebuild(root_path);
                return;
            }
        };

        // Mark tombstones for deleted + modified files
        for path in batch.deleted.iter().chain(batch.modified.iter()) {
            eprintln!("fxid: [delta] marking tombstone: {}", path.display());
            writer.mark_tombstone(path);
        }

        // Process created + modified files
        let mut added_count = 0;
        for rel_path in batch.created.iter().chain(batch.modified.iter()) {
            let full_path = root_path.join(rel_path);

            if let Some(processed) = process_file_for_delta(&full_path, rel_path) {
                eprintln!("fxid: [delta] indexing: {} ({} bytes)", rel_path.display(), processed.size);
                writer.add_file(processed);
                added_count += 1;
            }
        }
        eprintln!("fxid: [delta] {} files indexed, {} tombstones marked",
            added_count, batch.deleted.len() + batch.modified.len());

        // Check if there are any changes to write
        if !writer.has_changes() {
            eprintln!("fxid: no valid changes to apply");
            return;
        }

        // Finalize (writes segment + updates global files atomically)
        if let Err(e) = writer.finalize(&mut meta) {
            eprintln!("fxid: failed to finalize delta segment: {}", e);
            self.trigger_rebuild(root_path);
            return;
        }

        // Check if compaction is needed after this delta segment
        if should_compact(&meta, self.watcher_config.merge_segment_threshold) {
            let new_deltas = meta.delta_segments.len().saturating_sub(meta.delta_baseline);
            eprintln!("fxid: triggering segment merge (tombstones={}, new_deltas={}, threshold={})...",
                meta.tombstone_count, new_deltas, self.watcher_config.merge_segment_threshold);
            if let Err(e) = crate::index::compact::merge_segments(root_path) {
                eprintln!("fxid: merge failed, falling back to rebuild: {}", e);
                self.trigger_rebuild(root_path);
                return;
            }
            eprintln!("fxid: segment merge completed successfully");
        }

        // Hot-swap reader
        match IndexReader::open(root_path) {
            Ok(reader) => {
                let indexes = self.indexes.read().unwrap();
                if let Some(cached) = indexes.get(root_path) {
                    cached.set_pending_reader(reader);
                    eprintln!("fxid: index updated for {} (delta segment {})",
                        root_path.display(), next_segment_id);
                }
            }
            Err(e) => {
                eprintln!("fxid: failed to reload index: {}", e);
            }
        }
    }

    /// Trigger a full index rebuild
    fn trigger_rebuild(&self, root_path: &PathBuf) {
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

                // Restart watcher
                self.spawn_watcher(root_path);
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
        if let Some(cached) = indexes.get(root_path) {
            if let Ok(mut watcher_handle) = cached.watcher_handle.lock() {
                *watcher_handle = Some(handle);
            }
        }
    }

    /// Stop all active watchers
    fn stop_all_watchers(&self) {
        let indexes = self.indexes.read().unwrap();
        for cached in indexes.values() {
            cached.stop_watcher();
        }
    }

    /// Handle a single client connection with pipelining support.
    ///
    /// Uses `std::thread::scope` to spawn a writer thread and per-request
    /// handler threads. Requests are read with `read_message_with_id` and
    /// responses are written with `write_message_with_id`, preserving the
    /// optional `request_id` for client-side correlation.
    fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let reader_stream = stream.try_clone()?;
        let _ = reader_stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

        let (tx, rx) = std::sync::mpsc::channel::<(Response, Option<String>)>();
        let max_handlers = max_pipelined();
        let active = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|s| {
            // Writer thread: drains the channel and writes responses
            s.spawn(move || {
                let mut writer = BufWriter::new(stream);
                while let Ok((response, request_id)) = rx.recv() {
                    if write_message_with_id(&mut writer, &response, request_id.as_deref())
                        .is_err()
                    {
                        break;
                    }
                }
            });

            // Reader loop: reads requests and spawns handler threads
            let mut reader = BufReader::new(reader_stream);
            loop {
                let (request, request_id): (Request, _) = match read_message_with_id(&mut reader)
                {
                    Ok(r) => r,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => {
                        let _ = tx.send((
                            Response::Error {
                                message: format!("Invalid request: {}", e),
                            },
                            None,
                        ));
                        continue;
                    }
                };

                // Concurrency limit
                if active.fetch_add(1, Ordering::Relaxed) >= max_handlers {
                    active.fetch_sub(1, Ordering::Relaxed);
                    let _ = tx.send((
                        Response::Error {
                            message: "Too many concurrent requests".into(),
                        },
                        request_id,
                    ));
                    continue;
                }

                let is_shutdown = matches!(request, Request::Shutdown);
                let tx = tx.clone();
                let active = &active;

                s.spawn(move || {
                    let response = self.handle_request(request);
                    let _ = tx.send((response, request_id));
                    active.fetch_sub(1, Ordering::Relaxed);
                });

                if is_shutdown {
                    break;
                }
            }

            drop(tx); // signal writer to finish after in-flight handlers complete
        });

        Ok(())
    }

    /// Handle a single request
    fn handle_request(&self, request: Request) -> Response {
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

            Request::Hello { protocol_version: _ } => Response::Hello {
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
                    }
                }
            }
        };

        cached.touch();

        // Get the reader (handles pending swap)
        let reader = cached.get_reader();

        // Check query cache first
        if let Ok(mut cache) = cached.query_cache.lock()
            && let Some(cached_matches) = cache.get(&query)
        {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

            // Clone only the part of the cached set that is returned
            // (0 means use query's top:N limit -> full set)
            let matches = if limit > 0 && limit < cached_matches.len() {
                cached_matches[..limit].to_vec()
            } else {
                cached_matches.as_ref().clone()
            };

            return Response::Search(SearchResponse {
                matches,
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                cached: true,
                resolved_root: Some(root_path.clone()),
            });
        }

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
                }
            }
        };

        // Convert to serializable format
        let match_data: Arc<Vec<SearchMatchData>> = Arc::new(
            matches
                .iter()
                .map(|m| SearchMatchData {
                    path: m.path.clone(),
                    line_number: m.line_number,
                    score: m.score,
                })
                .collect(),
        );

        // Cache the results (refcount bump, not a copy)
        if let Ok(mut cache) = cached.query_cache.lock() {
            cache.put(query, Arc::clone(&match_data));
        }

        self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

        // Only truncate if limit is non-zero (0 means use query's top:N limit)
        let result_matches = if limit > 0 && limit < match_data.len() {
            match_data[..limit].to_vec()
        } else {
            match_data.as_ref().clone()
        };

        Response::Search(SearchResponse {
            matches: result_matches,
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
                    }
                }
            }
        };

        cached.touch();

        // Get the reader (handles pending swap)
        let reader = cached.get_reader();

        // Build cache key from pattern + options + limit
        let cache_key = format!(
            "{}\x00{}\x00{}\x00{}\x00{}\x00{}",
            pattern, options.context_before, options.context_after,
            options.case_insensitive, options.files_only, limit
        );

        // Check content cache first
        if let Ok(mut cache) = cached.content_cache.lock()
            && let Some((cached_matches, cached_file_count)) = cache.get(&cache_key)
        {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

            return Response::ContentSearch(ContentSearchResponse {
                matches: cached_matches.as_ref().clone(),
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: *cached_file_count,
                resolved_root: Some(root_path.clone()),
            });
        }

        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Parse and execute query. Case-insensitivity is applied at the plan
        // level: the planner narrows through the lowercased token index and
        // verifiers ignore case.
        let mut parsed = parse_query(&pattern);
        parsed.options.case_insensitive = options.case_insensitive;
        if parsed.is_empty() {
            return Response::ContentSearch(ContentSearchResponse {
                matches: vec![],
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: 0,
                resolved_root: Some(root_path.clone()),
            });
        }

        let executor = QueryExecutor::new(&reader);

        // Use optimized files-only path when requested
        if options.files_only {
            let effective_limit = if limit == 0 { MAX_RESULTS_CAP } else { limit.min(MAX_RESULTS_CAP) };
            let matching_files = match executor.execute_files_only(&parsed, effective_limit) {
                Ok(files) => files,
                Err(e) => {
                    return Response::Error {
                        message: format!("Search failed: {}", e),
                    }
                }
            };

            // Convert to minimal ContentMatch (just path, no content)
            let file_count = matching_files.len();
            let match_data: Arc<Vec<ContentMatch>> = Arc::new(
                matching_files
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
                    .collect(),
            );

            // Cache the results (refcount bump, not a copy)
            if let Ok(mut cache) = cached.content_cache.lock() {
                cache.put(cache_key, (Arc::clone(&match_data), file_count));
            }

            self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

            return Response::ContentSearch(ContentSearchResponse {
                matches: match_data.as_ref().clone(),
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: file_count,
                resolved_root: Some(root_path.clone()),
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
                }
            }
        };

        // Count unique files (dedup by borrowed path, no clones)
        let unique_files: std::collections::HashSet<&std::path::Path> =
            matches.iter().map(|m| m.path.as_path()).collect();
        let file_count = unique_files.len();
        drop(unique_files);

        // Convert to protocol type and apply limit
        let effective_limit = if limit == 0 { MAX_RESULTS_CAP } else { limit.min(MAX_RESULTS_CAP) };
        let iter = matches.into_iter().take(effective_limit);
        let match_data: Arc<Vec<ContentMatch>> = Arc::new(
            iter.map(|m| ContentMatch {
                path: m.path,
                line_number: m.line_number,
                line_content: m.line_content,
                match_start: m.match_start,
                match_end: m.match_end,
                context_before: m.context_before,
                context_after: m.context_after,
            })
            .collect(),
        );

        // Cache the results (refcount bump, not a copy)
        if let Ok(mut cache) = cached.content_cache.lock() {
            cache.put(cache_key, (Arc::clone(&match_data), file_count));
        }

        self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

        Response::ContentSearch(ContentSearchResponse {
            matches: match_data.as_ref().clone(),
            duration_ms: start.elapsed().as_secs_f64() * 1000.0,
            files_with_matches: file_count,
            resolved_root: Some(root_path),
        })
    }

    /// Handle status request
    fn handle_status(&self) -> Response {
        let indexes = self.indexes.read().unwrap();

        let total_docs: u32 = indexes.values().map(|idx| idx.get_reader().meta.doc_count).sum();

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
                indexes
                    .get(root_path)
                    .is_some_and(|c| !c.is_watching())
            };

            if should_start_watcher {
                // Reconcile before watching: the watcher only sees events
                // from now on, so changes made while no watcher was running
                // must be picked up by one incremental scan or the index
                // would stay stale until a manual `fxi index`
                eprintln!("fxid: reconciling index for {}", root_path.display());
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
    let new_deltas = meta.delta_segments.len().saturating_sub(meta.delta_baseline);
    new_deltas >= segment_threshold
}

/// Process a single file for delta segment indexing
fn process_file_for_delta(full_path: &std::path::Path, rel_path: &std::path::Path) -> Option<ProcessedFile> {
    use std::time::UNIX_EPOCH;

    // Fast-path for known binary extensions
    let ext = rel_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    if is_known_binary_ext(ext) {
        return None;
    }

    // Get metadata and check size BEFORE reading file content
    let metadata = match std::fs::metadata(full_path) {
        Ok(m) => m,
        Err(_) => return None,
    };

    // Skip empty or too large files
    const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB
    if metadata.len() == 0 || metadata.len() > MAX_FILE_SIZE {
        return None;
    }

    // Read file content
    let content = match std::fs::read(full_path) {
        Ok(c) => c,
        Err(_) => return None,
    };

    // Check if binary
    if is_binary(&content) {
        return None;
    }

    // Detect language
    let language = Language::from_extension(ext);

    // Check for minified
    let mut flags = DocFlags::new();
    if is_minified(&content) {
        flags.0 |= DocFlags::MINIFIED;
    }

    // Extract trigrams
    let trigrams: Vec<u32> = extract_trigrams(&content);

    // Extract tokens and token positions in a single scan of the content
    let (tokens, token_positions): (Vec<String>, Vec<(u32, u32)>) =
        if let Ok(text) = std::str::from_utf8(&content) {
            extract_tokens_and_positions(text)
        } else {
            (Vec::new(), Vec::new())
        };

    // Build line map
    let line_offsets = build_line_map_simple(&content);

    // Get modification time
    let mtime = metadata
        .modified()
        .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64)
        .unwrap_or(0);

    Some(ProcessedFile {
        rel_path: rel_path.to_path_buf(),
        mtime,
        size: content.len() as u64,
        language,
        flags,
        trigrams,
        tokens,
        token_positions,
        line_offsets,
    })
}

/// Build line offset map from content
fn build_line_map_simple(content: &[u8]) -> Vec<u32> {
    let mut offsets = vec![0u32];
    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' && i + 1 < content.len() {
            offsets.push((i + 1) as u32);
        }
    }
    offsets
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
            if let Ok(event) = res {
                let _ = event_tx.send(event);
            }
        },
        notify::Config::default(),
    )?;

    // Start watching
    watcher.watch(&root_path, RecursiveMode::Recursive)?;

    // Build gitignore matcher once for the root
    let gitignore = build_gitignore_matcher(&root_path);

    eprintln!("fxid: watching {} for changes", root_path.display());

    // Event processing loop
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Check for events with timeout
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                // Convert notify event to our change kind
                let kind = match event.kind {
                    EventKind::Create(_) => Some(ChangeKind::Created),
                    EventKind::Modify(_) => Some(ChangeKind::Modified),
                    EventKind::Remove(_) => Some(ChangeKind::Deleted),
                    _ => None,
                };

                if let Some(change_kind) = kind {
                    for path in event.paths {
                        // Skip non-files and hidden/ignored paths
                        if !path.is_file() {
                            continue;
                        }

                        // Get relative path
                        if let Ok(rel_path) = path.strip_prefix(&root_path) {
                            // Skip ignored paths (hardcoded dirs, hidden files, gitignore patterns)
                            if should_ignore_path(&gitignore, rel_path, false) {
                                continue;
                            }

                            // Log the detected change
                            let change_type = match change_kind {
                                ChangeKind::Created => "created",
                                ChangeKind::Modified => "modified",
                                ChangeKind::Deleted => "deleted",
                                ChangeKind::Renamed => "renamed",
                            };
                            eprintln!("fxid: [watch] {} {}", change_type, rel_path.display());

                            debouncer.add_event(rel_path.to_path_buf(), change_kind);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Check if debounce window has elapsed
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }

        // Check if we should flush the debouncer
        if debouncer.has_pending() && debouncer.is_ready() {
            if let Some(batch) = debouncer.flush() {
                let _ = tx.send(WatcherMessage::ChangesReady {
                    root_path: root_path.clone(),
                    batch,
                });
            }
        }
    }

    Ok(())
}

/// Daemonize the current process
pub fn daemonize(watch: bool) -> Result<()> {
    // Fork using double-fork technique for proper daemonization
    match unsafe { libc::fork() } {
        -1 => anyhow::bail!("First fork failed"),
        0 => {
            // Child process
            // Create new session
            if unsafe { libc::setsid() } == -1 {
                anyhow::bail!("setsid failed");
            }

            // Second fork to prevent acquiring a controlling terminal
            match unsafe { libc::fork() } {
                -1 => anyhow::bail!("Second fork failed"),
                0 => {
                    // Grandchild - this becomes the daemon
                    // Close standard file descriptors
                    unsafe {
                        libc::close(0);
                        libc::close(1);
                        libc::close(2);

                        // Redirect to /dev/null
                        let null = libc::open(
                            c"/dev/null".as_ptr(),
                            libc::O_RDWR,
                        );
                        if null != -1 {
                            libc::dup2(null, 0);
                            libc::dup2(null, 1);
                            libc::dup2(null, 2);
                            if null > 2 {
                                libc::close(null);
                            }
                        }
                    }

                    // Change to root directory to avoid holding mounts
                    let _ = std::env::set_current_dir("/");

                    // Now run the server
                    let server = IndexServer::new(watch);
                    if let Err(e) = server.run() {
                        // Can't really report this since stdout is closed
                        // Write to user-specific path to avoid symlink attacks on /tmp
                        if let Some(data_dir) = dirs::data_local_dir() {
                            let log_dir = data_dir.join("fxi");
                            let _ = fs::create_dir_all(&log_dir);
                            let _ = fs::write(log_dir.join("fxid-error.log"), format!("{}", e));
                        }
                    }
                    std::process::exit(0);
                }
                _ => {
                    // First child exits immediately
                    std::process::exit(0);
                }
            }
        }
        _ => {
            // Parent process - wait for first child then exit
            unsafe {
                let mut status: libc::c_int = 0;
                libc::wait(&mut status);
            }
            Ok(())
        }
    }
}

/// Start the daemon in foreground (for debugging)
pub fn run_foreground(watch: bool) -> Result<()> {
    let server = IndexServer::new(watch);
    server.run()
}

/// Stop the running daemon
pub fn stop_daemon() -> Result<bool> {
    let pid_path = get_pid_path();

    if !pid_path.exists() {
        return Ok(false);
    }

    let pid_str = fs::read_to_string(&pid_path)?;
    let pid: i32 = pid_str.trim().parse()?;

    // Send SIGTERM
    unsafe {
        if libc::kill(pid, libc::SIGTERM) == 0 {
            // Wait a bit for graceful shutdown
            thread::sleep(Duration::from_millis(500));

            // Check if still running, send SIGKILL if needed
            if libc::kill(pid, 0) == 0 {
                thread::sleep(Duration::from_secs(1));
                if libc::kill(pid, 0) == 0 {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    // Clean up socket and pid files
    let socket_path = get_socket_path();
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&pid_path);

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta(delta_segments: Vec<u16>, delta_baseline: usize, tombstone_count: u32, doc_count: u32) -> IndexMeta {
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
