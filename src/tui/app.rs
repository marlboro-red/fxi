use crate::index::build::build_index_with_progress;
use crate::index::reader::IndexReader;
use crate::index::types::SearchMatch;
use crate::query::{parse_query, QueryExecutor};
use crate::server::IndexClient;
use crate::utils::find_codebase_root;
use anyhow::Result;
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Preview,
    Help,
}

/// Index loading state for background loading
pub enum IndexLoadState {
    /// Index is loading in background
    Loading(Receiver<Result<IndexReader, String>>),
    /// Index loaded successfully
    Ready,
    /// No index found
    NotFound,
    /// Index loading failed (error message stored in status_message)
    Failed,
}

/// Search execution state for non-blocking search
pub enum SearchState {
    /// No search in progress
    Idle,
    /// Search is running in background
    Searching {
        query: String,
        receiver: Receiver<SearchResult>,
        start_time: Instant,
    },
}

/// Result from a background search
pub struct SearchResult {
    pub matches: Result<Vec<SearchMatch>, String>,
    pub query: String,
}

/// LRU cache size for search results (larger = more memory, faster re-queries)
const SEARCH_CACHE_SIZE: usize = 64;

/// Application state
pub struct App {
    /// The codebase root (detected or specified)
    pub root_path: PathBuf,
    /// Original path user started from (for relative path display)
    #[allow(dead_code)]
    pub start_path: PathBuf,
    /// Client connection to daemon (if available)
    client: Option<Arc<Mutex<IndexClient>>>,
    /// Whether we're using the daemon (vs direct index loading)
    using_daemon: bool,
    /// Shared reader for background search (Arc for thread safety)
    /// Only used when daemon is not available
    reader: Option<Arc<IndexReader>>,
    pub query: String,
    pub results: Vec<SearchMatch>,
    pub selected: usize,
    pub mode: Mode,
    /// Previous mode before entering help (to return to)
    pub previous_mode: Mode,
    pub preview_scroll: usize,
    pub preview_content: Option<String>,
    /// Path of the currently previewed file
    pub preview_path: Option<PathBuf>,
    pub status_message: String,
    pub index_available: bool,
    /// Pending key for vim multi-key commands (e.g., 'g' for 'gg')
    pub pending_key: Option<char>,
    /// Whether user is actively editing the query (vim bindings disabled until Enter)
    pub editing: bool,
    /// Background index loading state
    load_state: IndexLoadState,
    /// Background search state
    search_state: SearchState,
    /// LRU cache of recent search results for instant recall
    search_cache: LruCache<String, Vec<SearchMatch>>,
    /// Prefetched preview content for adjacent results
    prefetch_cache: HashMap<PathBuf, String>,
}

impl App {
    /// Create new app with INSTANT startup
    /// Tries to connect to daemon first for instant warm searches,
    /// falls back to background index loading if daemon unavailable
    pub fn new(path: PathBuf) -> Result<Self> {
        let start_path = path.canonicalize().unwrap_or(path);

        // Auto-detect codebase root (fast operation)
        let root_path = find_codebase_root(&start_path)?;

        // Try connecting to daemon first (instant if running)
        if let Some(mut client) = IndexClient::connect() {
            // Ping to verify connection works
            if client.ping().is_ok() {
                let status = format!(
                    "Connected to daemon (root: {})",
                    root_path.file_name().unwrap_or_default().to_string_lossy()
                );

                return Ok(Self {
                    root_path,
                    start_path,
                    client: Some(Arc::new(Mutex::new(client))),
                    using_daemon: true,
                    reader: None,
                    query: String::new(),
                    results: Vec::new(),
                    selected: 0,
                    mode: Mode::Search,
                    previous_mode: Mode::Search,
                    preview_scroll: 0,
                    preview_content: None,
                    preview_path: None,
                    status_message: status,
                    index_available: true, // Daemon handles index
                    pending_key: None,
                    editing: true,
                    load_state: IndexLoadState::Ready,
                    search_state: SearchState::Idle,
                    search_cache: LruCache::new(NonZeroUsize::new(SEARCH_CACHE_SIZE).unwrap()),
                    prefetch_cache: HashMap::new(),
                });
            }
        }

        // Fallback: load index directly (daemon not running)
        // Check if index exists (fast - just check file existence)
        let index_dir = crate::utils::get_index_dir(&root_path)?;
        let meta_path = index_dir.join("meta.json");

        let (load_state, status) = if meta_path.exists() {
            // Start loading index in background thread for instant TUI display
            let (tx, rx) = mpsc::channel();
            let root_for_thread = root_path.clone();

            thread::spawn(move || {
                let result = IndexReader::open(&root_for_thread)
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            });

            (IndexLoadState::Loading(rx), "Loading index...".to_string())
        } else {
            (IndexLoadState::NotFound, "No index found. Press F5 to build index.".to_string())
        };

        Ok(Self {
            root_path,
            start_path,
            client: None,
            using_daemon: false,
            reader: None,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            mode: Mode::Search,
            previous_mode: Mode::Search,
            preview_scroll: 0,
            preview_content: None,
            preview_path: None,
            status_message: status,
            index_available: false,
            pending_key: None,
            editing: true,
            load_state,
            search_state: SearchState::Idle,
            search_cache: LruCache::new(NonZeroUsize::new(SEARCH_CACHE_SIZE).unwrap()),
            prefetch_cache: HashMap::new(),
        })
    }

