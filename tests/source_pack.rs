//! Exercise opt-in packing through real CLI processes without changing this
//! test process's environment (other tests run concurrently).
#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(root: &Path, indexes: &Path, args: &[&str]) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_fxi"))
        .args(args)
        .current_dir(root)
        .env("FXI_INDEXES", indexes)
        .env("FXI_SOURCE_PACK", "1")
        .env("FXI_SOCKET", indexes.join("absent.sock"))
        .env("XDG_RUNTIME_DIR", indexes)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}
fn files(directory: &Path, name: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path, name));
        } else if path.file_name().unwrap() == name {
            result.push(path);
        }
    }
    result
}

#[test]
fn packed_cli_matches_live_files_after_edits_corruption_compaction_and_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    let a = root.path().join("a.txt");
    let b = root.path().join("b.txt");
    fs::write(&a, "prefix needle suffix\n").unwrap();
    fs::write(&b, "needle\nneedle\n").unwrap();
    fs::write(root.path().join("c.txt"), "other text\n").unwrap();
    // The production source-pack policy requires at least 128 candidates.
    // These unchanged matches keep every search above that threshold, including
    // searches after the two editable files stop matching or disappear.
    let stable: Vec<_> = (0..128)
        .map(|id| {
            let name = format!("stable-{id:03}.txt");
            fs::write(root.path().join(&name), "needle stable\n").unwrap();
            name
        })
        .collect();
    let expected = |extra: &[&str]| {
        let mut names = stable.clone();
        names.extend(extra.iter().map(|name| (*name).to_owned()));
        names.sort();
        names
    };
    let index = || {
        run(
            root.path(),
            indexes.path(),
            &["index", "--force", "--chunk-size", "64", "."],
        )
    };
    let search = || {
        let out = run(
            root.path(),
            indexes.path(),
            &["-l", "--color=never", "re:/needle/", "-p", "."],
        );
        let mut found: Vec<_> = out
            .lines()
            .map(|line| {
                Path::new(line)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        found.sort();
        found
    };
    index();
    assert_eq!(files(indexes.path(), "source.table").len(), 3);
    assert_eq!(search(), expected(&["a.txt", "b.txt"]));
    let modified = fs::metadata(&a).unwrap().modified().unwrap();
    fs::write(&a, "prefix absent suffix\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(search(), expected(&["b.txt"]));
    fs::write(&a, "prefix needle suffix\n").unwrap();
    fs::write(&b, b"needle\n\xff").unwrap();
    assert_eq!(search(), expected(&["a.txt"]));
    fs::remove_file(&a).unwrap();
    assert_eq!(search(), expected(&[]));
    fs::write(&a, "needle\n").unwrap();
    fs::write(&b, "needle\n").unwrap();
    index();
    // Same-length valid UTF-8 corruption would cause false negatives without
    // per-source integrity validation and fallback to the original files.
    for path in files(indexes.path(), "source.data") {
        let contents = fs::read(&path).unwrap();
        fs::write(path, vec![b'x'; contents.len()]).unwrap();
    }
    assert_eq!(search(), expected(&["a.txt", "b.txt"]));
    for path in files(indexes.path(), "source.table") {
        fs::write(path, b"broken").unwrap();
    }
    assert_eq!(search(), expected(&["a.txt", "b.txt"]));
    run(root.path(), indexes.path(), &["compact", "."]);
    assert_eq!(files(indexes.path(), "source.table").len(), 1);
    assert_eq!(search(), expected(&["a.txt", "b.txt"]));
    index();
    assert_eq!(search(), expected(&["a.txt", "b.txt"]));
}

#[test]
fn delta_and_compaction_preserve_pinned_pack_bytes_and_repair_orphan_links() {
    use std::os::unix::fs::MetadataExt;

    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "needle old a\n").unwrap();
    fs::write(root.path().join("b.txt"), "needle old b\n").unwrap();
    // Keep the edit fraction below the incremental-rebuild threshold.
    for id in 0..20 {
        fs::write(root.path().join(format!("filler-{id}.txt")), "unrelated\n").unwrap();
    }
    run(
        root.path(),
        indexes.path(),
        &["index", "--force", "--chunk-size", "10", "."],
    );
    let manifests = files(indexes.path(), "CURRENT");
    assert_eq!(manifests.len(), 1);
    let manifest = &manifests[0];
    let current = || {
        manifest
            .parent()
            .unwrap()
            .join("generations")
            .join(fs::read_to_string(manifest).unwrap().trim())
    };
    let original = current();
    let lease = fs::File::open(original.join("lease")).unwrap();
    fs2::FileExt::lock_shared(&lease).unwrap();
    let snapshots: Vec<_> = files(&original, "source.data")
        .into_iter()
        .map(|path| {
            let source = fs::File::open(&path).unwrap();
            let bytes = fs::read(&path).unwrap();
            assert!(!bytes.is_empty());
            // These immutable files stay pinned by the generation lease. The
            // test checks their ordinary bytes before touching each old mmap.
            let map = unsafe { memmap2::Mmap::map(&source).unwrap() };
            (path, bytes, map)
        })
        .collect();
    assert_eq!(snapshots.len(), 3);
    let orphan = snapshots
        .iter()
        .find(|(_, bytes, _)| String::from_utf8_lossy(bytes).contains("needle old a"))
        .unwrap()
        .0
        .parent()
        .unwrap();
    // Simulate a missing accelerator table while an older reader still has its
    // source bytes mapped. The inherited data link must be replaced, not opened
    // with truncation, when the next generation reconstructs this table.
    fs::remove_file(orphan.join("source.table")).unwrap();
    fs::write(
        root.path().join("a.txt"),
        "needle replacement a is longer\n",
    )
    .unwrap();
    fs::remove_file(root.path().join("b.txt")).unwrap();
    fs::write(root.path().join("c.txt"), "needle new c\n").unwrap();
    run(root.path(), indexes.path(), &["index", "."]);
    let delta = current();
    assert_ne!(original, delta);
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(delta.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["tombstone_count"], 2);
    assert_eq!(meta["segment_count"], 4);
    assert_eq!(files(&delta, "source.table").len(), 4);
    for (old_path, bytes, map) in &snapshots {
        let inherited = delta.join(old_path.strip_prefix(&original).unwrap());
        assert_eq!(fs::read(old_path).unwrap(), *bytes);
        assert_eq!(&map[..], bytes);
        let same_inode =
            fs::metadata(old_path).unwrap().ino() == fs::metadata(inherited).unwrap().ino();
        assert_eq!(same_inode, old_path.parent().unwrap() != orphan);
    }
    let search = || {
        let output = run(
            root.path(),
            indexes.path(),
            &["-l", "--color=never", "re:/needle/", "-p", "."],
        );
        let mut names: Vec<_> = output
            .lines()
            .map(|line| Path::new(line).file_name().unwrap().to_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(search(), ["a.txt", "c.txt"]);
    run(root.path(), indexes.path(), &["compact", "."]);
    let compacted = current();
    assert_ne!(delta, compacted);
    assert_eq!(files(&compacted, "source.table").len(), 1);
    assert_eq!(search(), ["a.txt", "c.txt"]);
    let docs = fxi::index::reader::read_documents(&compacted).unwrap();
    assert_eq!(docs.len(), 22);
    assert!(docs.iter().enumerate().all(|(index, doc)| {
        doc.doc_id as usize == index + 1 && doc.segment_id == 1 && doc.is_valid()
    }));
    assert!(
        original.exists(),
        "the old pack's lease must prevent collection"
    );
    for (path, bytes, map) in &snapshots {
        assert_eq!(fs::read(path).unwrap(), *bytes);
        assert_eq!(&map[..], bytes);
    }
    drop(snapshots);
    drop(lease);
    run(root.path(), indexes.path(), &["index", "--force", "."]);
    assert!(!original.exists(), "unleased packs should be collected");
    assert_eq!(search(), ["a.txt", "c.txt"]);
}
