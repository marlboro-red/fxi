//! File system watcher for live index updates
//!
//! This module provides file watching capabilities that enable automatic index updates
//! when files change in watched directories. Changes are debounced and batched for
//! efficient processing.

use serde::Deserialize;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::utils::app_data::get_app_data_dir;

/// Default debounce window in milliseconds
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;

/// Default interval for flushing accumulated changes to delta segments (in seconds)
/// Set to 5 minutes to avoid creating too many delta segments during active editing
pub const DEFAULT_DELTA_FLUSH_INTERVAL_SECS: u64 = 300;

/// Default threshold for triggering segment merge (number of delta segments)
/// Higher values mean less frequent merging but more memory/segments during queries
/// Bloom filters and parallel query make the cost of extra segments low
pub const DEFAULT_MERGE_SEGMENT_THRESHOLD: usize = 15;

/// Default threshold for triggering full rebuild (percentage of docs changed)
pub const DEFAULT_REBUILD_THRESHOLD_PERCENT: usize = 30;

/// Kind of file change detected
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// File was created
    Created,
    /// File content was modified
    Modified,
    /// File was deleted
    Deleted,
    /// File was renamed (treated as delete + create)
    #[allow(dead_code)]
    Renamed,
}

/// A single file change event
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path to the changed file (relative to root)
    pub path: PathBuf,
    /// Kind of change
    pub kind: ChangeKind,
}

/// Accumulated batch of changes ready for processing
#[derive(Debug, Clone, Default)]
pub struct ChangeBatch {
    /// Files that were created
    pub created: Vec<PathBuf>,
    /// Files that were modified
    pub modified: Vec<PathBuf>,
    /// Files that were deleted
    pub deleted: Vec<PathBuf>,
}

impl ChangeBatch {
    /// Create a new empty change batch
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the batch is empty
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    /// Get total number of changes
    pub fn total_changes(&self) -> usize {
        self.created.len() + self.modified.len() + self.deleted.len()
    }

    /// Add a change to the batch
    pub fn add(&mut self, change: FileChange) {
        match change.kind {
            ChangeKind::Created => {
                if !self.created.contains(&change.path) {
                    self.created.push(change.path);
                }
            }
            ChangeKind::Modified => {
                // If file was just created, don't add to modified
                if !self.created.contains(&change.path) && !self.modified.contains(&change.path) {
                    self.modified.push(change.path);
                }
            }
            ChangeKind::Deleted => {
                // Remove from created/modified if present (create+delete = noop)
                self.created.retain(|p| p != &change.path);
                self.modified.retain(|p| p != &change.path);
                if !self.deleted.contains(&change.path) {
                    self.deleted.push(change.path);
                }
            }
            ChangeKind::Renamed => {
                // Treat as deleted (the new path will come as a separate created event)
                if !self.deleted.contains(&change.path) {
                    self.deleted.push(change.path);
                }
            }
        }
    }

    /// Merge another batch into this one
    #[allow(dead_code)]
    pub fn merge(&mut self, other: ChangeBatch) {
        for path in other.created {
            self.add(FileChange {
                path,
                kind: ChangeKind::Created,
            });
        }
        for path in other.modified {
            self.add(FileChange {
                path,
                kind: ChangeKind::Modified,
            });
        }
        for path in other.deleted {
            self.add(FileChange {
                path,
                kind: ChangeKind::Deleted,
            });
        }
    }

    /// Clear the batch
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.created.clear();
        self.modified.clear();
        self.deleted.clear();
    }
}

/// Configuration file format (TOML)
/// Located at ~/Library/Application Support/fxi/config.toml (macOS)
/// or %LOCALAPPDATA%/fxi/config.toml (Windows)
/// or ~/.local/share/fxi/config.toml (Linux)
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfigFile {
    /// Watcher-related configuration
    #[serde(default)]
    pub watcher: WatcherConfigFile,
}

/// Watcher section of the config file
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WatcherConfigFile {
    /// Debounce window in milliseconds
    pub debounce_ms: Option<u64>,
    /// Interval in seconds for flushing accumulated changes to delta segments
    pub delta_flush_interval_secs: Option<u64>,
    /// Number of delta segments that triggers a merge
    pub merge_segment_threshold: Option<usize>,
    /// Percentage of docs that triggers full rebuild
    pub rebuild_threshold_percent: Option<usize>,
}