    /// Check for background index load completion (call this in event loop)
    pub fn poll_index_load(&mut self) {
        // Take ownership of the state temporarily
        let current_state = std::mem::replace(&mut self.load_state, IndexLoadState::Ready);

        match current_state {
            IndexLoadState::Loading(rx) => {
                match rx.try_recv() {
                    Ok(Ok(reader)) => {
                        // Index loaded successfully!
                        let doc_count = reader.meta.doc_count;
                        let msg = if self.root_path != self.start_path {
                            format!(
                                "{} files indexed (root: {})",
                                doc_count,
                                self.root_path.file_name().unwrap_or_default().to_string_lossy()
                            )
                        } else {
                            format!("{} files indexed", doc_count)
                        };
                        self.reader = Some(Arc::new(reader));
                        self.index_available = true;
                        self.status_message = msg;
                        self.load_state = IndexLoadState::Ready;

                        // Auto-execute pending search query if any
                        if !self.query.is_empty() {
                            self.execute_search();
                        }
                    }
                    Ok(Err(e)) => {
                        // Loading failed
                        self.status_message = format!("Index load failed: {}", e);
                        self.load_state = IndexLoadState::Failed;
                    }
                    Err(TryRecvError::Empty) => {
                        // Still loading, put the receiver back
                        self.load_state = IndexLoadState::Loading(rx);
                    }
                    Err(TryRecvError::Disconnected) => {
                        // Thread crashed?
                        self.status_message = "Index load thread terminated unexpectedly".to_string();
                        self.load_state = IndexLoadState::Failed;
                    }
                }
            }
            other => {
                // Put the state back if it wasn't Loading
                self.load_state = other;
            }
        }
    }

    /// Check if index is still loading
    pub fn is_loading(&self) -> bool {
        matches!(self.load_state, IndexLoadState::Loading(_))
    }

    /// Check if search is in progress
    pub fn is_searching(&self) -> bool {
        matches!(self.search_state, SearchState::Searching { .. })
    }

    /// Get search duration in ms (for display)
    pub fn search_duration_ms(&self) -> Option<u128> {
        match &self.search_state {
            SearchState::Searching { start_time, .. } => Some(start_time.elapsed().as_millis()),
            SearchState::Idle => None,
        }
    }

