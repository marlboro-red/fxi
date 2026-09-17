use crate::index::build::build_index_with_progress;
use crate::index::reader::IndexReader;
use crate::index::types::SearchMatch;
use crate::query::{QueryExecutor, try_parse_query};
use crate::server::IndexClient;
use crate::utils::find_codebase_root;
use anyhow::Result;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
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
    /// Coalesce repeated submissions while one worker owns the client.
    search_queued: bool,
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
                    search_queued: false,
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
                let result = IndexReader::open(&root_for_thread).map_err(|e| e.to_string());
                let _ = tx.send(result);
            });

            (IndexLoadState::Loading(rx), "Loading index...".to_string())
        } else {
            (
                IndexLoadState::NotFound,
                "No index found. Press F5 to build index.".to_string(),
            )
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
            search_queued: false,
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
                                self.root_path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
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
                        self.status_message =
                            "Index load thread terminated unexpectedly".to_string();
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
            SearchState::Searching {
                query,
                receiver,
                start_time,
            } => {
                match receiver.try_recv() {
                    Ok(result) => {
                        // Search completed!
                        let elapsed = start_time.elapsed();

                        // Only apply results if query still matches (user might have typed more)
                        if result.query == self.query && !self.search_queued {
                            match result.matches {
                                Ok(matches) => {
                                    let count = matches.len();
                                    self.status_message = format!(
                                        "{} matches ({:.1}ms)",
                                        count,
                                        elapsed.as_secs_f64() * 1000.0
                                    );

                                    self.results = matches;
                                    self.selected = 0;
                                    self.update_preview();

                                    // Populate bounded adjacent previews.
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
                        self.search_state = SearchState::Searching {
                            query,
                            receiver,
                            start_time,
                        };
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
        if !self.is_searching() && self.search_queued {
            self.search_queued = false;
            self.execute_search();
        }
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
    }

    pub fn set_initial_query(&mut self, query: &str) {
        self.set_query(query);
        if !self.is_loading() {
            self.execute_search();
        }
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.results.clear();
        self.selected = 0;
        self.editing = true;
    }

    pub fn execute_search(&mut self) {
        if self.is_searching() {
            self.search_queued = true;
            self.status_message = "Waiting for current search; latest query queued...".to_string();
            return;
        }
        if self.is_loading() {
            return; // The latest query runs when loading/rebuilding completes.
        }
        self.search_queued = false;
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

        let parsed = match try_parse_query(&self.query) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.status_message = format!("Invalid query: {error}");
                return;
            }
        };

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
                    match client.search(&query_for_thread, Some(&root_path), 0) {
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

    fn refresh_preview(&mut self) {
        self.prefetch_cache.clear();
        self.update_preview();
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

            let mut command = match editor_command(&editor, &full_path, result.line_number) {
                Ok(command) => command,
                Err(error) => {
                    self.status_message = format!("Cannot launch editor: {error}");
                    return;
                }
            };

            // Temporarily restore terminal before launching editor
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture
            );

            let result = command.status();
            if let Err(error) = result {
                self.status_message = format!("Cannot launch editor: {error}");
            } else if let Ok(status) = result
                && !status.success()
            {
                self.status_message = format!("Editor exited with {status}");
            }

            // Restore TUI terminal state
            let _ = crossterm::terminal::enable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture
            );
            self.refresh_preview();
        }
    }

    pub fn reindex(&mut self) {
        self.status_message = "Building index...".to_string();
        // Clear caches on reindex
        self.prefetch_cache.clear();
        self.search_queued = true;

        if self.is_loading() {
            return;
        }
        let root = self.root_path.clone();
        let client = self.client.clone();
        let (tx, rx) = mpsc::channel();
        self.load_state = IndexLoadState::Loading(rx);
        thread::spawn(move || {
            let result = (|| -> Result<IndexReader> {
                {
                    let _lock = crate::utils::IndexLock::acquire(&root)?;
                    build_index_with_progress(&root, true, true)?;
                }
                if let Some(client) = client {
                    let mut client = client
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Failed to lock daemon client"))?;
                    let (success, message) = client.reload(Some(&root))?;
                    anyhow::ensure!(success, "Daemon reload failed: {message}");
                }
                IndexReader::open(&root)
            })()
            .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
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

    /// Keep bounded preview content for nearby results ready for navigation.
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
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    self.prefetch_cache.entry(full_path.clone())
                {
                    // Bound I/O and allocation even for very large source files.
                    if let Ok(content) = read_preview(&full_path) {
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
        read_preview(path).ok().map(|s| expand_tabs(&s))
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

/// Read a bounded UTF-8 preview; never allocate the entire large source file.
fn read_preview(path: &Path) -> std::io::Result<String> {
    const LIMIT: u64 = 1024 * 1024;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(LIMIT).read_to_end(&mut bytes)?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(error)
            if error.as_bytes().len() == LIMIT as usize
                && error.utf8_error().error_len().is_none() =>
        {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            Ok(String::from_utf8(bytes).expect("validated UTF-8 prefix"))
        }
        Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
    }
}

/// Parse EDITOR arguments without executing shell substitutions or operators.
fn editor_command(editor: &str, path: &Path, line: u32) -> Result<Command> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    let mut characters = editor.chars().peekable();
    while let Some(ch) = characters.next() {
        if escaped {
            word.push(ch);
            escaped = false;
            started = true;
        } else if ch == '\\'
            && quote != Some('\'')
            && characters
                .peek()
                .is_some_and(|next| next.is_whitespace() || matches!(next, '\\' | '\'' | '"'))
        {
            escaped = true;
            started = true;
        } else if Some(ch) == quote {
            quote = None;
        } else if quote.is_some() {
            word.push(ch);
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    anyhow::ensure!(
        quote.is_none() && !escaped,
        "Unclosed quote or escape in EDITOR"
    );
    if started {
        words.push(word);
    }
    let executable = words
        .first()
        .filter(|word| !word.is_empty())
        .ok_or_else(|| anyhow::anyhow!("EDITOR is empty"))?;
    let mut command = Command::new(executable);
    command.args(&words[1..]);
    let name = Path::new(executable)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    match name {
        "code" | "code-insiders" | "codium" | "cursor" => {
            command
                .arg("--goto")
                .arg(format!("{}:{}", path.display(), line));
        }
        "subl" | "hx" | "helix" => {
            command.arg(format!("{}:{}", path.display(), line));
        }
        "vi" | "vim" | "nvim" | "nano" | "emacs" | "emacsclient" => {
            command.arg(format!("+{line}")).arg(path);
        }
        _ => {
            command.arg(path);
        }
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App {
            root_path: PathBuf::from("."),
            start_path: PathBuf::from("."),
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
            status_message: String::new(),
            index_available: false,
            pending_key: None,
            editing: true,
            load_state: IndexLoadState::Ready,
            search_state: SearchState::Idle,
            search_queued: false,
            prefetch_cache: HashMap::new(),
        }
    }

    #[test]
    fn initial_query_runs_when_ready_and_waits_for_loading() {
        let mut ready = app();
        ready.set_initial_query("needle");
        assert_eq!(ready.status_message, "No index available");
        let (_tx, rx) = mpsc::channel();
        let mut loading = app();
        loading.load_state = IndexLoadState::Loading(rx);
        loading.set_initial_query("needle");
        assert!(loading.status_message.is_empty());
        assert_eq!(loading.query, "needle");
    }

    #[test]
    fn repeated_submissions_coalesce_without_replacing_running_receiver() {
        let mut app = app();
        let (tx, rx) = mpsc::channel();
        app.search_state = SearchState::Searching {
            query: "old".into(),
            receiver: rx,
            start_time: Instant::now(),
        };
        for query in ["one", "two", "latest"] {
            app.set_query(query);
            app.execute_search();
            assert!(app.is_searching());
        }
        assert!(app.search_queued);
        tx.send(SearchResult {
            query: "old".into(),
            matches: Ok(Vec::new()),
        })
        .unwrap();
        app.poll_search();
        assert!(!app.is_searching());
        assert!(!app.search_queued);
        assert_eq!(app.query, "latest");
        assert_eq!(app.status_message, "No index available");
    }

    #[test]
    fn invalid_query_keeps_previous_results_and_reports_error() {
        let mut app = app();
        app.results.push(SearchMatch {
            doc_id: 1,
            path: "old.rs".into(),
            line_number: 1,
            score: 1.0,
        });
        app.set_query("\"unterminated");
        app.execute_search();
        assert_eq!(app.results.len(), 1);
        assert!(app.status_message.starts_with("Invalid query:"));
        assert!(!app.is_searching());
    }

    #[test]
    fn editor_arguments_are_parsed_without_shell_execution() {
        let path = Path::new("/tmp/a b.rs");
        let command = editor_command("code --wait", path, 12).unwrap();
        assert_eq!(command.get_program(), "code");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--wait", "--goto", "/tmp/a b.rs:12"]
        );
        let command = editor_command("'/path with spaces/vim' -f", path, 12).unwrap();
        assert_eq!(command.get_program(), "/path with spaces/vim");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-f", "+12", "/tmp/a b.rs"]
        );
        let command = editor_command("editor '$(touch malicious)'", path, 1).unwrap();
        assert_eq!(command.get_args().next().unwrap(), "$(touch malicious)");
        let command =
            editor_command(r#""C:\Program Files\Editor\editor.exe" --wait"#, path, 1).unwrap();
        assert_eq!(command.get_program(), r"C:\Program Files\Editor\editor.exe");
        assert!(editor_command("'unclosed", path, 1).is_err());
        assert!(editor_command("", path, 1).is_err());
    }

    #[test]
    fn returning_from_editor_refreshes_cached_preview() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app();
        app.root_path = directory.path().to_path_buf();
        app.results.push(SearchMatch {
            doc_id: 1,
            path: "edit.rs".into(),
            line_number: 1,
            score: 1.0,
        });
        std::fs::write(directory.path().join("edit.rs"), "old contents").unwrap();
        app.update_preview();
        assert_eq!(app.preview_content.as_deref(), Some("old contents"));
        std::fs::write(directory.path().join("edit.rs"), "new contents").unwrap();
        app.refresh_preview();
        assert_eq!(app.preview_content.as_deref(), Some("new contents"));
    }

    #[test]
    fn preview_is_bounded_and_handles_split_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let mut text = "x".repeat(1024 * 1024 - 1);
        text.push_str("é remaining text");
        std::fs::write(&path, text).unwrap();
        let preview = read_preview(&path).unwrap();
        assert_eq!(preview.len(), 1024 * 1024 - 1);
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(read_preview(&path).is_err());
        std::fs::write(&path, [0xc3]).unwrap();
        assert!(read_preview(&path).is_err());
    }
}
