//! Immutable index generations with one atomic publication point.
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Generation {
    container: PathBuf,
    pub path: PathBuf,
    _lease: File,
    published: bool,
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
        })
    }

    /// Reuse immutable segment bytes; metadata is always written afresh.
    pub fn inherit_segments(&self, previous: &Path) -> Result<()> {
        fn link_tree(source: &Path, destination: &Path) -> Result<()> {
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let target = destination.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    link_tree(&entry.path(), &target)?;
                } else if entry.file_type()?.is_file() {
                    fs::hard_link(entry.path(), &target)
                        .or_else(|_| fs::copy(entry.path(), &target).map(|_| ()))?;
                }
            }
            Ok(())
        }
        let source = previous.join("segments");
        if source.exists() {
            link_tree(&source, &self.path.join("segments"))?;
        }
        Ok(())
    }

    pub fn publish(&mut self) -> Result<()> {
        // Publish only after every referenced byte and directory entry is durable.
        sync_tree(&self.path)?;
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

fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())?
                .sync_all()?;
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
