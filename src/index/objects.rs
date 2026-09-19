//! Experimental immutable segment storage. A generation's metadata is the
//! complete reference manifest; generations, not mutable reference counters,
//! are the authority for reclamation. Publication runs under the writer lock.
use super::types::IndexMeta;
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn requested() -> bool {
    std::env::var_os("FXI_STABLE_SEGMENTS").is_some_and(|v| v == "1")
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.starts_with("gen-")
        && name.contains("-seg-")
        && name.len() < 160
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(crate) fn store(index: &Path) -> Result<PathBuf> {
    let generations = index.parent().context("Missing generation parent")?;
    ensure!(
        generations.file_name().is_some_and(|n| n == "generations"),
        "Stable objects require a generation layout"
    );
    Ok(generations
        .parent()
        .context("Missing index container")?
        .join("objects"))
}

/// Export only new segments. Existing objects must never be rewritten, linked,
/// renamed or have optional evidence added by a later generation.
pub(crate) fn prepare(index: &Path, inherited: &BTreeMap<u16, String>) -> Result<()> {
    let mut meta: IndexMeta = serde_json::from_slice(&fs::read(index.join("meta.json"))?)?;
    let objects = store(index)?;
    fs::create_dir_all(&objects)?;
    ensure!(
        fs::symlink_metadata(&objects)?.file_type().is_dir(),
        "Object store is not a directory"
    );
    let generation = index
        .file_name()
        .and_then(|v| v.to_str())
        .context("Invalid generation name")?;
    let mut references = BTreeMap::new();
    for id in meta
        .base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
    {
        let local = index.join("segments").join(format!("seg_{id:04}"));
        let name = if let Some(name) = inherited.get(&id) {
            ensure!(
                !local.exists(),
                "Segment has both local and stable representations"
            );
            name.clone()
        } else {
            let name = format!("{generation}-seg-{id:04}");
            let target = objects.join(&name);
            ensure!(!target.exists(), "Segment object already exists");
            ensure!(
                fs::symlink_metadata(&local)?.file_type().is_dir(),
                "New segment is not a directory"
            );
            // Complete and sync the new object's bytes before its name can be
            // referenced by CURRENT. Failure leaves an unreachable orphan.
            super::generation::sync_new_tree(&local)?;
            fs::rename(&local, &target)?;
            name
        };
        ensure!(valid_name(&name), "Invalid segment object name");
        ensure!(
            fs::symlink_metadata(objects.join(&name))?
                .file_type()
                .is_dir(),
            "Missing or non-directory segment object"
        );
        ensure!(
            references.insert(id, name).is_none(),
            "Duplicate segment ID"
        );
    }
    super::generation::sync_directory(&objects)?;
    super::generation::sync_directory(objects.parent().context("Missing object store parent")?)?;
    meta.version = match meta.profile {
        super::types::IndexProfile::Full => 4,
        super::types::IndexProfile::Lean => 5,
    };
    meta.segment_objects = references;
    meta.validate_format()?;
    super::writer::write_meta_atomic(index, &meta)?;
    let metadata = fs::read(index.join("meta.json"))?;
    fs::write(
        index.join("objects.check"),
        xxhash_rust::xxh3::xxh3_64(&metadata).to_le_bytes(),
    )?;
    Ok(())
}

/// GC must use the same references that an already-pinned reader saw, even if
/// meta.json later suffers a valid-shaped mutation. This is a corruption check,
/// not authentication against an actor rewriting both metadata and its digest.
pub(crate) fn validate_manifest_binding(
    index: &Path,
    meta: &IndexMeta,
    bytes: &[u8],
) -> Result<()> {
    let proof = index.join("objects.check");
    if meta.version >= 4 || proof.try_exists()? {
        ensure!(
            fs::symlink_metadata(&proof)?.file_type().is_file(),
            "Invalid object manifest proof"
        );
        ensure!(
            fs::read(proof)? == xxhash_rust::xxh3::xxh3_64(bytes).to_le_bytes(),
            "Object manifest changed after publication"
        );
    }
    Ok(())
}

/// Called only after old unpinned generations have been removed. A pinned,
/// incomplete or unreadable generation makes reclamation conservative. Never
/// infer an empty reference set from a parsing or directory enumeration error.
pub(crate) fn collect(container: &Path) -> Result<()> {
    let objects = container.join("objects");
    if !objects.try_exists()? {
        return Ok(());
    }
    ensure!(
        fs::symlink_metadata(&objects)?.file_type().is_dir(),
        "Invalid object store"
    );
    let mut live = HashSet::new();
    for entry in fs::read_dir(container.join("generations"))? {
        let path = entry?.path();
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_dir(),
            "Invalid generation directory"
        );
        let bytes = fs::read(path.join("meta.json"))?;
        let meta: IndexMeta = serde_json::from_slice(&bytes)?;
        meta.validate_format()?;
        validate_manifest_binding(&path, &meta, &bytes)?;
        for name in meta.segment_objects.into_values() {
            ensure!(
                fs::symlink_metadata(objects.join(&name))?
                    .file_type()
                    .is_dir(),
                "Dangling or non-directory object reference"
            );
            live.insert(name);
        }
    }
    // Validate the whole listing before deleting anything. Unknown files and
    // symlinks are preserved, including their targets.
    let mut garbage = Vec::new();
    for entry in fs::read_dir(&objects)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("Invalid object name")?;
        ensure!(
            valid_name(name) && entry.file_type()?.is_dir(),
            "Unrecognized object store entry"
        );
        if !live.contains(name) {
            garbage.push(entry.path());
        }
    }
    for path in garbage {
        fs::remove_dir_all(path)?;
    }
    super::generation::sync_directory(&objects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::generation::Generation;

    fn staged(root: &Path) -> Generation {
        let mut generation = Generation::new(root).unwrap();
        generation.stable_objects = true;
        let meta = IndexMeta {
            root_path: root.to_path_buf(),
            base_segment: Some(1),
            segment_count: 1,
            ..IndexMeta::default()
        };
        super::super::writer::write_meta_atomic(&generation.path, &meta).unwrap();
        fs::create_dir_all(generation.path.join("segments/seg_0001")).unwrap();
        fs::write(
            generation.path.join("segments/seg_0001/evidence"),
            b"immutable",
        )
        .unwrap();
        generation
    }

    #[test]
    fn failed_publication_orphans_are_reclaimed_only_after_pinned_generations_retire() {
        let root = tempfile::tempdir().unwrap();
        let mut current = staged(root.path());
        current.publish().unwrap();
        let container = current
            .path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let pointer = fs::read(container.join("CURRENT")).unwrap();
        let abandoned = staged(root.path());
        prepare(&abandoned.path, &BTreeMap::new()).unwrap();
        let meta: IndexMeta =
            serde_json::from_slice(&fs::read(abandoned.path.join("meta.json")).unwrap()).unwrap();
        let orphan = store(&abandoned.path)
            .unwrap()
            .join(&meta.segment_objects[&1]);
        // Equivalent on-disk state to failure after installing objects but before CURRENT.
        assert_eq!(fs::read(container.join("CURRENT")).unwrap(), pointer);
        collect(&container).unwrap();
        assert!(orphan.exists()); // The staged manifest is still a live reference.
        drop(abandoned);
        collect(&container).unwrap();
        assert!(!orphan.exists());
        assert_eq!(fs::read(container.join("CURRENT")).unwrap(), pointer);
        assert!(current.path.exists());
    }

    #[test]
    fn incomplete_or_corrupt_manifests_and_unknown_entries_prevent_object_collection() {
        let root = tempfile::tempdir().unwrap();
        let mut current = staged(root.path());
        current.publish().unwrap();
        let container = current.path.parent().unwrap().parent().unwrap();
        let orphan = store(&current.path).unwrap().join("gen-orphan-seg-0001");
        fs::create_dir(&orphan).unwrap();
        let incomplete = Generation::new(root.path()).unwrap();
        assert!(collect(container).is_err());
        assert!(orphan.exists());
        fs::write(incomplete.path.join("meta.json"), b"{").unwrap();
        assert!(collect(container).is_err());
        assert!(orphan.exists());
        drop(incomplete);
        let unknown = store(&current.path).unwrap().join("unrecognized");
        fs::write(&unknown, b"preserve").unwrap();
        assert!(collect(container).is_err());
        assert!(orphan.exists());
        fs::remove_file(unknown).unwrap();
        collect(container).unwrap();
        assert!(!orphan.exists());
    }

    #[test]
    fn changed_valid_shaped_pinned_manifest_cannot_release_original_objects() {
        let root = tempfile::tempdir().unwrap();
        let mut pinned = staged(root.path());
        pinned.publish().unwrap();
        let mut current = staged(root.path());
        current.publish().unwrap();
        let container = current.path.parent().unwrap().parent().unwrap();
        let bytes = fs::read(pinned.path.join("meta.json")).unwrap();
        let mut meta: IndexMeta = serde_json::from_slice(&bytes).unwrap();
        let original_object = store(&pinned.path).unwrap().join(&meta.segment_objects[&1]);
        let other: IndexMeta =
            serde_json::from_slice(&fs::read(current.path.join("meta.json")).unwrap()).unwrap();
        for replacement in [
            "gen-missing-seg-0001".to_owned(),
            other.segment_objects[&1].clone(),
        ] {
            meta.segment_objects.insert(1, replacement);
            super::super::writer::write_meta_atomic(&pinned.path, &meta).unwrap();
            assert!(collect(container).is_err());
            assert!(original_object.exists());
        }
        fs::write(pinned.path.join("meta.json"), &bytes).unwrap();
        collect(container).unwrap();
        assert!(original_object.exists());
        fs::remove_file(pinned.path.join("objects.check")).unwrap();
        assert!(collect(container).is_err());
        assert!(original_object.exists());
    }

    #[test]
    fn manifest_requires_exact_unique_coverage_and_safe_object_names() {
        let mut meta = IndexMeta {
            version: 4,
            base_segment: Some(1),
            segment_count: 1,
            ..IndexMeta::default()
        };
        assert!(meta.validate_format().is_err());
        meta.segment_objects.insert(1, "gen-test-seg-0001".into());
        meta.validate_format().unwrap();
        for name in [
            "../outside",
            "gen-foo/seg-0001",
            "gen-foo\\seg-0001",
            "/gen-foo-seg-0001",
            "C:gen-foo-seg-0001",
        ] {
            meta.segment_objects.insert(1, name.into());
            assert!(meta.validate_format().is_err());
        }
        meta.segment_objects.insert(1, "gen-test-seg-0001".into());
        meta.delta_segments.push(2);
        meta.segment_count = 2;
        meta.segment_objects.insert(2, "gen-test-seg-0001".into());
        assert!(meta.validate_format().is_err());
    }
}
