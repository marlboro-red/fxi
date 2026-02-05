//! Windows index server daemon
//!
//! Keeps indexes loaded in memory and serves search requests over named pipes.
//! Supports live file watching for automatic index updates.

use crate::index::build::{build_index_with_progress, ProcessedFile};
use crate::index::reader::IndexReader;
use crate::index::types::{DocFlags, IndexMeta, Language};
use crate::index::writer::DeltaSegmentWriter;
use crate::query::{parse_query, QueryExecutor};
use crate::utils::{extract_tokens, extract_trigrams, get_index_dir, is_binary, is_minified};
use crate::server::debouncer::EventDebouncer;
use crate::server::protocol::{
    read_message, write_message, ContentMatch, ContentSearchOptions, ContentSearchResponse,
    Request, Response, SearchMatchData, SearchResponse, StatusResponse,
};
use crate::server::watcher::{
    build_gitignore_matcher, should_ignore_path, ChangeBatch, ChangeKind, WatcherConfig,
    WatcherHandle, WatcherMessage,
};
use crate::server::{get_pid_path, get_pipe_name};
use anyhow::{Context, Result};
use lru::LruCache;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::num::NonZeroUsize;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

// Windows API constants
const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
const PIPE_TYPE_BYTE: u32 = 0x00000000;
const PIPE_READMODE_BYTE: u32 = 0x00000000;
const PIPE_WAIT: u32 = 0x00000000;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;
const ERROR_PIPE_CONNECTED: u32 = 535;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        lpName: *const u16,
        dwOpenMode: u32,
        dwPipeMode: u32,
        nMaxInstances: u32,
        nOutBufferSize: u32,
        nInBufferSize: u32,
        nDefaultTimeOut: u32,
        lpSecurityAttributes: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;

    fn ConnectNamedPipe(
        hNamedPipe: *mut std::ffi::c_void,
        lpOverlapped: *mut std::ffi::c_void,
    ) -> i32;

    fn DisconnectNamedPipe(hNamedPipe: *mut std::ffi::c_void) -> i32;

    fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;

    fn GetLastError() -> u32;

    fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::ffi::c_void;

    fn TerminateProcess(hProcess: *mut std::ffi::c_void, uExitCode: u32) -> i32;
}

const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// LRU cache size for search results per index
const CACHE_SIZE: usize = 128;

/// Compaction threshold: trigger merge when tombstone ratio exceeds this
const COMPACTION_TOMBSTONE_THRESHOLD: f32 = 0.15; // 15%

/// Connection timeout in milliseconds
const CONNECTION_TIMEOUT_MS: u32 = 30000;

/// Maximum results to return to avoid exceeding message size limits
const MAX_RESULTS_CAP: usize = 10_000_000;

/// Buffer size for named pipe
const PIPE_BUFFER_SIZE: u32 = 65536;

/// A Send-safe wrapper for a Windows HANDLE
#[derive(Clone, Copy)]
struct SendableHandle(isize);

// Safety: Windows HANDLEs can be used from any thread
unsafe impl Send for SendableHandle {}

impl SendableHandle {
    fn from_raw(ptr: *mut std::ffi::c_void) -> Self {
        Self(ptr as isize)
    }

    fn as_raw(&self) -> *mut std::ffi::c_void {
        self.0 as *mut std::ffi::c_void
    }
}

/// Wrapper for Windows handle that implements Read + Write
struct PipeHandle {
    handle: SendableHandle,
}

impl PipeHandle {
    #[allow(dead_code)]
    fn try_clone(&self) -> std::io::Result<Self> {
        // For simplicity, we don't actually clone the handle
        // The server uses separate reader/writer on the same handle
        Ok(Self { handle: self.handle })
    }
}

impl Read for PipeHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::FromRawHandle;
        // Convert to File temporarily to use its Read impl
        let file = unsafe { File::from_raw_handle(self.handle.as_raw() as *mut _) };
        let result = (&file).read(buf);
        // Prevent the File from closing our handle
        std::mem::forget(file);
        result
    }
}

impl Write for PipeHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use std::os::windows::io::FromRawHandle;
        let file = unsafe { File::from_raw_handle(self.handle.as_raw() as *mut _) };
        let result = (&file).write(buf);
        std::mem::forget(file);
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        use std::os::windows::io::FromRawHandle;
        let file = unsafe { File::from_raw_handle(self.handle.as_raw() as *mut _) };
        let result = (&file).flush();
        std::mem::forget(file);
        result
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        let raw = self.handle.as_raw();
        if raw != INVALID_HANDLE_VALUE && !raw.is_null() {
            unsafe {
                CloseHandle(raw);
            }
        }
    }
}

