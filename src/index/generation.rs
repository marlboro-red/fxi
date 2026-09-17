//! Immutable index generations with one atomic publication point.
use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Generation {
    container: PathBuf,
    pub path: PathBuf,
    _lease: File,
    published: bool,
    /// Data already synced by a published generation. New hard-link directory
    /// entries still need syncing; unchanged file contents do not.
    inherited_durable: HashMap<PathBuf, fs::Metadata>,
}

impl Generation {
    pub fn new(root: &Path) -> Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let container = crate::utils::app_data::get_index_container(root)?;
        let generations = container.join("generations");
        fs::create_dir_all(&generations)?;
        let path = loop {
            let name = format!(
                "gen-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let candidate = generations.join(name);
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.join("lease"))?;
        FileExt::lock_shared(&lease)?;
        Ok(Self {
            container,
            path,
            _lease: lease,
            published: false,
            inherited_durable: HashMap::new(),
        })
    }

    /// Reuse immutable segment bytes; metadata is always written afresh.
    pub fn inherit_segments(&mut self, previous: &Path) -> Result<()> {
        fn link_tree(
            source: &Path,
            destination: &Path,
            durable: bool,
            inherited: &mut HashMap<PathBuf, fs::Metadata>,
        ) -> Result<()> {
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let target = destination.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    link_tree(&entry.path(), &target, durable, inherited)?;
                } else if entry.file_type()?.is_file() {
                    if fs::hard_link(entry.path(), &target).is_ok() {
                        if durable && cfg!(unix) {
                            inherited.insert(target.clone(), fs::metadata(&target)?);
                        }
                    } else {
                        // Copies contain newly written bytes and must be synced.
                        fs::copy(entry.path(), &target)?;
                    }
                }
            }
            Ok(())
        }
        let source = previous.join("segments");
        if source.exists() {
            // Legacy layouts do not establish this module's durable publication
            // contract. Keep the conservative sync for those imports.
            let durable = previous.parent() == Some(self.container.join("generations").as_path())
                && resolve(&self.container)? == previous;
            link_tree(
                &source,
                &self.path.join("segments"),
                durable,
                &mut self.inherited_durable,
            )?;
        }
        Ok(())
    }

    pub fn publish(&mut self) -> Result<()> {
        crate::index::negative_routing::write_if_requested(&self.path)?;
        // Publish only after every referenced byte and directory entry is durable.
        sync_tree(&self.path, &self.inherited_durable)?;
        sync_directory(&self.container.join("generations"))?;
        let pending = self.container.join("CURRENT.tmp");
        let mut file = File::create(&pending)?;
        writeln!(file, "{}", self.path.file_name().unwrap().to_str().unwrap())?;
        file.sync_all()?;
        drop(file);
        fs::rename(pending, self.container.join("CURRENT"))?;
        self.published = true;
        sync_directory(&self.container)?;
        // Readers pin their generation with a shared lease. Cleanup is best
        // effort (Windows can retain mapped files until their last handle closes).
        self.collect_unpinned();
        Ok(())
    }

    fn collect_unpinned(&self) {
        let Ok(entries) = fs::read_dir(self.container.join("generations")) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.path {
                continue;
            }
            let Ok(lease) = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join("lease"))
            else {
                continue;
            };
            if lease.try_lock_exclusive().is_ok() {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn unchanged_durable_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.is_file()
            && after.is_file()
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        let _ = (before, after);
        false
    }
}

fn sync_tree(path: &Path, inherited: &HashMap<PathBuf, fs::Metadata>) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path(), inherited)?;
        } else {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())?;
            if !inherited.get(&entry.path()).is_some_and(|before| {
                file.metadata()
                    .is_ok_and(|after| unchanged_durable_file(before, &after))
            }) {
                file.sync_all()?;
            }
        }
    }
    sync_directory(path)
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Resolve and pin the same generation before opening any of its files.
pub(crate) fn pin(root: &Path) -> Result<(PathBuf, Option<File>)> {
    for _ in 0..8 {
        let path = crate::utils::get_index_dir(root)?;
        if path == crate::utils::app_data::get_index_container(root)? {
            return Ok((path, None)); // Legacy layout, never garbage-collected.
        }
        let lease = match File::open(path.join("lease")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        FileExt::lock_shared(&lease)?;
        if path.join("meta.json").exists() {
            return Ok((path, Some(lease)));
        }
        // Publication/GC won the race before the lease was acquired; retry.
    }
    anyhow::bail!("Index generations changed repeatedly while opening; retry search")
}

pub(crate) fn resolve(container: &Path) -> Result<PathBuf> {
    match fs::read_to_string(container.join("CURRENT")) {
        Ok(name) => {
            let name = name.trim();
            anyhow::ensure!(
                name.starts_with("gen-")
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "Invalid index generation manifest"
            );
            Ok(container.join("generations").join(name))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(container.to_path_buf()),
        Err(e) => Err(e).context("Cannot read index generation manifest"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_published_hardlinks_are_eligible_for_sync_reuse() {
        let root = tempfile::tempdir().unwrap();
        let mut original = Generation::new(root.path()).unwrap();
        fs::create_dir_all(original.path.join("segments/seg_0000")).unwrap();
        fs::write(original.path.join("segments/seg_0000/data"), b"durable").unwrap();
        fs::write(original.path.join("meta.json"), b"{}").unwrap();
        original.publish().unwrap();
        let mut next = Generation::new(root.path()).unwrap();
        next.inherit_segments(&original.path).unwrap();
        let inherited = next.path.join("segments/seg_0000/data");
        assert_eq!(next.inherited_durable.len(), 1);
        assert!(unchanged_durable_file(
            &next.inherited_durable[&inherited],
            &fs::metadata(&inherited).unwrap(),
        ));
        fs::write(next.path.join("meta.json"), b"{}").unwrap();
        next.publish().unwrap();
        assert_eq!(fs::read(&inherited).unwrap(), b"durable");

        let legacy = tempfile::tempdir().unwrap();
        fs::create_dir_all(legacy.path().join("segments/seg_0000")).unwrap();
        fs::write(legacy.path().join("segments/seg_0000/data"), b"legacy").unwrap();
        let mut import = Generation::new(root.path()).unwrap();
        import.inherit_segments(legacy.path()).unwrap();
        assert!(import.inherited_durable.is_empty());
        drop(import);
        drop(next);
        drop(original);
        crate::utils::remove_index(root.path()).unwrap();
    }

    #[test]
    fn changed_or_replaced_inherited_bytes_require_sync_even_with_restored_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data");
        fs::write(&path, b"before").unwrap();
        File::open(&path).unwrap().sync_all().unwrap();
        let before = fs::metadata(&path).unwrap();
        // Ensure a distinct change timestamp even on coarser test filesystems.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, b"edited").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        assert!(!unchanged_durable_file(
            &before,
            &fs::metadata(&path).unwrap()
        ));
        let changed = fs::metadata(&path).unwrap();
        let replacement = temp.path().join("replacement");
        fs::write(&replacement, b"edited").unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(changed.modified().unwrap()))
            .unwrap();
        fs::rename(replacement, &path).unwrap();
        assert!(!unchanged_durable_file(
            &changed,
            &fs::metadata(path).unwrap()
        ));
    }
}
