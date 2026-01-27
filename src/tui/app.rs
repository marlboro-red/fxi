use crate::index::build::build_index;
use crate::index::reader::IndexReader;
use crate::index::types::SearchMatch;
use crate::query::{parse_query, QueryExecutor};
use crate::utils::find_codebase_root;
use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

/// Application mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Preview,
    Help,
}

/// Application state
pub struct App {
    /// The codebase root (detected or specified)
    pub root_path: PathBuf,
    /// Original path user started from (for relative path display)
    #[allow(dead_code)]
    pub start_path: PathBuf,
    pub reader: Option<IndexReader>,
    pub query: String,
    pub results: Vec<SearchMatch>,
    pub selected: usize,
    pub mode: Mode,
    /// Previous mode before entering help (to return to)
    pub previous_mode: Mode,
    pub preview_scroll: usize,
    pub preview_content: Option<String>,
    pub status_message: String,
    pub index_available: bool,
    /// Pending key for vim multi-key commands (e.g., 'g' for 'gg')
    pub pending_key: Option<char>,
}

impl App {
    pub fn new(path: PathBuf) -> Result<Self> {
        let start_path = path.canonicalize().unwrap_or(path);

        // Auto-detect codebase root
        let root_path = find_codebase_root(&start_path)?;

        // Try to open existing index
        let (reader, index_available, status) = match IndexReader::open(&root_path) {
            Ok(r) => {
                let doc_count = r.meta.doc_count;
                let msg = if root_path != start_path {
                    format!(
                        "{} files indexed (root: {})",
                        doc_count,
                        root_path.file_name().unwrap_or_default().to_string_lossy()
                    )
                } else {
                    format!("{} files indexed", doc_count)
                };
                (Some(r), true, msg)
            }
            Err(_) => (
                None,
                false,
                "No index found. Press F5 to build index.".to_string(),
            ),
        };

        Ok(Self {
            root_path,
            start_path,
            reader,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            mode: Mode::Search,
            previous_mode: Mode::Search,
            preview_scroll: 0,
            preview_content: None,
            status_message: status,
            index_available,
            pending_key: None,
        })
    }

    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
        self.results.clear();
        self.selected = 0;
    }

    pub fn execute_search(&mut self) {
        if self.query.is_empty() {
            self.results.clear();
            self.status_message = if self.index_available {
                format!(
                    "{} files indexed",
                    self.reader.as_ref().map(|r| r.meta.doc_count).unwrap_or(0)
                )
            } else {
                "No index. Press F5 to build.".to_string()
            };
            return;
        }

        let reader = match &self.reader {
            Some(r) => r,
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

        let executor = QueryExecutor::new(reader);

        match executor.execute(&parsed) {
            Ok(matches) => {
                self.status_message = format!("{} matches", matches.len());
                self.results = matches;
                self.selected = 0;
                self.update_preview();
            }
            Err(e) => {
                self.status_message = format!("Error: {}", e);
                self.results.clear();
            }
        }
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
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                self.preview_content = Some(content);
                // Scroll to show the match
                self.preview_scroll = result.line_number.saturating_sub(5) as usize;
            } else {
                self.preview_content = None;
            }
        } else {
            self.preview_content = None;
        }
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

        match build_index(&self.root_path, true) {
            Ok(()) => {
                // Reload reader
                match IndexReader::open(&self.root_path) {
                    Ok(r) => {
                        let doc_count = r.meta.doc_count;
                        self.reader = Some(r);
                        self.index_available = true;
                        self.status_message = format!("Index rebuilt: {} files", doc_count);

                        // Re-run query if any
                        if !self.query.is_empty() {
                            self.execute_search();
                        }
                    }
                    Err(e) => {
                        self.status_message = format!("Error loading index: {}", e);
                    }
                }
            }
            Err(e) => {
                self.status_message = format!("Index build failed: {}", e);
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
        self.execute_search();
    }

    /// Clear pending key state
    pub fn clear_pending_key(&mut self) {
        self.pending_key = None;
    }
}
