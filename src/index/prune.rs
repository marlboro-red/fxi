//! Conservative cleanup of registrations whose source roots have disappeared.
//!
//! This uses the writer lock and every generation lease, rather than asking a
//! daemon to drop readers. Legacy readers have no lease and cannot be excluded;
//! their cleanup requires explicit acknowledgment that all readers are stopped.
use super::types::{IndexMeta, IndexProfile};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

#[derive(Default)]
struct Report {
    scanned: usize,
    eligible: usize,
    bytes: u64,
    removed: usize,
    locks: usize,
    skipped: BTreeMap<&'static str, usize>,
    errors: usize,
}

enum Outcome {
    Candidate {
        bytes: u64,
        removed: bool,
        root: PathBuf,
    },
    Skipped(&'static str),
    LockFile,
}

#[derive(Debug)]
struct RemovalError(std::io::Error);
impl std::fmt::Display for RemovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Cannot remove abandoned index container: {}",
            self.0
        )
    }
}
impl std::error::Error for RemovalError {}

struct Registration {
    root: PathBuf,
    current: Option<Vec<u8>>,
    metadata_path: PathBuf,
    metadata: Vec<u8>,
    legacy_metadata: Option<Vec<u8>>,
}

impl Registration {
    fn has_legacy(&self) -> bool {
        self.current.is_none() || self.legacy_metadata.is_some()
    }
}

#[derive(Default)]
struct LegacyPolicy {
    include: bool,
    loaded_roots: HashSet<PathBuf>,
    status_unknown: bool,
}

impl LegacyPolicy {
    fn acknowledged_offline() -> Self {
        let mut policy = Self {
            include: true,
            ..Self::default()
        };
        if let Some(mut client) = crate::server::IndexClient::connect() {
            match client.status() {
                Ok(status) => policy.loaded_roots.extend(status.loaded_roots),
                Err(_) => policy.status_unknown = true,
            }
        }
        policy
    }

    fn skip(&self, registration: &Registration) -> Option<&'static str> {
        if !registration.has_legacy() {
            None
        } else if !self.include {
            Some("legacy layout has no reader leases")
        } else if self.status_unknown {
            Some("daemon status unavailable for legacy cleanup")
        } else if self.loaded_roots.contains(&registration.root) {
            Some("legacy root is loaded by the daemon")
        } else {
            None
        }
    }
}

/// Prune all eligible registrations, continuing after individual failures.
/// Invalid/unrecognized data is preserved. Filesystem errors make the final
/// command fail after printing the completed scan's summary.
pub fn run(dry_run: bool, verbose: bool, include_legacy: bool) -> Result<()> {
    let path = crate::utils::app_data::indexes_path()?;
    let legacy = if include_legacy {
        println!(
            "Legacy cleanup enabled: assumes all fxi readers/indexers are stopped; old readers have no leases."
        );
        LegacyPolicy::acknowledged_offline()
    } else {
        LegacyPolicy::default()
    };
    let report = scan(&path, dry_run, verbose, &legacy)?;
    println!(
        "{}: {} eligible indexes, {} logical bytes; {} removed; {} skipped; {} errors ({} entries scanned).",
        if dry_run { "Dry run" } else { "Prune" },
        report.eligible,
        report.bytes,
        report.removed,
        report.skipped.values().sum::<usize>(),
        report.errors,
        report.scanned,
    );
    for (reason, count) in &report.skipped {
        println!("  Preserved {count}: {reason}");
    }
    if report.locks != 0 {
        println!("  Preserved {} writer lock files.", report.locks);
    }
    if report
        .skipped
        .contains_key("busy writer or generation reader")
    {
        println!(
            "Busy indexes were not changed. Stop the daemon or wait for searches/indexers, then retry."
        );
    }
    if dry_run {
        println!(
            "Preview only; run `fxi prune{}` to apply. Eligibility is checked again at deletion.",
            if include_legacy {
                " --include-legacy"
            } else {
                ""
            }
        );
    }
    ensure!(
        report.errors == 0,
        "Prune completed with {} filesystem errors; use --verbose for all entries",
        report.errors
    );
    Ok(())
}

