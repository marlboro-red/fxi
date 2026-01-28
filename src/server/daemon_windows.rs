//! Windows index server daemon
//!
//! Keeps indexes loaded in memory and serves search requests over named pipes.

use crate::index::reader::IndexReader;
use crate::query::{parse_query, QueryExecutor};
use crate::server::protocol::{
    read_message, write_message, ContentMatch, ContentSearchOptions, ContentSearchResponse,
    Request, Response, SearchMatchData, SearchResponse, StatusResponse,
};
use crate::server::{get_pid_path, get_pipe_name};
use anyhow::{Context, Result};
use lru::LruCache;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::num::NonZeroUsize;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Cached index with its query cache
struct CachedIndex {
    reader: Arc<IndexReader>,
    query_cache: Mutex<LruCache<String, Vec<SearchMatchData>>>,
    last_used: Mutex<Instant>,
}

impl CachedIndex {
    fn new(reader: IndexReader) -> Self {
        Self {
            reader: Arc::new(reader),
            query_cache: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
            last_used: Mutex::new(Instant::now()),
        }
    }

    fn touch(&self) {
        if let Ok(mut last) = self.last_used.lock() {
            *last = Instant::now();
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

/// The index server daemon
pub struct IndexServer {
    /// Loaded indexes by canonical root path
    indexes: RwLock<HashMap<PathBuf, CachedIndex>>,
    /// Server statistics
    stats: ServerStats,
    /// Shutdown flag
    shutdown: AtomicBool,
}

impl IndexServer {
    /// Create a new index server wrapped in Arc
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            indexes: RwLock::new(HashMap::new()),
            stats: ServerStats::new(),
            shutdown: AtomicBool::new(false),
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

        // Ensure index is loaded
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

        let executor = QueryExecutor::new(&cached.reader);
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

        // Ensure index is loaded
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

        let executor = QueryExecutor::new(&cached.reader);

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

    /// Ensure an index is loaded
    fn ensure_index_loaded(&self, root_path: &PathBuf) -> Result<()> {
        // Check with read lock first
        {
            let indexes = self.indexes.read().unwrap();
            if indexes.contains_key(root_path) {
                return Ok(());
            }
        }

        // Load with write lock
        let mut indexes = self.indexes.write().unwrap();

        // Double-check after acquiring write lock
        if indexes.contains_key(root_path) {
            return Ok(());
        }

        eprintln!("fxid: loading index for {}", root_path.display());
        let reader = IndexReader::open(root_path)?;
        let doc_count = reader.meta.doc_count;

        indexes.insert(root_path.clone(), CachedIndex::new(reader));
        eprintln!(
            "fxid: loaded {} files from {}",
            doc_count,
            root_path.display()
        );

        Ok(())
    }
}

/// Daemonize the current process (Windows version - runs as background process)
pub fn daemonize() -> Result<()> {
    use std::process::Command;

    // On Windows, we spawn a detached child process
    let exe = std::env::current_exe()?;

    // Start the server in foreground mode as a detached process
    Command::new(&exe)
        .args(["server", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| "Failed to spawn daemon process")?;

    // Give it a moment to start
    thread::sleep(Duration::from_millis(100));

    Ok(())
}

/// Start the daemon in foreground (for debugging)
pub fn run_foreground() -> Result<()> {
    let server = IndexServer::new();
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