/// Configuration for the file watcher
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Debounce window in milliseconds (changes within this window are batched)
    pub debounce_ms: u64,
    /// Interval in seconds for flushing accumulated changes to delta segments
    pub delta_flush_interval_secs: u64,
    /// Number of delta segments that triggers a merge
    pub merge_segment_threshold: usize,
    /// Percentage of docs that triggers full rebuild vs incremental update
    pub rebuild_threshold_percent: usize,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            delta_flush_interval_secs: DEFAULT_DELTA_FLUSH_INTERVAL_SECS,
            merge_segment_threshold: DEFAULT_MERGE_SEGMENT_THRESHOLD,
            rebuild_threshold_percent: DEFAULT_REBUILD_THRESHOLD_PERCENT,
        }
    }
}

impl WatcherConfig {
    /// Get debounce duration
    pub fn debounce_duration(&self) -> Duration {
        Duration::from_millis(self.debounce_ms)
    }

    /// Get delta flush interval duration
    pub fn delta_flush_duration(&self) -> Duration {
        Duration::from_secs(self.delta_flush_interval_secs)
    }

    /// Load config from file in the app data directory
    /// Returns None if file doesn't exist or can't be parsed
    fn load_from_file() -> Option<ConfigFile> {
        let app_dir = get_app_data_dir().ok()?;
        let config_path = app_dir.join("config.toml");

        if !config_path.exists() {
            return None;
        }

        let content = fs::read_to_string(&config_path).ok()?;
        toml::from_str(&content).ok()
    }

    /// Load config with priority: environment variables > config file > defaults
    pub fn load() -> Self {
        let mut config = Self::default();

        // First, apply config file values (if any)
        if let Some(file_config) = Self::load_from_file() {
            if let Some(v) = file_config.watcher.debounce_ms {
                config.debounce_ms = v;
            }
            if let Some(v) = file_config.watcher.delta_flush_interval_secs {
                config.delta_flush_interval_secs = v;
            }
            if let Some(v) = file_config.watcher.merge_segment_threshold {
                config.merge_segment_threshold = v;
            }
            if let Some(v) = file_config.watcher.rebuild_threshold_percent {
                config.rebuild_threshold_percent = v;
            }
        }

        // Then, apply environment variable overrides
        if let Ok(val) = std::env::var("FXI_DEBOUNCE_MS") {
            if let Ok(ms) = val.parse() {
                config.debounce_ms = ms;
            }
        }

        if let Ok(val) = std::env::var("FXI_DELTA_FLUSH_SECS") {
            if let Ok(secs) = val.parse() {
                config.delta_flush_interval_secs = secs;
            }
        }

        if let Ok(val) = std::env::var("FXI_MERGE_SEGMENTS") {
            if let Ok(count) = val.parse() {
                config.merge_segment_threshold = count;
            }
        }

        if let Ok(val) = std::env::var("FXI_REBUILD_THRESHOLD") {
            if let Ok(pct) = val.parse() {
                config.rebuild_threshold_percent = pct;
            }
        }

        config
    }

    /// Create config from environment variables (with fallbacks to defaults)
    /// Deprecated: use load() instead which also reads from config file
    pub fn from_env() -> Self {
        Self::load()
    }
}

/// Handle to a running watcher thread
pub struct WatcherHandle {
    /// Shutdown signal
    shutdown: Arc<AtomicBool>,
    /// Thread handle (wrapped in Option to allow taking on shutdown)
    thread: Option<JoinHandle<()>>,
    /// Root path being watched
    #[allow(dead_code)]
    pub root_path: PathBuf,
}

impl WatcherHandle {
    /// Create a new watcher handle
    pub fn new(shutdown: Arc<AtomicBool>, thread: JoinHandle<()>, root_path: PathBuf) -> Self {
        Self {
            shutdown,
            thread: Some(thread),
            root_path,
        }
    }