fn scan(indexes: &Path, dry_run: bool, verbose: bool, legacy: &LegacyPolicy) -> Result<Report> {
    let mut report = Report::default();
    match fs::symlink_metadata(indexes) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(report),
        result => ensure!(
            result?.file_type().is_dir(),
            "Index storage is not a regular directory: {}",
            indexes.display()
        ),
    }
    for entry in fs::read_dir(indexes)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.errors += 1;
                if verbose || report.errors <= 8 {
                    eprintln!("fxi prune: cannot read storage entry: {error}");
                }
                continue;
            }
        };
        report.scanned += 1;
        let path = entry.path();
        match prune_entry(&path, dry_run, legacy) {
            Ok(Outcome::Candidate {
                bytes,
                removed,
                root,
            }) => {
                report.eligible += 1;
                report.bytes = report.bytes.saturating_add(bytes);
                report.removed += usize::from(removed);
                if verbose {
                    println!(
                        "{} {path:?} (source {root:?}): {bytes} logical bytes",
                        if removed { "Removed" } else { "Eligible" }
                    );
                }
            }
            Ok(Outcome::LockFile) => report.locks += 1,
            Ok(Outcome::Skipped(reason)) => {
                *report.skipped.entry(reason).or_default() += 1;
                if verbose {
                    println!("Preserved {path:?}: {reason}");
                }
            }
            Err(error) => {
                // Format/layout failures are preservation decisions. Permission,
                // traversal, locking and deletion failures are operational errors.
                let filesystem = error.downcast_ref::<std::io::Error>();
                if error.downcast_ref::<RemovalError>().is_none()
                    && filesystem.is_none_or(|e| e.kind() == ErrorKind::NotFound)
                {
                    *report
                        .skipped
                        .entry("malformed, incomplete or unrecognized index")
                        .or_default() += 1;
                    if verbose {
                        println!("Preserved {path:?}: {error:#}");
                    }
                } else {
                    report.errors += 1;
                    if verbose || report.errors <= 8 {
                        eprintln!("fxi prune: preserved or partially removed {path:?}: {error:#}");
                    }
                }
            }
        }
    }
    Ok(report)
}

fn regular(path: &Path, directory: bool) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Unexpected file type or symlink: {}",
        path.display()
    );
    Ok(metadata)
}

fn generation_name(name: &str) -> bool {
    name.starts_with("gen-")
        && name.len() > 4
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn registration(container: &Path) -> Result<Registration> {
    regular(container, true)?;
    let current_path = container.join("CURRENT");
    let (current, metadata_path) = match fs::symlink_metadata(&current_path) {
        Err(error) if error.kind() == ErrorKind::NotFound => (None, container.join("meta.json")),
        result => {
            ensure!(result?.is_file(), "CURRENT is not a regular file");
            let bytes = fs::read(current_path)?;
            let name = std::str::from_utf8(&bytes)?.trim();
            ensure!(generation_name(name), "Invalid CURRENT manifest");
            let generations = container.join("generations");
            regular(&generations, true)?;
            let generation = generations.join(name);
            regular(&generation, true)?;
            (Some(bytes), generation.join("meta.json"))
        }
    };
    regular(&metadata_path, false)?;
    let metadata = fs::read(&metadata_path)?;
    let meta: IndexMeta = serde_json::from_slice(&metadata)?;
    meta.validate_format()?;
    ensure!(
        meta.root_path.is_absolute()
            && !meta
                .root_path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir)),
        "Recorded root is not an absolute normalized path"
    );
    let expected = crate::utils::app_data::recorded_container_name(&meta.root_path)?;
    ensure!(
        container.file_name() == Some(std::ffi::OsStr::new(&expected)),
        "Container identity does not match its recorded root"
    );
    let legacy_metadata = if current.is_some() {
        let path = container.join("meta.json");
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            result => {
                ensure!(result?.is_file(), "Legacy metadata is not a regular file");
                Some(fs::read(path)?)
            }
        }
    } else {
        None
    };
    Ok(Registration {
        root: meta.root_path,
        current,
        metadata_path,
        metadata,
        legacy_metadata,
    })
}

