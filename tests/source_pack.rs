//! Exercise opt-in packing through real CLI processes without changing this
//! test process's environment (other tests run concurrently).
#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_codec(root: &Path, indexes: &Path, args: &[&str], compression: bool) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_fxi"))
        .args(args)
        .current_dir(root)
        .env("FXI_APP_DATA", indexes.join("app-data"))
        .env("FXI_INDEXES", indexes)
        .env("FXI_SOURCE_PACK", "1")
        .env(
            "FXI_SOURCE_PACK_COMPRESSION",
            if compression { "1" } else { "0" },
        )
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
fn required_literal_regexes_match_across_compressed_block_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    let mut texts = Vec::new();
    for id in 0..132 {
        let text = format!(
            "{}needle{}\r\nneedle\n{}\nneedle\r",
            "x".repeat(4090 + id % 12),
            if id % 2 == 0 { "0" } else { "K" },
            "needle unrelated ".repeat(700)
        );
        fs::write(root.path().join(format!("{id:03}.txt")), &text).unwrap();
        texts.push(text);
    }
    run_codec(
        root.path(),
        indexes.path(),
        &["index", "--force", "."],
        true,
    );
    for pattern in [
        "needle.*0",
        "^needle$",
        r"needle\r$",
        r"\bneedle\b",
        "needle.*K",
        "needle.*[0-9]{12}",
    ] {
        let re = regex::Regex::new(pattern).unwrap();
        let expected: Vec<_> = texts
            .iter()
            .enumerate()
            .filter(|(_, text)| text.lines().any(|line| re.is_match(line)))
            .map(|(id, _)| format!("{id:03}.txt"))
            .collect();
        let out = run_codec(
            root.path(),
            indexes.path(),
            &["-l", "--color=never", &format!("re:/{pattern}/"), "-p", "."],
            true,
        );
        let found: Vec<_> = out
            .lines()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(found, expected, "{pattern}");
    }
}

#[test]
fn packed_cli_matches_live_files_after_edits_corruption_compaction_and_rebuild() {
    packed_cli_matches_live_files_after_edits_corruption_compaction_and_rebuild_case(false);
}
#[test]
fn compressed_packed_cli_matches_live_files_after_edits_corruption_compaction_and_rebuild() {
    packed_cli_matches_live_files_after_edits_corruption_compaction_and_rebuild_case(true);
}
fn packed_cli_matches_live_files_after_edits_corruption_compaction_and_rebuild_case(
    compression: bool,
) {
    let run =
        |root: &Path, indexes: &Path, args: &[&str]| run_codec(root, indexes, args, compression);
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
fn delta_and_compaction_preserve_pinned_pack_bytes_and_leave_orphan_links() {
    delta_and_compaction_preserve_pinned_pack_bytes_and_leave_orphan_links_case(false);
}
#[test]
fn compressed_delta_and_compaction_preserve_pinned_pack_bytes_and_leave_orphan_links() {
    delta_and_compaction_preserve_pinned_pack_bytes_and_leave_orphan_links_case(true);
}
fn delta_and_compaction_preserve_pinned_pack_bytes_and_leave_orphan_links_case(compression: bool) {
    let run =
        |root: &Path, indexes: &Path, args: &[&str]| run_codec(root, indexes, args, compression);
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
    // source bytes mapped. Updates must leave this orphan unchanged, rather
    // than recapture live bytes against the inherited segment's old postings.
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
    assert_eq!(files(&delta, "source.table").len(), 3);
    for (old_path, bytes, map) in &snapshots {
        let inherited = delta.join(old_path.strip_prefix(&original).unwrap());
        assert_eq!(fs::read(old_path).unwrap(), *bytes);
        assert_eq!(&map[..], bytes);
        let same_inode =
            fs::metadata(old_path).unwrap().ino() == fs::metadata(inherited).unwrap().ino();
        assert!(same_inode, "inherited source bytes must remain untouched");
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

#[test]
fn legacy_raw_and_captured_compressed_generations_match_live_cli_modes() {
    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    for n in 0..130 {
        fs::write(
            root.path().join(format!("file-{n:03}.txt")),
            format!(
                "{}\nneedle suffix\n{}\n",
                "x".repeat(4094),
                "y".repeat(9000)
            ),
        )
        .unwrap();
    }
    run_codec(
        root.path(),
        indexes.path(),
        &["index", "--force", "."],
        false,
    );
    // Legacy packs remain readable, but cannot establish posting provenance
    // when compaction constructs a new revision-bound pack.
    for path in files(indexes.path(), "source.table") {
        let mut bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"FXISRC04");
        bytes[..8].copy_from_slice(b"FXISRC02");
        fs::write(path, bytes).unwrap();
    }
    fs::write(
        root.path().join("new.txt"),
        format!("{}\nneedle suffix\n{}", "z".repeat(8190), "q".repeat(9000)),
    )
    .unwrap();
    run_codec(root.path(), indexes.path(), &["index", "."], true);
    let headers: Vec<_> = files(indexes.path(), "source.table")
        .iter()
        .map(|p| fs::read(p).unwrap()[..8].to_vec())
        .collect();
    assert!(headers.iter().any(|h| h == b"FXISRC02"));
    assert!(headers.iter().any(|h| h == b"FXISRC05"));
    for args in [
        vec!["-l", "-F", "needle", "."],
        vec!["-l", "re:/need.e/", "-p", "."],
        vec!["-l", "re:/^needle suffix$/", "-p", "."],
        vec!["-l", "re:/^$/", "-p", "."],
        vec!["-l", "re:/\\Aneedle/", "-p", "."],
        vec!["-l", "re:/(?i)NEEDLE/", "-p", "."],
        vec!["-l", "-i", "-F", "NEEDLE", "."],
        vec!["-c", "-F", "needle", "."],
        vec!["-F", "-C", "1", "needle", "."],
    ] {
        let packed = run_codec(root.path(), indexes.path(), &args, true);
        let live = Command::new(env!("CARGO_BIN_EXE_fxi"))
            .args(&args)
            .current_dir(root.path())
            .env("FXI_APP_DATA", indexes.path().join("app-data"))
            .env("FXI_INDEXES", indexes.path())
            .env("FXI_SOURCE_PACK", "0")
            .env("FXI_SOCKET", indexes.path().join("absent.sock"))
            .env("XDG_RUNTIME_DIR", indexes.path())
            .output()
            .unwrap();
        assert!(
            live.status.success(),
            "{}",
            String::from_utf8_lossy(&live.stderr)
        );
        assert_eq!(packed, String::from_utf8(live.stdout).unwrap(), "{args:?}");
    }
    run_codec(root.path(), indexes.path(), &["compact", "."], true);
    let tables = files(indexes.path(), "source.table");
    assert_eq!(tables.len(), 1);
    let table = fs::read(&tables[0]).unwrap();
    assert_eq!(
        u64::from_le_bytes(table[8..16].try_into().unwrap()),
        1,
        "only the captured delta has proven posting provenance; legacy records must be omitted"
    );
    let output = run_codec(
        root.path(),
        indexes.path(),
        &["-l", "-F", "needle", "."],
        true,
    );
    assert_eq!(output.lines().count(), 131);
}
