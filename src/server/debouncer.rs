//! Event debouncer for file system events
//!
//! Accumulates file system events within a configurable time window and produces
//! normalized change batches. This helps handle rapid changes like git operations
//! or IDE auto-save by collecting them into single batch updates.

use crate::server::watcher::{ChangeBatch, ChangeKind, FileChange, WatcherConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Tracks the state of a single file during debouncing
#[derive(Debug, Clone)]
struct FileState {
    /// Most recent change kind
    kind: ChangeKind,
    /// Time of the last change (kept for potential future use in age-based eviction)
    #[allow(dead_code)]
    last_change: Instant,
}

/// Debouncer that accumulates file changes within a time window
pub struct EventDebouncer {
    /// Configuration
    config: WatcherConfig,
    /// Pending changes by path
    pending: HashMap<PathBuf, FileState>,
    /// Time of the last event (any file)
    last_event: Option<Instant>,
}

impl EventDebouncer {
    /// Create a new debouncer with the given configuration
    pub fn new(config: WatcherConfig) -> Self {
        Self {
            config,
            pending: HashMap::new(),
            last_event: None,
        }
    }

    /// Add a file change event to the debouncer
    pub fn add_event(&mut self, path: PathBuf, kind: ChangeKind) {
        let now = Instant::now();
        self.last_event = Some(now);

        // Normalize the change based on existing state
        if let Some(existing) = self.pending.get(&path) {
            let new_kind = match (existing.kind, kind) {
                // Create + Modify = Create (content update during creation)
                (ChangeKind::Created, ChangeKind::Modified) => ChangeKind::Created,
                // Create + Delete = remove from pending (noop)
                (ChangeKind::Created, ChangeKind::Deleted) => {
                    self.pending.remove(&path);
                    return;
                }
                // Modify + Delete = Delete
                (ChangeKind::Modified, ChangeKind::Deleted) => ChangeKind::Deleted,
                // Delete + Create = Modified (file was replaced)
                (ChangeKind::Deleted, ChangeKind::Created) => ChangeKind::Modified,
                // Delete + Modify = shouldn't happen, but treat as Modified
                (ChangeKind::Deleted, ChangeKind::Modified) => ChangeKind::Modified,
                // Renamed is handled specially - treat as delete
                (_, ChangeKind::Renamed) => ChangeKind::Deleted,
                (ChangeKind::Renamed, _) => kind,
                // Same kind = keep kind
                (k, _) if k == kind => k,
                // Default: use new kind
                _ => kind,
            };
            self.pending.insert(
                path,
                FileState {
                    kind: new_kind,
                    last_change: now,
                },
            );
        } else {
            self.pending.insert(
                path,
                FileState {
                    kind,
                    last_change: now,
                },
            );
        }
    }

    /// Check if the debounce window has elapsed since the last event
    pub fn is_ready(&self) -> bool {
        if let Some(last) = self.last_event {
            last.elapsed() >= self.config.debounce_duration()
        } else {
            false
        }
    }

    /// Check if there are any pending changes
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Get the time until the next batch is ready (None if no pending events)
    #[allow(dead_code)]
    pub fn time_until_ready(&self) -> Option<Duration> {
        self.last_event.map(|last| {
            let elapsed = last.elapsed();
            let debounce = self.config.debounce_duration();
            if elapsed >= debounce {
                Duration::ZERO
            } else {
                debounce - elapsed
            }
        })
    }

    /// Flush all pending changes into a batch
    /// Returns None if no changes are pending
    pub fn flush(&mut self) -> Option<ChangeBatch> {
        if self.pending.is_empty() {
            return None;
        }

        let mut batch = ChangeBatch::new();

        for (path, state) in self.pending.drain() {
            batch.add(FileChange {
                path,
                kind: state.kind,
            });
        }

        self.last_event = None;

        if batch.is_empty() {
            None
        } else {
            Some(batch)
        }
    }

    /// Clear all pending events without producing a batch
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.pending.clear();
        self.last_event = None;
    }

    /// Get the number of pending file changes
    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn quick_config() -> WatcherConfig {
        WatcherConfig {
            debounce_ms: 50, // Short for testing
            ..Default::default()
        }
    }

    #[test]
    fn test_debouncer_single_event() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Modified);

        assert!(debouncer.has_pending());
        assert_eq!(debouncer.pending_count(), 1);

        // Wait for debounce
        sleep(Duration::from_millis(60));
        assert!(debouncer.is_ready());

        let batch = debouncer.flush().unwrap();
        assert_eq!(batch.modified.len(), 1);
        assert!(batch.created.is_empty());
        assert!(batch.deleted.is_empty());
    }

    #[test]
    fn test_debouncer_create_then_modify() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Created);
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Modified);

        sleep(Duration::from_millis(60));
        let batch = debouncer.flush().unwrap();

        // Should be just Created, not Modified
        assert_eq!(batch.created.len(), 1);
        assert!(batch.modified.is_empty());
    }

    #[test]
    fn test_debouncer_create_then_delete() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Created);
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Deleted);

        sleep(Duration::from_millis(60));
        // Create + Delete = noop
        let batch = debouncer.flush();
        assert!(batch.is_none() || batch.unwrap().is_empty());
    }

    #[test]
    fn test_debouncer_delete_then_create() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Deleted);
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Created);

        sleep(Duration::from_millis(60));
        let batch = debouncer.flush().unwrap();

        // Delete + Create = Modified (file replaced)
        assert_eq!(batch.modified.len(), 1);
        assert!(batch.created.is_empty());
        assert!(batch.deleted.is_empty());
    }

    #[test]
    fn test_debouncer_multiple_files() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("a.rs"), ChangeKind::Created);
        debouncer.add_event(PathBuf::from("b.rs"), ChangeKind::Modified);
        debouncer.add_event(PathBuf::from("c.rs"), ChangeKind::Deleted);

        sleep(Duration::from_millis(60));
        let batch = debouncer.flush().unwrap();

        assert_eq!(batch.created.len(), 1);
        assert_eq!(batch.modified.len(), 1);
        assert_eq!(batch.deleted.len(), 1);
    }

    #[test]
    fn test_debouncer_not_ready_immediately() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Modified);

        // Should not be ready immediately
        assert!(!debouncer.is_ready());
        assert!(debouncer.time_until_ready().unwrap() > Duration::ZERO);
    }

    #[test]
    fn test_debouncer_clear() {
        let mut debouncer = EventDebouncer::new(quick_config());
        debouncer.add_event(PathBuf::from("test.rs"), ChangeKind::Modified);
        assert!(debouncer.has_pending());

        debouncer.clear();
        assert!(!debouncer.has_pending());
        assert_eq!(debouncer.pending_count(), 0);
    }
}