/// Do not collapse permission errors, non-directory ancestors, or dangling
/// symlinks into absence. Canonical recorded roots should contain no symlinks;
/// if an ancestor has since become one, preserve the registration too.
fn missing_root(root: &Path) -> Result<bool> {
    match fs::symlink_metadata(root) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot inspect recorded source root"),
    }
    let mut prefix = PathBuf::new();
    for component in root.components() {
        prefix.push(component.as_os_str());
        // A Windows drive/UNC prefix is not a filesystem path until its
        // following root separator is appended (notably canonical \\?\ paths).
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(false),
            Ok(metadata) if !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    ErrorKind::NotADirectory,
                    "Recorded source ancestor is not a directory",
                )
                .into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error).context("Cannot inspect source root ancestor"),
        }
    }
    Ok(false) // The source reappeared during the check.
}

fn try_exclusive(file: &File) -> Result<bool> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn writer_lock(container: &Path, dry_run: bool) -> Result<Option<File>> {
    let path = container.with_extension("lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "Writer lock is not a regular file"),
        Err(error) if error.kind() == ErrorKind::NotFound && dry_run => return Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Never truncate or remove this stable lock inode. Existing writers and
    // their waiters must keep sharing the same lock after pruning.
    Ok(Some(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(!dry_run)
            .truncate(false)
            .open(path)?,
    ))
}

fn unchanged(container: &Path, original: &Registration) -> Result<bool> {
    let current = registration(container)?;
    Ok(current.current == original.current
        && current.metadata == original.metadata
        && current.legacy_metadata == original.legacy_metadata
        && current.metadata_path == original.metadata_path
        && missing_root(&current.root)?)
}

fn prune_entry(container: &Path, dry_run: bool, legacy: &LegacyPolicy) -> Result<Outcome> {
    let metadata = fs::symlink_metadata(container)?;
    if metadata.is_file()
        && container
            .extension()
            .is_some_and(|extension| extension == "lock")
    {
        return Ok(Outcome::LockFile);
    }
    if !metadata.is_dir() {
        return Ok(Outcome::Skipped("non-directory or symlink entry"));
    }
    let original = registration(container)?;
    if !missing_root(&original.root)? {
        return Ok(Outcome::Skipped(
            "source exists or resolves through a symlink",
        ));
    }
    if let Some(reason) = legacy.skip(&original) {
        return Ok(Outcome::Skipped(reason));
    }
    let writer = writer_lock(container, dry_run)?;
    if let Some(writer) = &writer
        && !try_exclusive(writer)?
    {
        return Ok(Outcome::Skipped("busy writer or generation reader"));
    }
    prune_locked(container, &original, dry_run, legacy)
}

fn prune_locked(
    container: &Path,
    original: &Registration,
    dry_run: bool,
    legacy: &LegacyPolicy,
) -> Result<Outcome> {
    if !unchanged(container, original)? {
        return Ok(Outcome::Skipped(
            "source or registration changed during scan",
        ));
    }
    if let Some(reason) = legacy.skip(original) {
        return Ok(Outcome::Skipped(reason));
    }
    let has_legacy = original.has_legacy();
    let mut bytes = 0u64;
    let mut has_generations = false;
    for entry in fs::read_dir(container)? {
        let entry = entry?;
        match entry.file_name().to_str() {
            Some("CURRENT") => bytes = bytes.saturating_add(regular(&entry.path(), false)?.len()),
            Some("objects") => {
                regular(&entry.path(), true)?;
            }
            Some("generations") => {
                regular(&entry.path(), true)?;
                has_generations = true;
            }
            // validate_table below checks every legacy file, reference and byte
            // count; unrelated root entries are never authorized by the flag.
            _ if has_legacy => {}
            _ => anyhow::bail!("Unrecognized container entry: {:?}", entry.file_name()),
        }
    }
    let mut leases = Vec::new();
    let mut generations = Vec::new();
    let entries = if has_generations {
        Some(fs::read_dir(container.join("generations"))?)
    } else {
        None
    };
    for entry in entries.into_iter().flatten() {
        let entry = entry?;
        ensure!(
            entry.file_name().to_str().is_some_and(generation_name),
            "Unrecognized generation directory"
        );
        regular(&entry.path(), true)?;
        let lease_path = entry.path().join("lease");
        regular(&lease_path, false)?;
        let lease = OpenOptions::new().read(true).write(true).open(lease_path)?;
        if !try_exclusive(&lease)? {
            return Ok(Outcome::Skipped("busy writer or generation reader"));
        }
        leases.push(lease);
        generations.push(entry.path());
    }
    ensure!(
        original.current.is_none() || !generations.is_empty(),
        "Missing published generation"
    );
    for generation in &generations {
        bytes = bytes.saturating_add(validate_table(generation, &original.root, false)?);
    }
    if has_legacy {
        bytes = bytes.saturating_add(validate_table(container, &original.root, true)?);
    }
    if container.join("objects").try_exists()? {
        for entry in fs::read_dir(container.join("objects"))? {
            let entry = entry?;
            ensure!(
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(super::objects::valid_name),
                "Unknown segment object"
            );
            bytes = bytes.saturating_add(validate_segment(&entry.path(), None)?);
        }
    }
    // Locks are still held. Recheck the required evidence and source immediately
    // before deletion; a dry-run never creates either storage or lock files.
    if !unchanged(container, original)? {
        return Ok(Outcome::Skipped(
            "source or registration changed during scan",
        ));
    }
    if !dry_run {
        fs::remove_dir_all(container).map_err(RemovalError)?;
    }
    drop(leases);
    Ok(Outcome::Candidate {
        bytes,
        removed: !dry_run,
        root: original.root.clone(),
    })
}

fn validate_table(path: &Path, root: &Path, legacy: bool) -> Result<u64> {
    let files = [
        "meta.json",
        "docs.bin",
        "paths.bin",
        "lease",
        "query-routing.bin",
        "negative-routing.bin",
        "generation-routing.bin",
        "objects.check",
    ];
    let mut bytes = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if legacy
            && (entry.file_name() == "CURRENT"
                || entry.file_name() == "generations"
                || entry.file_name() == "objects")
        {
            // Container structure and CURRENT bytes were checked by the caller.
            continue;
        }
        if entry.file_name() == "segments" {
            regular(&entry.path(), true)?;
        } else {
            ensure!(
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| files.contains(&name)),
                "Unrecognized generation entry: {:?}",
                entry.file_name()
            );
            bytes = bytes.saturating_add(regular(&entry.path(), false)?.len());
        }
    }
    for name in ["meta.json", "docs.bin", "paths.bin"] {
        regular(&path.join(name), false)?;
    }
    if !legacy {
        regular(&path.join("lease"), false)?;
    }
    let meta: IndexMeta = serde_json::from_slice(&fs::read(path.join("meta.json"))?)?;
    meta.validate_format()?;
    super::objects::validate_manifest_binding(path, &meta, &fs::read(path.join("meta.json"))?)?;
    ensure!(
        meta.root_path == root,
        "Generation root differs from registration"
    );
    let ids: HashSet<_> = meta
        .base_segment
        .into_iter()
        .chain(meta.delta_segments.iter().copied())
        .collect();
    ensure!(
        ids.len() == usize::from(meta.base_segment.is_some()) + meta.delta_segments.len()
            && ids.len() == usize::from(meta.segment_count),
        "Invalid segment identity/count"
    );
    let documents = super::reader::read_documents(path)?;
    let paths = super::reader::read_paths(path)?;
    ensure!(
        documents.len() == meta.doc_count as usize
            && documents
                .iter()
                .all(|document| (document.path_id as usize) < paths.len()
                    && ids.contains(&document.segment_id)),
        "Invalid document references/count"
    );
    if meta.version >= 4 {
        for &id in &ids {
            validate_segment(&meta.segment_path(path, id)?, Some(&meta))?;
        }
        if path.join("segments").try_exists()? {
            regular(&path.join("segments"), true)?;
            ensure!(
                fs::read_dir(path.join("segments"))?.next().is_none(),
                "Stable generation has local segments"
            );
        }
        return Ok(bytes);
    }
    let mut expected: HashSet<_> = ids.iter().map(|id| format!("seg_{id:04}")).collect();
    regular(&path.join("segments"), true)?;
    for entry in fs::read_dir(path.join("segments"))? {
        let entry = entry?;
        ensure!(
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| expected.remove(name)),
            "Unrecognized segment directory"
        );
        bytes = bytes.saturating_add(validate_segment(&entry.path(), Some(&meta))?);
    }
    ensure!(expected.is_empty(), "Missing segment directory");
    Ok(bytes)
}