/// Convert a Rust string to a null-terminated wide string
fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Cached index with its query cache and optional file watcher
struct CachedIndex {
    /// Current reader (atomically swappable)
    reader: Arc<IndexReader>,
    /// Query result cache (cleared on reader swap)
    query_cache: Mutex<LruCache<String, Vec<SearchMatchData>>>,
    /// Last access time
    last_used: Mutex<Instant>,
    /// Pending new reader to swap in (set by watcher processor)
    pending_reader: Mutex<Option<Arc<IndexReader>>>,
    /// Reader version for cache invalidation
    reader_version: AtomicU64,
    /// File watcher handle (if watching is active)
    watcher_handle: Mutex<Option<WatcherHandle>>,
}

impl CachedIndex {
    fn new(reader: IndexReader) -> Self {
        Self {
            reader: Arc::new(reader),
            query_cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
            last_used: Mutex::new(Instant::now()),
            pending_reader: Mutex::new(None),
            reader_version: AtomicU64::new(0),
            watcher_handle: Mutex::new(None),
        }
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_used.lock() {
            *last = Instant::now();
        }
    }

    /// Get the current reader, checking for pending swap first
    fn get_reader(&self) -> Arc<IndexReader> {
        // Check for pending reader swap
        if let Ok(mut pending) = self.pending_reader.lock() {
            if let Some(new_reader) = pending.take() {
                // Clear query cache since index changed
                if let Ok(mut cache) = self.query_cache.lock() {
                    cache.clear();
                }
                // Increment version
                self.reader_version.fetch_add(1, Ordering::SeqCst);
                return new_reader;
            }
        }
        Arc::clone(&self.reader)
    }

    /// Set a pending reader to be swapped in on next access
    fn set_pending_reader(&self, reader: IndexReader) {
        if let Ok(mut pending) = self.pending_reader.lock() {
            *pending = Some(Arc::new(reader));
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

/// The index server daemon
pub struct IndexServer {
    /// Loaded indexes by canonical root path
    indexes: RwLock<HashMap<PathBuf, CachedIndex>>,
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
        let pipe_name = get_pipe_name();
        let pid_path = get_pid_path();

        // Ensure parent directory exists for PID file
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write PID file
        fs::write(&pid_path, format!("{}", std::process::id()))?;

        eprintln!("fxid: listening on {}", pipe_name);

        // Start watcher processor thread
        let server_for_watcher = Arc::clone(self);
        let watcher_processor = thread::spawn(move || {
            server_for_watcher.run_watcher_processor();
        });

        // Main server loop - create pipe instances and accept connections
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Create a new pipe instance
            let wide_name = to_wide_string(&pipe_name);
            let pipe_handle = unsafe {
                CreateNamedPipeW(
                    wide_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    PIPE_BUFFER_SIZE,
                    PIPE_BUFFER_SIZE,
                    CONNECTION_TIMEOUT_MS,
                    ptr::null_mut(),
                )
            };

            if pipe_handle == INVALID_HANDLE_VALUE {
                let err = unsafe { GetLastError() };
                eprintln!("fxid: failed to create pipe: error {}", err);
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            // Wait for client connection
            let connected = unsafe { ConnectNamedPipe(pipe_handle, ptr::null_mut()) };

            if connected == 0 {
                let err = unsafe { GetLastError() };
                if err != ERROR_PIPE_CONNECTED {
                    unsafe { CloseHandle(pipe_handle); }
                    continue;
                }
            }

            if self.shutdown.load(Ordering::Relaxed) {
                unsafe { CloseHandle(pipe_handle); }
                break;
            }

            // Handle connection in new thread
            let server = Arc::clone(self);
            let sendable_handle = SendableHandle::from_raw(pipe_handle);
            thread::spawn(move || {
                let handle = PipeHandle { handle: sendable_handle };
                if let Err(e) = server.handle_connection(handle) {
                    eprintln!("fxid: connection error: {}", e);
                }
            });
        }

        // Stop all watchers
        self.stop_all_watchers();

        // Wait for watcher processor to finish
        let _ = watcher_processor.join();

        // Cleanup
        let _ = fs::remove_file(&pid_path);

        Ok(())
    }

    /// Handle a single client connection
    fn handle_connection(&self, pipe: PipeHandle) -> Result<()> {
        let mut reader = BufReader::new(PipeHandle { handle: pipe.handle });
        let mut writer = BufWriter::new(PipeHandle { handle: pipe.handle });

        // Prevent the original pipe from being dropped (closing the handle)
        std::mem::forget(pipe);

        loop {
            // Read request
            let request: Request = match read_message(&mut reader) {
                Ok(req) => req,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    break;
                }
                Err(e) => {
                    let resp = Response::Error {
                        message: format!("Invalid request: {}", e),
                    };
                    let _ = write_message(&mut writer, &resp);
                    continue;
                }
            };

            // Handle request
            let response = self.handle_request(request);

            // Send response
            if write_message(&mut writer, &response).is_err() {
                break;
            }

            // Check for shutdown
            if matches!(response, Response::ShuttingDown) {
                break;
            }
        }

        // Disconnect and close the pipe
        unsafe {
            DisconnectNamedPipe(reader.into_inner().handle.as_raw());
        }

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
                .map(|c| c.reader.meta.doc_count as usize)
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
        }
    }