    /// Poll for background search completion (call this in event loop)
    pub fn poll_search(&mut self) {
        // Take ownership of the state temporarily
        let current_state = std::mem::replace(&mut self.search_state, SearchState::Idle);

        match current_state {
            SearchState::Searching { query, receiver, start_time } => {
                match receiver.try_recv() {
                    Ok(result) => {
                        // Search completed!
                        let elapsed = start_time.elapsed();

                        // Only apply results if query still matches (user might have typed more)
                        if result.query == self.query {
                            match result.matches {
                                Ok(matches) => {
                                    let count = matches.len();
                                    self.status_message = format!(
                                        "{} matches ({:.1}ms)",
                                        count,
                                        elapsed.as_secs_f64() * 1000.0
                                    );

                                    // Cache the results (LRU automatically evicts oldest)
                                    self.search_cache.put(result.query.clone(), matches.clone());

                                    self.results = matches;
                                    self.selected = 0;
                                    self.update_preview();

                                    // Prefetch adjacent previews in background
                                    self.prefetch_adjacent_previews();
                                }
                                Err(e) => {
                                    self.status_message = format!("Error: {}", e);
                                    self.results.clear();
                                }
                            }
                        }
                        // Search state is already Idle from the replace
                    }
                    Err(TryRecvError::Empty) => {
                        // Still searching, put the state back
                        self.search_state = SearchState::Searching { query, receiver, start_time };
                    }
                    Err(TryRecvError::Disconnected) => {
                        // Thread crashed?
                        self.status_message = "Search thread terminated unexpectedly".to_string();
                        // State is already Idle
                    }
                }
            }
            SearchState::Idle => {
                // Nothing to do
            }
        }
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.results.clear();
        self.selected = 0;
        self.editing = true;
    }

    pub fn execute_search(&mut self) {
        self.prefetch_cache.clear();

        if self.query.is_empty() {
            self.results.clear();
            self.search_state = SearchState::Idle;
            self.status_message = if self.index_available {
                if self.using_daemon {
                    "Connected to daemon".to_string()
                } else {
                    format!(
                        "{} files indexed",
                        self.reader.as_ref().map(|r| r.meta.doc_count).unwrap_or(0)
                    )
                }
            } else {
                "No index. Press F5 to build.".to_string()
            };
            return;
        }

        // Check local cache first for instant results (LRU cache)
        if let Some(cached) = self.search_cache.get(&self.query) {
            self.results = cached.clone();
            self.selected = 0;
            self.status_message = format!("{} matches (cached)", self.results.len());
            self.update_preview();
            self.prefetch_adjacent_previews();
            return;
        }

        // Clear stale results immediately when starting a new search
        // This prevents showing old results if the new search fails
        self.results.clear();
        self.selected = 0;

        // Use daemon if available (fast path)
        if self.using_daemon
            && let Some(ref client) = self.client
        {
            let client = Arc::clone(client);
            let (tx, rx) = mpsc::channel();
            let query = self.query.clone();
            let query_for_thread = query.clone();
            let root_path = self.root_path.clone();

            self.status_message = "Searching (daemon)...".to_string();
            self.search_state = SearchState::Searching {
                query: query.clone(),
                receiver: rx,
                start_time: Instant::now(),
            };

            thread::spawn(move || {
                let result = if let Ok(mut client) = client.lock() {
                    match client.search(&query_for_thread, &root_path, 0) {
                        Ok(sr) => Ok(sr.matches),
                        Err(e) => Err(e.to_string()),
                    }
                } else {
                    Err("Failed to lock client".to_string())
                };

                let _ = tx.send(SearchResult {
                    matches: result,
                    query: query_for_thread,
                });
            });
            return;
        }

        // Fallback to direct index search
        let reader = match &self.reader {
            Some(r) => Arc::clone(r),
            None => {
                self.status_message = "No index available".to_string();
                return;
            }
        };

        let parsed = parse_query(&self.query);

        if parsed.is_empty() {
            self.results.clear();
            return;
        }

        // Start background search
        let (tx, rx) = mpsc::channel();
        let query = self.query.clone();
        let query_for_thread = query.clone();

        self.status_message = "Searching...".to_string();
        self.search_state = SearchState::Searching {
            query: query.clone(),
            receiver: rx,
            start_time: Instant::now(),
        };

        thread::spawn(move || {
            let executor = QueryExecutor::new(&reader);
            let result = executor.execute(&parsed).map_err(|e| e.to_string());

            let _ = tx.send(SearchResult {
                matches: result,
                query: query_for_thread,
            });
        });
    }