fn validate_segment(path: &Path, meta: Option<&IndexMeta>) -> Result<u64> {
    regular(path, true)?;
    let segment_files = [
        "grams.dict",
        "grams.postings",
        "tokens.dict",
        "tokens.postings",
        "tokens.positions",
        "linemap.bin",
        "bloom.bin",
        "grams.checks",
        "grams.bloom-check",
        "source.table",
        "source.data",
    ];
    let mut bytes = 0u64;
    for file in fs::read_dir(path)? {
        let file = file?;
        ensure!(
            file.file_name()
                .to_str()
                .is_some_and(|name| segment_files.contains(&name)),
            "Unrecognized segment file"
        );
        bytes = bytes.saturating_add(regular(&file.path(), false)?.len());
    }
    for name in ["grams.dict", "grams.postings"] {
        regular(&path.join(name), false)?;
    }
    if let Some(meta) = meta {
        if meta.profile == IndexProfile::Full {
            for name in ["tokens.dict", "tokens.postings", "linemap.bin"] {
                regular(&path.join(name), false)?;
            }
        }
        if meta.has_positions {
            regular(&path.join("tokens.positions"), false)?;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(indexes: &Path, dry_run: bool, verbose: bool) -> Result<Report> {
        super::scan(indexes, dry_run, verbose, &LegacyPolicy::default())
    }
    fn prune_entry(container: &Path, dry_run: bool) -> Result<Outcome> {
        super::prune_entry(container, dry_run, &LegacyPolicy::default())
    }
    fn prune_locked(container: &Path, original: &Registration, dry_run: bool) -> Result<Outcome> {
        super::prune_locked(container, original, dry_run, &LegacyPolicy::default())
    }
    fn offline() -> LegacyPolicy {
        LegacyPolicy {
            include: true,
            ..LegacyPolicy::default()
        }
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        base: PathBuf,
        indexes: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            crate::utils::isolate_test_storage().unwrap();
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let indexes = base.join("indexes");
            fs::create_dir(&indexes).unwrap();
            Self {
                _temporary: temporary,
                base,
                indexes,
            }
        }
        fn add(&self, name: &str) -> (PathBuf, PathBuf) {
            let root = self.base.join(name);
            let container = self
                .indexes
                .join(crate::utils::app_data::recorded_container_name(&root).unwrap());
            self.generation(&container.join("generations/gen-1"), &root);
            fs::write(container.join("CURRENT"), "gen-1\n").unwrap();
            (root, container)
        }
        fn generation(&self, path: &Path, root: &Path) {
            fs::create_dir_all(path.join("segments")).unwrap();
            let meta = IndexMeta {
                root_path: root.to_path_buf(),
                ..IndexMeta::default()
            };
            fs::write(path.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
            fs::write(path.join("docs.bin"), 0u32.to_le_bytes()).unwrap();
            fs::write(path.join("paths.bin"), 0u32.to_le_bytes()).unwrap();
            fs::write(path.join("lease"), []).unwrap();
        }
        fn legacy(&self, name: &str, mixed: bool) -> (PathBuf, PathBuf) {
            let (root, container) = self.add(name);
            if !mixed {
                fs::remove_file(container.join("CURRENT")).unwrap();
                fs::remove_dir_all(container.join("generations")).unwrap();
            }
            self.generation(&container, &root);
            fs::remove_file(container.join("lease")).unwrap();
            (root, container)
        }
    }

    #[test]
    fn canonical_missing_root_requires_directory_ancestors() {
        let fixture = Fixture::new();
        // canonicalize supplies a verbatim drive prefix on Windows. Walk the
        // complete root, never query the drive prefix as a standalone path.
        assert!(missing_root(&fixture.base.join("absent/child")).unwrap());
        assert!(!missing_root(&fixture.base).unwrap());
        let file = fixture.base.join("file");
        fs::write(&file, "not a directory").unwrap();
        assert!(missing_root(&file.join("child")).is_err());
    }

    #[test]
    fn preview_is_read_only_and_apply_preserves_lock_identity() {
        let fixture = Fixture::new();
        let (_, container) = fixture.add("missing");
        let stamp = fs::metadata(&fixture.indexes).unwrap().modified().unwrap();
        let preview = scan(&fixture.indexes, true, false).unwrap();
        assert_eq!(preview.eligible, 1);
        assert_eq!(preview.removed, 0);
        assert!(preview.bytes > 0);
        assert!(container.is_dir());
        assert!(!container.with_extension("lock").exists());
        assert_eq!(
            stamp,
            fs::metadata(&fixture.indexes).unwrap().modified().unwrap()
        );
        let lock_path = container.with_extension("lock");
        fs::write(&lock_path, b"keep lock inode and content").unwrap();
        let lock = File::open(&lock_path).unwrap();
        let report = scan(&fixture.indexes, false, false).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!container.exists());
        assert_eq!(
            fs::read(&lock_path).unwrap(),
            b"keep lock inode and content"
        );
        assert!(try_exclusive(&lock).unwrap());
        let contender = File::open(&lock_path).unwrap();
        assert!(
            !try_exclusive(&contender).unwrap(),
            "lock identity was replaced"
        );
        let empty = scan(&fixture.indexes, false, false).unwrap();
        assert_eq!(empty.eligible, 0);
        assert_eq!(empty.locks, 1);
        let absent = fixture.base.join("not-created");
        assert_eq!(scan(&absent, true, false).unwrap().scanned, 0);
        assert!(!absent.exists());
    }

    #[test]
    fn malformed_entries_do_not_abort_cleanup_and_legacy_is_preserved() {
        let fixture = Fixture::new();
        let (live, live_container) = fixture.add("live");
        fs::create_dir(&live).unwrap();
        let (_, missing) = fixture.add("missing");
        let (_, manifest) = fixture.add("manifest");
        fs::write(manifest.join("CURRENT"), "../elsewhere").unwrap();
        let (_, metadata) = fixture.add("metadata");
        fs::write(metadata.join("generations/gen-1/meta.json"), "{").unwrap();
        let (_, unknown) = fixture.add("unknown");
        fs::write(unknown.join("generations/gen-1/user-file"), "preserve").unwrap();
        let (_, identity) = fixture.add("identity");
        let wrong_identity = fixture.indexes.join("some-other-container");
        fs::rename(identity, &wrong_identity).unwrap();
        let (legacy_root, legacy) = fixture.add("legacy");
        fs::remove_file(legacy.join("CURRENT")).unwrap();
        fixture.generation(&legacy, &legacy_root);
        let (mixed_root, mixed) = fixture.add("mixed");
        fixture.generation(&mixed, &mixed_root);
        let (_, truncated) = fixture.add("truncated");
        fs::write(truncated.join("generations/gen-1/docs.bin"), [1, 0, 0, 0]).unwrap();
        let report = scan(&fixture.indexes, false, false).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.errors, 0);
        assert!(!missing.exists());
        for path in [
            live_container,
            manifest,
            metadata,
            unknown,
            wrong_identity,
            legacy,
            mixed,
            truncated,
        ] {
            assert!(path.exists(), "removed protected entry {path:?}");
        }
        assert_eq!(report.skipped["legacy layout has no reader leases"], 2);
    }

    #[test]
    fn busy_writer_and_any_generation_reader_are_skipped_without_waiting() {
        let fixture = Fixture::new();
        let (root, container) = fixture.add("missing");
        let writer = writer_lock(&container, false).unwrap().unwrap();
        assert!(try_exclusive(&writer).unwrap());
        assert!(matches!(
            prune_entry(&container, false).unwrap(),
            Outcome::Skipped("busy writer or generation reader")
        ));
        drop(writer);
        fixture.generation(&container.join("generations/gen-0"), &root);
        let reader = File::open(container.join("generations/gen-0/lease")).unwrap();
        FileExt::lock_shared(&reader).unwrap();
        for dry_run in [true, false] {
            assert!(matches!(
                prune_entry(&container, dry_run).unwrap(),
                Outcome::Skipped("busy writer or generation reader")
            ));
            assert!(container.exists());
        }
        drop(reader);
        assert!(matches!(
            prune_entry(&container, false).unwrap(),
            Outcome::Candidate { removed: true, .. }
        ));
    }

    #[test]
    fn source_and_metadata_are_rechecked_after_lock_acquisition() {
        let fixture = Fixture::new();
        let (root, container) = fixture.add("missing");
        let original = registration(&container).unwrap();
        let writer = writer_lock(&container, false).unwrap().unwrap();
        assert!(try_exclusive(&writer).unwrap());
        fs::create_dir(&root).unwrap();
        assert!(matches!(
            prune_locked(&container, &original, false).unwrap(),
            Outcome::Skipped("source or registration changed during scan")
        ));
        fs::remove_dir(root).unwrap();
        let mut meta: IndexMeta = serde_json::from_slice(&original.metadata).unwrap();
        meta.updated_at += 1;
        fs::write(&original.metadata_path, serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(matches!(
            prune_locked(&container, &original, false).unwrap(),
            Outcome::Skipped("source or registration changed during scan")
        ));
        assert!(container.exists());
    }

    #[test]
    fn traversal_errors_are_reported_while_other_candidates_continue() {
        let fixture = Fixture::new();
        let (_, inaccessible) = fixture.add("file-parent/child");
        fs::write(fixture.base.join("file-parent"), "not a directory").unwrap();
        let (_, removable) = fixture.add("missing");
        let report = scan(&fixture.indexes, false, false).unwrap();
        assert_eq!(report.errors, 1);
        assert_eq!(report.removed, 1);
        assert!(inaccessible.exists());
        assert!(!removable.exists());
    }

    #[test]
    fn missing_leases_and_inconsistent_old_generations_preserve_container() {
        let fixture = Fixture::new();
        let (_, missing_lease) = fixture.add("missing-lease");
        fs::remove_file(missing_lease.join("generations/gen-1/lease")).unwrap();
        let (_, old) = fixture.add("old");
        fixture.generation(
            &old.join("generations/gen-0"),
            &fixture.base.join("different-root"),
        );
        let report = scan(&fixture.indexes, false, false).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.skipped.values().sum::<usize>(), 2);
        assert!(missing_lease.exists() && old.exists());
    }

    #[cfg(unix)]
    #[test]
    fn root_ancestor_container_and_internal_symlinks_are_preserved() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let (root, root_link) = fixture.add("root-link");
        symlink(fixture.base.join("absent-target"), root).unwrap();
        let (_, ancestor_link) = fixture.add("ancestor/child");
        symlink(
            fixture.base.join("absent-ancestor"),
            fixture.base.join("ancestor"),
        )
        .unwrap();
        let (_, internal_link) = fixture.add("internal-link");
        let docs = internal_link.join("generations/gen-1/docs.bin");
        let outside = fixture.base.join("outside.bin");
        fs::rename(&docs, &outside).unwrap();
        symlink(&outside, docs).unwrap();
        let (_, lock_link) = fixture.add("lock-link");
        symlink(&outside, lock_link.with_extension("lock")).unwrap();
        symlink(&internal_link, fixture.indexes.join("container-link")).unwrap();
        let report = scan(&fixture.indexes, false, false).unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.errors, 0);
        let included = super::scan(&fixture.indexes, false, false, &offline()).unwrap();
        assert_eq!(included.removed, 0);
        assert_eq!(included.errors, 0);
        for path in [root_link, ancestor_link, internal_link, lock_link, outside] {
            assert!(path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_source_ancestor_is_an_error_not_an_absent_root() {
        use std::os::unix::fs::PermissionsExt;
        // A privileged process can traverse a mode-000 directory, so it cannot
        // exercise this operating-system permission boundary.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = Fixture::new();
        let (_, container) = fixture.add("private/child");
        let private = fixture.base.join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
        let report = scan(&fixture.indexes, false, false);
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let report = report.unwrap();
        assert_eq!(report.errors, 1);
        assert_eq!(report.removed, 0);
        assert!(container.exists());
    }

    #[test]
    fn offline_opt_in_removes_only_valid_missing_legacy_and_mixed_layouts() {
        let fixture = Fixture::new();
        let (_, legacy) = fixture.legacy("legacy", false);
        let (_, mixed) = fixture.legacy("mixed", true);
        let (live, live_container) = fixture.legacy("live", false);
        fs::create_dir(&live).unwrap();
        let (_, malformed) = fixture.legacy("malformed", true);
        fs::write(malformed.join("meta.json"), "{").unwrap();
        let (_, unknown) = fixture.legacy("unknown", false);
        fs::write(unknown.join("user-data"), "keep").unwrap();
        let (_, truncated) = fixture.legacy("truncated", true);
        fs::write(truncated.join("docs.bin"), [1, 0, 0, 0]).unwrap();
        let (_, busy) = fixture.legacy("busy", false);
        let writer = writer_lock(&busy, false).unwrap().unwrap();
        assert!(try_exclusive(&writer).unwrap());
        assert_eq!(scan(&fixture.indexes, false, false).unwrap().removed, 0);
        let preview = super::scan(&fixture.indexes, true, false, &offline()).unwrap();
        assert_eq!(preview.eligible, 2);
        assert_eq!(preview.removed, 0);
        assert!(!legacy.with_extension("lock").exists());
        assert!(!mixed.with_extension("lock").exists());
        let report = super::scan(&fixture.indexes, false, false, &offline()).unwrap();
        assert_eq!(report.removed, 2);
        assert_eq!(report.errors, 0);
        assert!(!legacy.exists() && !mixed.exists());
        for path in [live_container, malformed, unknown, truncated, busy] {
            assert!(path.exists(), "removed protected legacy entry {path:?}");
        }
    }

    #[test]
    fn offline_opt_in_still_requires_all_mixed_generation_leases_and_meta_rechecks() {
        let fixture = Fixture::new();
        let (root, container) = fixture.legacy("mixed", true);
        fixture.generation(&container.join("generations/gen-0"), &root);
        let reader = File::open(container.join("generations/gen-0/lease")).unwrap();
        FileExt::lock_shared(&reader).unwrap();
        assert!(matches!(
            super::prune_entry(&container, false, &offline()).unwrap(),
            Outcome::Skipped("busy writer or generation reader")
        ));
        drop(reader);
        let original = registration(&container).unwrap();
        let writer = writer_lock(&container, false).unwrap().unwrap();
        assert!(try_exclusive(&writer).unwrap());
        let mut meta: IndexMeta =
            serde_json::from_slice(original.legacy_metadata.as_ref().unwrap()).unwrap();
        meta.updated_at += 1;
        fs::write(
            container.join("meta.json"),
            serde_json::to_vec(&meta).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            super::prune_locked(&container, &original, false, &offline()).unwrap(),
            Outcome::Skipped("source or registration changed during scan")
        ));
        assert!(container.exists());
        drop(writer);
        assert!(matches!(
            super::prune_entry(&container, false, &offline()).unwrap(),
            Outcome::Candidate { removed: true, .. }
        ));
    }

    #[test]
    fn known_or_unverifiable_daemon_readers_preserve_legacy_with_opt_in() {
        let fixture = Fixture::new();
        let (root, legacy) = fixture.legacy("legacy", false);
        let (_, modern) = fixture.add("modern");
        let mut policy = offline();
        policy.loaded_roots.insert(root);
        let report = super::scan(&fixture.indexes, true, false, &policy).unwrap();
        assert_eq!(report.eligible, 1);
        assert_eq!(report.skipped["legacy root is loaded by the daemon"], 1);
        policy.status_unknown = true;
        let report = super::scan(&fixture.indexes, true, false, &policy).unwrap();
        assert_eq!(report.eligible, 1);
        assert_eq!(
            report.skipped["daemon status unavailable for legacy cleanup"],
            1
        );
        assert!(legacy.exists() && modern.exists());
    }
}