    /// Handle a search request
    fn handle_search(&self, query: String, root_path: PathBuf, limit: usize) -> Response {
        let start = Instant::now();

        // Canonicalize root path
        let root_path = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Response::Error {
                    message: format!("Invalid path: {}", e),
                }
            }
        };

        // Ensure index is loaded (and watcher started)
        if let Err(e) = self.ensure_index_loaded(&root_path) {
            return Response::Error {
                message: format!("Failed to load index: {}", e),
            };
        }

        // Access the index with read lock
        let indexes = self.indexes.read().unwrap();
        let cached = match indexes.get(&root_path) {
            Some(c) => c,
            None => {
                return Response::Error {
                    message: "Index not found after loading".to_string(),
                }
            }
        };

        cached.touch();

        // Get the reader (handles pending swap)
        let reader = cached.get_reader();

        // Check query cache first
        if let Ok(mut cache) = cached.query_cache.lock() {
            if let Some(cached_matches) = cache.get(&query) {
                self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

                let mut matches = cached_matches.clone();
                if limit > 0 {
                    matches.truncate(limit);
                }

                return Response::Search(SearchResponse {
                    matches,
                    duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                    cached: true,
                });
            }
        }

        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Parse and execute query
        let parsed = parse_query(&query);
        if parsed.is_empty() {
            return Response::Search(SearchResponse {
                matches: vec![],
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                cached: false,
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
        let match_data: Vec<SearchMatchData> = matches
            .iter()
            .map(|m| SearchMatchData {
                doc_id: m.doc_id,
                path: m.path.clone(),
                line_number: m.line_number,
                score: m.score,
            })
            .collect();

        // Cache the results
        if let Ok(mut cache) = cached.query_cache.lock() {
            cache.put(query, match_data.clone());
        }

        self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

        let mut result_matches = match_data;
        if limit > 0 {
            result_matches.truncate(limit);
        }

        Response::Search(SearchResponse {
            matches: result_matches,
            duration_ms: start.elapsed().as_secs_f64() * 1000.0,
            cached: false,
        })
    }

    /// Handle a content search request (ripgrep-like)
    fn handle_content_search(
        &self,
        pattern: String,
        root_path: PathBuf,
        limit: usize,
        options: ContentSearchOptions,
    ) -> Response {
        let start = Instant::now();

        // Canonicalize root path
        let root_path = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Response::Error {
                    message: format!("Invalid path: {}", e),
                }
            }
        };

        // Ensure index is loaded (and watcher started)
        if let Err(e) = self.ensure_index_loaded(&root_path) {
            return Response::Error {
                message: format!("Failed to load index: {}", e),
            };
        }

        // Access the index with read lock
        let indexes = self.indexes.read().unwrap();
        let cached = match indexes.get(&root_path) {
            Some(c) => c,
            None => {
                return Response::Error {
                    message: "Index not found after loading".to_string(),
                }
            }
        };

        cached.touch();

        // Get the reader (handles pending swap)
        let reader = cached.get_reader();

        // Build query - handle case insensitivity by wrapping pattern
        let query_str = if options.case_insensitive && !pattern.starts_with("re:/") {
            format!("re:/(?i){}/", regex::escape(&pattern))
        } else {
            pattern.clone()
        };

        // Parse and execute query
        let parsed = parse_query(&query_str);
        if parsed.is_empty() {
            return Response::ContentSearch(ContentSearchResponse {
                matches: vec![],
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: 0,
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
                matches: match_data,
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                files_with_matches: file_count,
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

        // Count unique files
        let mut unique_files = std::collections::HashSet::new();
        for m in &matches {
            unique_files.insert(m.path.clone());
        }

        // Convert to protocol type and apply limit
        let effective_limit = if limit == 0 { MAX_RESULTS_CAP } else { limit.min(MAX_RESULTS_CAP) };
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
            matches: match_data,
            duration_ms: start.elapsed().as_secs_f64() * 1000.0,
            files_with_matches: unique_files.len(),
        })
    }

    /// Handle status request
    fn handle_status(&self) -> Response {
        let indexes = self.indexes.read().unwrap();

        let total_docs: u32 = indexes.values().map(|idx| idx.reader.meta.doc_count).sum();

        let loaded_roots: Vec<PathBuf> = indexes.keys().cloned().collect();

        let memory_bytes: u64 = indexes
            .values()
            .map(|idx| {
                (idx.reader.meta.doc_count as u64) * 100 + 1024 * 1024
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
        })
    }

    /// Handle reload request
    fn handle_reload(&self, root_path: PathBuf) -> Response {
        let root_path = match root_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return Response::Reloaded {
                    success: false,
                    message: format!("Invalid path: {}", e),
                }
            }
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
                    .map(|c| c.reader.meta.doc_count)
                    .unwrap_or(0);
                Response::Reloaded {
                    success: true,
                    message: format!("Reloaded {} files", doc_count),
                }
            }
            Err(e) => Response::Reloaded {
                success: false,
                message: format!("Failed to reload: {}", e),
            },
        }
    }

    /// Ensure an index is loaded and watcher is running
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
            // Load with write lock
            let mut indexes = self.indexes.write().unwrap();

            // Double-check after acquiring write lock
            if !indexes.contains_key(root_path) {
                eprintln!("fxid: loading index for {}", root_path.display());
                let reader = IndexReader::open(root_path)?;
                let doc_count = reader.meta.doc_count;

                indexes.insert(root_path.clone(), CachedIndex::new(reader));
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
    let is_known_binary = matches!(ext.to_ascii_lowercase().as_str(),
        "dll" | "exe" | "pdb" | "so" | "dylib" | "a" | "lib" | "o" | "obj" |
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "nupkg" | "jar" |
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" | "tiff" |
        "woff" | "woff2" | "ttf" | "eot" | "otf" |
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" |
        "mp3" | "mp4" | "avi" | "mov" | "wav" | "ogg" | "flac" | "mkv" |
        "snk" | "pfx" | "p12" | "cer" | "crt" | "p7s" | "p7b" |
        "cache" | "db" | "sqlite" | "mdb" | "ldf" | "mdf"
    );

    if is_known_binary {
        return None;
    }

    // Read file content
    let content = match std::fs::read(full_path) {
        Ok(c) => c,
        Err(_) => return None,
    };

    // Get metadata
    let metadata = match std::fs::metadata(full_path) {
        Ok(m) => m,
        Err(_) => return None,
    };

    // Skip empty or too large files
    const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB
    if metadata.len() == 0 || metadata.len() > MAX_FILE_SIZE {
        return None;
    }

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

    // Extract tokens
    let tokens: Vec<String> = if let Ok(text) = std::str::from_utf8(&content) {
        extract_tokens(text).into_iter().collect()
    } else {
        Vec::new()
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

/// Daemonize the current process (Windows version - runs as background process)
pub fn daemonize(watch: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    // Windows process creation flags
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;

    // On Windows, we spawn a detached child process
    let exe = std::env::current_exe()?;

    // Start the server in foreground mode as a detached process
    let mut args = vec!["daemon", "foreground"];
    if watch {
        args.push("--watch");
    }
    Command::new(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| "Failed to spawn daemon process")?;

    // Give it a moment to start
    thread::sleep(Duration::from_millis(100));

    Ok(())
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
    let pid: u32 = pid_str.trim().parse()?;

    // Open the process and terminate it
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Process doesn't exist
            let _ = fs::remove_file(&pid_path);
            return Ok(false);
        }

        let result = TerminateProcess(handle, 0);
        CloseHandle(handle);

        if result == 0 {
            return Ok(false);
        }
    }

    // Wait a bit for process to exit
    thread::sleep(Duration::from_millis(500));

    // Clean up pid file
    let _ = fs::remove_file(&pid_path);

    Ok(true)
}