    pub fn select_next(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1).min(self.results.len() - 1);
            self.update_preview();
        }
    }

    pub fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.update_preview();
        }
    }

    pub fn select_page_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 10).min(self.results.len() - 1);
            self.update_preview();
        }
    }

    pub fn select_page_up(&mut self) {
        self.selected = self.selected.saturating_sub(10);
        self.update_preview();
    }

    pub fn toggle_preview(&mut self) {
        self.mode = match self.mode {
            Mode::Search => Mode::Preview,
            Mode::Preview => Mode::Search,
            Mode::Help => Mode::Help, // Don't toggle preview in help mode
        };
        self.update_preview();
    }

    pub fn show_help(&mut self) {
        if self.mode != Mode::Help {
            self.previous_mode = self.mode;
            self.mode = Mode::Help;
        }
    }

    pub fn hide_help(&mut self) {
        if self.mode == Mode::Help {
            self.mode = self.previous_mode;
        }
    }

    pub fn update_preview(&mut self) {
        if let Some(result) = self.results.get(self.selected) {
            let full_path = self.root_path.join(&result.path);

            // Use prefetch cache if available, otherwise read from disk
            let content = self.get_preview_content(&full_path);

            if let Some(content) = content {
                self.preview_content = Some(content);
                self.preview_path = Some(full_path);
                // Scroll to show the match
                self.preview_scroll = result.line_number.saturating_sub(5) as usize;
            } else {
                self.preview_content = None;
                self.preview_path = None;
            }
        } else {
            self.preview_content = None;
            self.preview_path = None;
        }

        // Prefetch adjacent results for faster navigation
        self.prefetch_adjacent_previews();
    }

    pub fn scroll_preview_down(&mut self) {
        self.preview_scroll += 1;
    }

    pub fn scroll_preview_up(&mut self) {
        self.preview_scroll = self.preview_scroll.saturating_sub(1);
    }

    pub fn scroll_preview_page_down(&mut self) {
        self.preview_scroll += 20;
    }

    pub fn scroll_preview_page_up(&mut self) {
        self.preview_scroll = self.preview_scroll.saturating_sub(20);
    }

    pub fn open_selected(&mut self) {
        if let Some(result) = self.results.get(self.selected) {
            let full_path = self.root_path.join(&result.path);

            // Try to open in $EDITOR
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

            // Format line number for editors that support it
            let line_arg = format!("+{}", result.line_number);

            let _ = Command::new(&editor)
                .arg(&line_arg)
                .arg(&full_path)
                .status();
        }
    }

    pub fn reindex(&mut self) {
        self.status_message = "Building index...".to_string();
        // Clear caches on reindex
        self.search_cache.clear();
        self.prefetch_cache.clear();

        match build_index_with_progress(&self.root_path, true, true) {
            Ok(()) => {
                // Notify daemon to reload if we're using it
                if self.using_daemon {
                    if let Some(ref client) = self.client
                        && let Ok(mut client) = client.lock()
                    {
                        let _ = client.reload(&self.root_path);
                    }
                    self.status_message = "Index rebuilt (daemon notified)".to_string();

                    // Re-run query if any
                    if !self.query.is_empty() {
                        self.execute_search();
                    }
                } else {
                    // Reload reader directly
                    match IndexReader::open(&self.root_path) {
                        Ok(r) => {
                            let doc_count = r.meta.doc_count;
                            self.reader = Some(Arc::new(r));
                            self.index_available = true;
                            self.load_state = IndexLoadState::Ready;
                            self.status_message = format!("Index rebuilt: {} files", doc_count);

                            // Re-run query if any
                            if !self.query.is_empty() {
                                self.execute_search();
                            }
                        }
                        Err(e) => {
                            self.status_message = format!("Error loading index: {}", e);
                            self.load_state = IndexLoadState::Failed;
                        }
                    }
                }
            }
            Err(e) => {
                self.status_message = format!("Index build failed: {}", e);
                self.load_state = IndexLoadState::Failed;
            }
        }
    }

    pub fn get_selected_result(&self) -> Option<&SearchMatch> {
        self.results.get(self.selected)
    }

    // Vim-style navigation methods

    /// Jump to first result
    pub fn select_first(&mut self) {
        if !self.results.is_empty() {
            self.selected = 0;
            self.update_preview();
        }
    }

    /// Jump to last result
    pub fn select_last(&mut self) {
        if !self.results.is_empty() {
            self.selected = self.results.len() - 1;
            self.update_preview();
        }
    }

    /// Scroll preview to top (vim 'gg')
    pub fn scroll_preview_to_top(&mut self) {
        self.preview_scroll = 0;
    }

    /// Scroll preview to bottom (vim 'G')
    pub fn scroll_preview_to_bottom(&mut self) {
        if let Some(ref content) = self.preview_content {
            let line_count = content.lines().count();
            self.preview_scroll = line_count.saturating_sub(20);
        }
    }

    /// Scroll preview half-page down (vim Ctrl+d)
    pub fn scroll_preview_half_page_down(&mut self) {
        self.preview_scroll += 10;
    }

    /// Scroll preview half-page up (vim Ctrl+u)
    pub fn scroll_preview_half_page_up(&mut self) {
        self.preview_scroll = self.preview_scroll.saturating_sub(10);
    }

    /// Delete word backward from query (vim Ctrl+w)
    pub fn delete_word(&mut self) {
        // Remove trailing whitespace first
        while self.query.ends_with(' ') {
            self.query.pop();
        }
        // Remove word characters
        while !self.query.is_empty() && !self.query.ends_with(' ') {
            self.query.pop();
        }
    }

    /// Clear pending key state
    pub fn clear_pending_key(&mut self) {
        self.pending_key = None;
    }

    /// Prefetch preview content for adjacent results (next/prev)
    /// This runs in background to make navigation feel instant
    fn prefetch_adjacent_previews(&mut self) {
        let indices_to_prefetch: Vec<usize> = [
            self.selected.checked_sub(1),
            Some(self.selected),
            self.selected.checked_add(1),
            self.selected.checked_add(2),
        ]
        .into_iter()
        .flatten()
        .filter(|&i| i < self.results.len())
        .collect();

        for idx in indices_to_prefetch {
            if let Some(result) = self.results.get(idx) {
                let full_path = self.root_path.join(&result.path);
                // Use entry API to avoid redundant lookups
                if let std::collections::hash_map::Entry::Vacant(entry) = self.prefetch_cache.entry(full_path.clone()) {
                    // Read and cache synchronously for now (files are usually small)
                    // Could be made async for very large files
                    if let Ok(content) = std::fs::read_to_string(&full_path) {
                        // Only cache files under 1MB
                        if content.len() < 1024 * 1024 {
                            entry.insert(content);
                        }
                    }
                }
            }
        }

        // Limit cache size to prevent memory bloat
        while self.prefetch_cache.len() > 20 {
            if let Some(key) = self.prefetch_cache.keys().next().cloned() {
                self.prefetch_cache.remove(&key);
            }
        }
    }

    /// Get preview content - uses prefetch cache if available
    fn get_preview_content(&self, path: &PathBuf) -> Option<String> {
        // Check prefetch cache first
        if let Some(content) = self.prefetch_cache.get(path) {
            return Some(expand_tabs(content));
        }
        // Fall back to disk read
        std::fs::read_to_string(path).ok().map(|s| expand_tabs(&s))
    }
}

/// Expand tabs to spaces with a tab width of 4.
/// This ensures consistent rendering in the terminal where tab stops vary.
fn expand_tabs(s: &str) -> String {
    const TAB_WIDTH: usize = 4;

    let mut result = String::with_capacity(s.len());
    let mut column = 0;

    for c in s.chars() {
        match c {
            '\t' => {
                let spaces = TAB_WIDTH - (column % TAB_WIDTH);
                result.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\n' | '\r' => {
                result.push(c);
                column = 0;
            }
            _ => {
                result.push(c);
                column += 1;
            }
        }
    }

    result
}
