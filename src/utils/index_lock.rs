//! Single-writer discipline for index mutations.
//!
//! Anything that writes to an index — full build, incremental update,
//! compaction, the daemon's delta flush — must hold this advisory lock so a
//! CLI `fxi index` can never interleave segment/meta writes with a watching
//! daemon's flush. Acquired at entry points only (CLI command handlers and
//! the daemon's flush/rebuild paths), never inside library functions, so
//! nested operations (an incremental update escalating to a full rebuild,
//! or triggering compaction) run under their caller's lock.

use crate::utils::app_data::get_index_dir;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File};
use std::path::Path;

/// Held for the duration of an index mutation; released on drop.
pub struct IndexLock {
    file: File,
}

impl IndexLock {
    /// Acquire the exclusive write lock for the index of `root`. Blocks if
    /// another writer (e.g. a daemon flush) holds it, with a note on stderr
    /// so a waiting CLI invocation doesn't look hung.
    ///
    /// The lock file lives BESIDE the index directory (`<index_dir>.lock`),
    /// not inside it: a forced rebuild deletes the index directory while
    /// holding the lock.
    pub fn acquire(root: &Path) -> Result<IndexLock> {
        let index_dir = get_index_dir(root)?;
        let lock_path = index_dir.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = File::create(&lock_path)
            .with_context(|| format!("Failed to create lock file {}", lock_path.display()))?;

        if file.try_lock_exclusive().is_err() {
            eprintln!(
                "fxi: waiting for another indexer to finish ({})",
                lock_path.display()
            );
            file.lock_exclusive()
                .with_context(|| format!("Failed to lock {}", lock_path.display()))?;
        }

        Ok(IndexLock { file })
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_excludes_second_writer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let _held = IndexLock::acquire(root).unwrap();

        // A second lock attempt on the same root must not succeed while the
        // first is held (probe with try_lock on the same path)
        let lock_path = get_index_dir(root).unwrap().with_extension("lock");
        let probe = File::create(&lock_path).unwrap();
        assert!(
            probe.try_lock_exclusive().is_err(),
            "second writer acquired the lock while the first held it"
        );
    }

    #[test]
    fn test_lock_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        drop(IndexLock::acquire(root).unwrap());

        let lock_path = get_index_dir(root).unwrap().with_extension("lock");
        let probe = File::create(&lock_path).unwrap();
        assert!(probe.try_lock_exclusive().is_ok());
    }
}