    /// Signal the watcher to stop
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // Give it a moment to notice the shutdown signal
            let _ = thread.join();
        }
    }

    /// Check if the watcher is still running
    pub fn is_running(&self) -> bool {
        !self.shutdown.load(Ordering::SeqCst)
            && self
                .thread
                .as_ref()
                .is_some_and(|t| !t.is_finished())
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Messages sent from watcher to IndexServer
#[derive(Debug)]
pub enum WatcherMessage {
    /// A batch of changes is ready to be processed
    ChangesReady {
        /// Root path of the index
        root_path: PathBuf,
        /// Batch of accumulated changes
        batch: ChangeBatch,
    },
    /// Request a full index rebuild (too many changes)
    #[allow(dead_code)]
    RequestRebuild {
        /// Root path of the index
        root_path: PathBuf,
        /// Reason for rebuild request
        reason: String,
    },
    /// Watcher encountered an error
    Error {
        /// Root path of the index
        root_path: PathBuf,
        /// Error message
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_change_batch_add_created() {
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("test.rs"),
            kind: ChangeKind::Created,
        });
        assert_eq!(batch.created.len(), 1);
        assert!(batch.modified.is_empty());
        assert!(batch.deleted.is_empty());
    }

    #[test]
    fn test_change_batch_create_then_modify() {
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("test.rs"),
            kind: ChangeKind::Created,
        });
        batch.add(FileChange {
            path: PathBuf::from("test.rs"),
            kind: ChangeKind::Modified,
        });
        // Modified should not be added if file was just created
        assert_eq!(batch.created.len(), 1);
        assert!(batch.modified.is_empty());
    }

    #[test]
    fn test_change_batch_create_then_delete() {
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("test.rs"),
            kind: ChangeKind::Created,
        });
        batch.add(FileChange {
            path: PathBuf::from("test.rs"),
            kind: ChangeKind::Deleted,
        });
        // Create + delete = noop, but file still in deleted list
        assert!(batch.created.is_empty());
        assert_eq!(batch.deleted.len(), 1);
    }

    #[test]
    fn test_change_batch_total_changes() {
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("a.rs"),
            kind: ChangeKind::Created,
        });
        batch.add(FileChange {
            path: PathBuf::from("b.rs"),
            kind: ChangeKind::Modified,
        });
        batch.add(FileChange {
            path: PathBuf::from("c.rs"),
            kind: ChangeKind::Deleted,
        });
        assert_eq!(batch.total_changes(), 3);
    }

    #[test]
    fn test_watcher_config_default() {
        let config = WatcherConfig::default();
        assert_eq!(config.debounce_ms, DEFAULT_DEBOUNCE_MS);
        assert_eq!(
            config.delta_flush_interval_secs,
            DEFAULT_DELTA_FLUSH_INTERVAL_SECS
        );
        assert_eq!(
            config.merge_segment_threshold,
            DEFAULT_MERGE_SEGMENT_THRESHOLD
        );
        assert_eq!(
            config.rebuild_threshold_percent,
            DEFAULT_REBUILD_THRESHOLD_PERCENT
        );
    }

    #[test]
    fn test_watcher_config_durations() {
        let config = WatcherConfig {
            debounce_ms: 1000,
            delta_flush_interval_secs: 60,
            merge_segment_threshold: 10,
            rebuild_threshold_percent: 25,
        };
        assert_eq!(config.debounce_duration(), Duration::from_millis(1000));
        assert_eq!(config.delta_flush_duration(), Duration::from_secs(60));
    }

    // Note: Environment variable tests are skipped because tests run in parallel
    // and modifying global env vars causes race conditions. The from_env() function
    // is simple enough that manual testing suffices:
    //   FXI_DEBOUNCE_MS=1000 FXI_DELTA_FLUSH_SECS=60 cargo run -- daemon foreground

    #[test]
    fn test_change_batch_merge() {
        let mut batch1 = ChangeBatch::new();
        batch1.add(FileChange {
            path: PathBuf::from("a.rs"),
            kind: ChangeKind::Created,
        });
        batch1.add(FileChange {
            path: PathBuf::from("b.rs"),
            kind: ChangeKind::Modified,
        });

        let mut batch2 = ChangeBatch::new();
        batch2.add(FileChange {
            path: PathBuf::from("c.rs"),
            kind: ChangeKind::Deleted,
        });
        batch2.add(FileChange {
            path: PathBuf::from("d.rs"),
            kind: ChangeKind::Created,
        });

        batch1.merge(batch2);
        assert_eq!(batch1.created.len(), 2); // a.rs, d.rs
        assert_eq!(batch1.modified.len(), 1); // b.rs
        assert_eq!(batch1.deleted.len(), 1); // c.rs
        assert_eq!(batch1.total_changes(), 4);
    }

    #[test]
    fn test_change_batch_merge_overlapping() {
        // Test merging batches with overlapping files
        let mut batch1 = ChangeBatch::new();
        batch1.add(FileChange {
            path: PathBuf::from("file.rs"),
            kind: ChangeKind::Created,
        });

        let mut batch2 = ChangeBatch::new();
        batch2.add(FileChange {
            path: PathBuf::from("file.rs"),
            kind: ChangeKind::Modified,
        });

        batch1.merge(batch2);
        // Created + Modified = still Created
        assert_eq!(batch1.created.len(), 1);
        assert!(batch1.modified.is_empty());
    }

    #[test]
    fn test_change_batch_merge_create_then_delete() {
        let mut batch1 = ChangeBatch::new();
        batch1.add(FileChange {
            path: PathBuf::from("temp.rs"),
            kind: ChangeKind::Created,
        });

        let mut batch2 = ChangeBatch::new();
        batch2.add(FileChange {
            path: PathBuf::from("temp.rs"),
            kind: ChangeKind::Deleted,
        });

        batch1.merge(batch2);
        // Created file then deleted = file in deleted list (tombstone needed if it existed before)
        assert!(batch1.created.is_empty());
        assert_eq!(batch1.deleted.len(), 1);
    }

    #[test]
    fn test_change_batch_is_empty() {
        let batch = ChangeBatch::new();
        assert!(batch.is_empty());

        let mut batch2 = ChangeBatch::new();
        batch2.add(FileChange {
            path: PathBuf::from("test.rs"),
            kind: ChangeKind::Modified,
        });
        assert!(!batch2.is_empty());
    }

    #[test]
    fn test_change_batch_clear() {
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("a.rs"),
            kind: ChangeKind::Created,
        });
        batch.add(FileChange {
            path: PathBuf::from("b.rs"),
            kind: ChangeKind::Modified,
        });

        batch.clear();
        assert!(batch.is_empty());
        assert_eq!(batch.total_changes(), 0);
    }

    #[test]
    fn test_change_batch_delete_then_create() {
        // Note: ChangeBatch::add does NOT normalize Delete+Create → Modified.
        // That normalization happens in EventDebouncer, not in ChangeBatch.
        // ChangeBatch just accumulates the changes as-is (with deduplication).
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("config.json"),
            kind: ChangeKind::Deleted,
        });
        batch.add(FileChange {
            path: PathBuf::from("config.json"),
            kind: ChangeKind::Created,
        });

        // Both deleted and created contain the file
        // (the debouncer normalizes this to Modified before creating the batch)
        assert_eq!(batch.deleted.len(), 1);
        assert_eq!(batch.created.len(), 1);
    }

    #[test]
    fn test_change_batch_multiple_modifications() {
        // Multiple modifications to same file should only appear once
        let mut batch = ChangeBatch::new();
        batch.add(FileChange {
            path: PathBuf::from("file.rs"),
            kind: ChangeKind::Modified,
        });
        batch.add(FileChange {
            path: PathBuf::from("file.rs"),
            kind: ChangeKind::Modified,
        });
        batch.add(FileChange {
            path: PathBuf::from("file.rs"),
            kind: ChangeKind::Modified,
        });

        assert_eq!(batch.modified.len(), 1);
    }

    #[test]
    fn test_config_file_parse_full() {
        let toml_content = r#"
[watcher]
debounce_ms = 1000
delta_flush_interval_secs = 60
merge_segment_threshold = 10
rebuild_threshold_percent = 25
"#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(config.watcher.debounce_ms, Some(1000));
        assert_eq!(config.watcher.delta_flush_interval_secs, Some(60));
        assert_eq!(config.watcher.merge_segment_threshold, Some(10));
        assert_eq!(config.watcher.rebuild_threshold_percent, Some(25));
    }

    #[test]
    fn test_config_file_parse_partial() {
        let toml_content = r#"
[watcher]
merge_segment_threshold = 20
"#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(config.watcher.debounce_ms, None);
        assert_eq!(config.watcher.delta_flush_interval_secs, None);
        assert_eq!(config.watcher.merge_segment_threshold, Some(20));
        assert_eq!(config.watcher.rebuild_threshold_percent, None);
    }

    #[test]
    fn test_config_file_parse_empty() {
        let toml_content = "";

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(config.watcher.debounce_ms, None);
        assert_eq!(config.watcher.merge_segment_threshold, None);
    }

    #[test]
    fn test_config_file_parse_no_watcher_section() {
        let toml_content = r#"
[other_section]
foo = "bar"
"#;

        let config: ConfigFile = toml::from_str(toml_content).unwrap();
        assert_eq!(config.watcher.debounce_ms, None);
        assert_eq!(config.watcher.merge_segment_threshold, None);
    }
}
