//! Immutable index generations with one atomic publication point.
use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Generation {
    container: PathBuf,
    pub path: PathBuf,
    _lease: File,
    published: bool,
    pub(crate) stable_objects: bool,
    inherited_objects: std::collections::BTreeMap<u16, String>,
    /// Immutable hard links whose bytes were synced by a prior publication.
    /// Copies and legacy-layout files have no such durability proof.
    durable_links: HashSet<PathBuf>,
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
        #[cfg(test)]
        super::lifecycle_tests::checkpoint("generation_created");
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
            stable_objects: super::objects::requested(),
            inherited_objects: Default::default(),
            durable_links: HashSet::new(),
        })
    }

    /// Reuse immutable segment bytes; metadata is always written afresh.
    pub fn inherit_segments(&mut self, previous: &Path) -> Result<()> {
        // Test fixtures and legacy staging trees can omit real metadata.
        if let Ok(bytes) = fs::read(previous.join("meta.json"))
            && let Ok(meta) = serde_json::from_slice::<super::types::IndexMeta>(&bytes)
        {
            meta.validate_format()?;
            if meta.version >= 4 {
                super::objects::validate_manifest_binding(previous, &meta, &bytes)?;
                self.stable_objects = true;
                self.inherited_objects = meta.segment_objects;
                return Ok(());
            }
        }
        fn link_tree(
            source: &Path,
            destination: &Path,
            durable: bool,
            links: &mut HashSet<PathBuf>,
        ) -> Result<()> {
            fs::create_dir_all(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let target = destination.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    link_tree(&entry.path(), &target, durable, links)?;
                } else if entry.file_type()?.is_file() {
                    match fs::hard_link(entry.path(), &target) {
                        Ok(()) if durable => {
                            links.insert(target);
                        }
                        Ok(()) => {}
                        Err(_) => {
                            fs::copy(entry.path(), &target)?;
                        }
                    }
                }
            }
            Ok(())
        }
        let source = previous.join("segments");
        if source.exists() {
            let durable = previous.parent() == Some(self.container.join("generations").as_path())
                && resolve(&self.container)? == previous;
            link_tree(
                &source,
                &self.path.join("segments"),
                durable,
                &mut self.durable_links,
            )?;
        }
        Ok(())
    }

    pub fn publish(&mut self) -> Result<()> {
        if self.stable_objects {
            anyhow::ensure!(
                !crate::index::query_local::requested()
                    && !crate::index::negative_routing::requested()
                    && !crate::index::generation_routing::requested(),
                "Experimental stable segments currently require strict validation; disable checked/negative routing"
            );
            super::objects::prepare(&self.path, &self.inherited_objects)?;
        }
        if crate::index::query_local::requested() {
            crate::index::reader::write_query_local_checks(&self.path)?;
        }
        crate::index::negative_routing::write_if_requested(&self.path)?;
        // Publish only after every referenced byte and directory entry is durable.
        // Inherited hard links reuse already-durable bytes. Their new directory
        // entries still require syncing, as do every copied/new file and CURRENT.
        sync_tree(&self.path, &self.durable_links, &mut |path| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?
                .sync_all()?;
            Ok(())
        })?;
        sync_directory(&self.container.join("generations"))?;
        #[cfg(test)]
        super::lifecycle_tests::checkpoint("generation_synced");
        let pending = self.container.join("CURRENT.tmp");
        let mut file = File::create(&pending)?;
        writeln!(file, "{}", self.path.file_name().unwrap().to_str().unwrap())?;
        file.sync_all()?;
        drop(file);
        #[cfg(test)]
        super::lifecycle_tests::checkpoint("current_file_synced");
        fs::rename(pending, self.container.join("CURRENT"))?;
        self.published = true;
        #[cfg(test)]
        super::lifecycle_tests::checkpoint("current_renamed");
        sync_directory(&self.container)?;
        #[cfg(test)]
        super::lifecycle_tests::checkpoint("current_directory_synced");
        // Readers pin their generation with a shared lease. Cleanup is best
        // effort (Windows can retain mapped files until their last handle closes).
        let retirement_attempted = self.collect_unpinned();
        #[cfg(test)]
        super::lifecycle_tests::checkpoint("generations_retired");
        // Failure leaks reclaimable space rather than invalidating publication.
        let _ = self.collect_objects_after_retirement(retirement_attempted, sync_directory);
        Ok(())
    }

    fn collect_objects_after_retirement(
        &self,
        attempted: bool,
        sync: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        if attempted {
            sync(&self.container.join("generations"))?;
        }
        super::objects::collect(&self.container)
    }

    fn collect_unpinned(&self) -> bool {
        self.collect_unpinned_with(&mut |path| fs::remove_dir_all(path))
    }

    fn collect_unpinned_with(&self, remove: &mut impl FnMut(&Path) -> std::io::Result<()>) -> bool {
        let Ok(entries) = fs::read_dir(self.container.join("generations")) else {
            return false;
        };
        let mut retirement_attempted = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.path {
                continue;
            }
            let lease = match OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join("lease"))
            {
                Ok(lease) => lease,
                Err(error) => {
                    // The writer lock excludes another generation creator. A
                    // killed process can leave mkdir completed but no lease.
                    // Only remove an empty, recognized, real directory: a
                    // nonempty unleased tree may be damaged and is preserved.
                    if error.kind() == std::io::ErrorKind::NotFound
                        && entry.file_type().is_ok_and(|kind| kind.is_dir())
                        && entry.file_name().to_str().is_some_and(|name| {
                            name.starts_with("gen-")
                                && name.len() > 4
                                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        })
                        && fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none())
                    {
                        retirement_attempted = true;
                        let _ = fs::remove_dir(&path);
                    }
                    continue;
                }
            };
            if lease.try_lock_exclusive().is_ok() {
                // Even an error can follow partial deletion; that attempt must
                // be made durable before reclaiming any referenced objects.
                retirement_attempted = true;
                let _ = remove(&path);
            }
        }
        retirement_attempted
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn sync_tree(
    path: &Path,
    durable_links: &HashSet<PathBuf>,
    sync_file: &mut impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path(), durable_links, sync_file)?;
        } else if !durable_links.contains(&entry.path()) {
            sync_file(&entry.path())?;
        }
    }
    sync_directory(path)
}
pub(crate) fn sync_new_tree(path: &Path) -> Result<()> {
    sync_tree(path, &HashSet::new(), &mut |path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?
            .sync_all()?;
        Ok(())
    })
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_recovers_only_empty_recognized_unleased_generations() {
        let root = tempfile::tempdir().unwrap();
        let current = Generation::new(root.path()).unwrap();
        let generations = current.path.parent().unwrap();
        let empty = generations.join("gen-crashed");
        let damaged = generations.join("gen-damaged");
        let unknown = generations.join("unrecognized");
        for path in [&empty, &damaged, &unknown] {
            fs::create_dir(path).unwrap();
        }
        fs::write(damaged.join("keep"), b"evidence").unwrap();
        #[cfg(unix)]
        let linked = {
            let linked = generations.join("gen-symlink");
            std::os::unix::fs::symlink(&unknown, &linked).unwrap();
            linked
        };
        assert!(current.collect_unpinned());
        assert!(!empty.exists());
        assert!(damaged.join("keep").is_file());
        assert!(unknown.is_dir());
        #[cfg(unix)]
        assert!(linked.is_symlink());
    }

    #[test]
    fn retirement_reports_mutations_but_not_pinned_or_empty_scans() {
        let root = tempfile::tempdir().unwrap();
        let mut previous = Generation::new(root.path()).unwrap();
        fs::write(previous.path.join("meta.json"), b"{}").unwrap();
        previous.publish().unwrap();
        let mut current = Generation::new(root.path()).unwrap();
        fs::write(current.path.join("meta.json"), b"{}").unwrap();
        current.publish().unwrap();
        assert!(!current.collect_unpinned());
        let retired = previous.path.clone();
        drop(previous);
        assert!(
            current.collect_unpinned_with(&mut |_| Err(std::io::Error::other("partial deletion")))
        );
        assert!(retired.exists());
        assert!(current.collect_unpinned());
        assert!(!retired.exists());
        assert!(!current.collect_unpinned());
    }

    #[test]
    fn failed_retirement_sync_prevents_object_deletion() {
        let root = tempfile::tempdir().unwrap();
        let current = Generation::new(root.path()).unwrap();
        let meta = super::super::types::IndexMeta::default();
        fs::write(
            current.path.join("meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        let orphan = current.container.join("objects/gen-orphan-seg-0001");
        fs::create_dir_all(&orphan).unwrap();
        assert!(
            current
                .collect_objects_after_retirement(true, |_| anyhow::bail!("injected sync failure"))
                .is_err()
        );
        assert!(orphan.exists());
        current
            .collect_objects_after_retirement(true, |path| {
                assert!(path.ends_with("generations"));
                Ok(())
            })
            .unwrap();
        assert!(!orphan.exists());
        current
            .collect_objects_after_retirement(false, |_| panic!("no retirement to sync"))
            .unwrap();
    }

    #[test]
    fn publication_syncs_new_and_copied_bytes_but_reuses_durable_hardlinks() {
        let root = tempfile::tempdir().unwrap();
        let mut first = Generation::new(root.path()).unwrap();
        fs::create_dir_all(first.path.join("segments/seg_0001")).unwrap();
        fs::write(first.path.join("segments/seg_0001/evidence"), b"original").unwrap();
        fs::write(first.path.join("meta.json"), b"{}").unwrap();
        first.publish().unwrap();
        let mut next = Generation::new(root.path()).unwrap();
        next.inherit_segments(&first.path).unwrap();
        let inherited = next.path.join("segments/seg_0001/evidence");
        // A platform may decline hard linking; then the copy must be synced.
        let was_linked = next.durable_links.contains(&inherited);
        let copied = next.path.join("segments/seg_0001/copied");
        fs::copy(&inherited, &copied).unwrap();
        let metadata = next.path.join("meta.json");
        fs::write(&metadata, b"{}").unwrap();
        let mut synced = HashSet::new();
        sync_tree(&next.path, &next.durable_links, &mut |path| {
            synced.insert(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(synced.contains(&inherited), !was_linked);
        assert!(synced.contains(&copied));
        assert!(synced.contains(&metadata));
        assert!(synced.contains(&next.path.join("lease")));
        assert!(
            sync_tree(&next.path, &next.durable_links, &mut |_| anyhow::bail!(
                "injected sync failure"
            ))
            .is_err()
        );
        assert_eq!(resolve(&next.container).unwrap(), first.path);
        next.publish().unwrap();
        assert_eq!(resolve(&next.container).unwrap(), next.path);
        assert_eq!(fs::read(inherited).unwrap(), b"original");
        drop(next);
        drop(first);
        crate::utils::remove_index(root.path()).unwrap();
    }

    #[test]
    fn legacy_inheritance_does_not_assume_prior_durability() {
        let root = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        fs::create_dir_all(legacy.path().join("segments/seg_0001")).unwrap();
        fs::write(legacy.path().join("segments/seg_0001/evidence"), b"legacy").unwrap();
        let mut next = Generation::new(root.path()).unwrap();
        next.inherit_segments(legacy.path()).unwrap();
        assert!(next.durable_links.is_empty());
        let mut synced = HashSet::new();
        sync_tree(&next.path, &next.durable_links, &mut |path| {
            synced.insert(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert!(synced.contains(&next.path.join("segments/seg_0001/evidence")));
    }
}
