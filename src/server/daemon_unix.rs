//! Unix index server daemon
//!
//! Keeps indexes loaded in memory and serves search requests over Unix socket.

use crate::index::reader::IndexReader;
use crate::query::{parse_query, QueryExecutor};
use crate::server::protocol::{
    read_message, write_message, ContentMatch, ContentSearchOptions, ContentSearchResponse,
    Request, Response, SearchMatchData, SearchResponse, StatusResponse,
};
use crate::server::{get_pid_path, get_socket_path};
use anyhow::{Context, Result};
use lru::LruCache;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::num::NonZeroUsize;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// LRU cache size for search results per index
const CACHE_SIZE: usize = 128;

/// Connection timeout
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum results to return to avoid exceeding message size limits
/// This caps unbounded (limit=0) requests to prevent excessive memory/transfer
/// Set very high since the protocol already has a 100MB message limit
const MAX_RESULTS_CAP: usize = 10_000_000;

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

        // Accept connections
        for stream in listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            match stream {
                Ok(stream) => {
                    // Set timeout
                    let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

                    // Handle in new thread
                    let server = Arc::clone(self);
                    thread::spawn(move || {
                        if let Err(e) = server.handle_connection(stream) {
                            eprintln!("fxid: connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("fxid: accept error: {}", e);
                }
            }
        }

        // Cleanup
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(&pid_path);

        Ok(())
    }

    /// Handle a single client connection
    fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = BufWriter::new(stream);

        loop {
            // Read request
            let request: Request = match read_message(&mut reader) {
                Ok(req) => req,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Client disconnected
                    break;
                }
                Err(e) => {
                    let resp = Response::Error {
                        message: format!("Invalid request: {}", e),
                    };
                    write_message(&mut writer, &resp)?;
                    continue;
                }
            };

            // Handle request
            let response = self.handle_request(request);

            // Send response
            write_message(&mut writer, &response)?;

            // Check for shutdown
            if matches!(response, Response::ShuttingDown) {
                break;
            }
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
        if let Ok(mut cache) = cached.query_cache.lock()
            && let Some(cached_matches) = cache.get(&query)
        {
            self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            self.stats.queries_served.fetch_add(1, Ordering::Relaxed);

            let mut matches = cached_matches.clone();
            // Only truncate if limit is non-zero (0 means use query's top:N limit)
            if limit > 0 {
                matches.truncate(limit);
            }

            return Response::Search(SearchResponse {
                matches,
                duration_ms: start.elapsed().as_secs_f64() * 1000.0,
                cached: true,
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
        // Only truncate if limit is non-zero (0 means use query's top:N limit)
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
        // Skip wrapping if pattern is already a regex (starts with "re:/")
        let query_str = if options.case_insensitive && !pattern.starts_with("re:/") {
            // Use regex for case-insensitive search
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
        // Cap unlimited requests to prevent exceeding message size limits
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

        // Estimate memory usage (rough)
        let memory_bytes: u64 = indexes
            .values()
            .map(|idx| {
                // Rough estimate: doc count * 100 bytes per doc + overhead
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

/// Daemonize the current process
pub fn daemonize() -> Result<()> {
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
                    let server = IndexServer::new();
                    if let Err(e) = server.run() {
                        // Can't really report this since stdout is closed
                        let _ = fs::write("/tmp/fxid-error.log", format!("{}", e));
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
