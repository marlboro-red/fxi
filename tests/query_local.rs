use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, indexes: &Path, enabled: bool, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fxi"))
        .args(args)
        .current_dir(root)
        .env("FXI_INDEXES", indexes)
        .env("FXI_SOCKET", indexes.join("unused.sock"))
        .env("XDG_RUNTIME_DIR", indexes)
        .env("FXI_QUERY_LOCAL", if enabled { "1" } else { "0" })
        .env("FXI_NEGATIVE_ROUTING", "0")
        .output()
        .unwrap()
}
fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn records(output: Output) -> Vec<String> {
    let bytes = success(output);
    let mut rows: Vec<_> = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    rows.sort();
    rows
}
fn compare(root: &Path, indexes: &Path) {
    for query in [
        "re:/rareNeedle/",
        "re:/absentSymbol94283/",
        "re:/rare.*[0-9]/",
        "re:/(?i)rareneedle/",
        "re:/^alpha.*omega$/",
        "re:/alpha|rareNeedle/",
        "alpha omega",
        "\"alpha omega\"",
        "alpha -rareNeedle",
        "re:/K|ſ/",
        "re:/^$/",
        "re:/.*/",
        "re:/rareNeedle/ ext:rs",
    ] {
        for mode in ["-l", "-c", ""] {
            let mut args = vec!["--color=never", query, "-p", "."];
            if !mode.is_empty() {
                args.insert(0, mode);
            }
            assert_eq!(
                records(run(root, indexes, true, &args)),
                records(run(root, indexes, false, &args)),
                "{mode} {query}"
            );
        }
    }
    assert!(
        !run(root, indexes, true, &["-l", "re:/[/", "-p", "."])
            .status
            .success()
    );
}

#[test]
fn checked_queries_match_strict_across_profiles_updates_and_compaction() {
    for profile in ["lean", "full"] {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        for i in 0..40 {
            let text = if i % 7 == 0 {
                "rareNeedle 123\r\nalpha omega\nK ſ\n"
            } else {
                "other contents\n\n"
            };
            fs::write(root.path().join(format!("file-{i}.rs")), text).unwrap();
        }
        success(run(
            root.path(),
            indexes.path(),
            true,
            &["index", "--profile", profile, "--chunk-size", "8", "."],
        ));
        compare(root.path(), indexes.path());
        let paths = records(run(
            root.path(),
            indexes.path(),
            true,
            &["-l", "re:/rareNeedle/", "-p", "."],
        ));
        assert_eq!(paths.len(), 6);
        fs::write(root.path().join("new.rs"), "rareNeedle alpha omega\n").unwrap();
        fs::write(root.path().join("file-0.rs"), "replacement contents\n").unwrap();
        fs::remove_file(root.path().join("file-7.rs")).unwrap();
        success(run(root.path(), indexes.path(), true, &["index", "."]));
        compare(root.path(), indexes.path());
        assert_eq!(
            records(run(
                root.path(),
                indexes.path(),
                true,
                &["-l", "re:/rareNeedle/", "-p", "."]
            ))
            .len(),
            5
        );
        success(run(root.path(), indexes.path(), true, &["compact", "."]));
        compare(root.path(), indexes.path());
        // Eager opening remains an integrity check even with the experiment set.
        success(run(root.path(), indexes.path(), true, &["stats", "."]));
    }
}

#[test]
fn timestamp_preflight_cannot_bypass_damaged_query_local_evidence() {
    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source.rs"), "fn main() {}\n").unwrap();
    let both = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fxi"))
            .args(args)
            .current_dir(root.path())
            .env("FXI_INDEXES", indexes.path())
            .env("FXI_SOCKET", indexes.path().join("unused.sock"))
            .env("XDG_RUNTIME_DIR", indexes.path())
            .env("FXI_QUERY_LOCAL", "1")
            .env("FXI_NEGATIVE_ROUTING", "1")
            .output()
            .unwrap()
    };
    success(both(&["index", "."]));
    fn damage(path: &Path) -> usize {
        let mut count = 0;
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                count += damage(&path);
            } else if path.file_name().unwrap() == "grams.checks" {
                fs::write(path, b"damaged").unwrap();
                count += 1;
            }
        }
        count
    }
    assert!(damage(indexes.path()) > 0);
    fn current_files(path: &Path, found: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                current_files(&path, found);
            } else if path.file_name().unwrap() == "CURRENT" {
                found.push((path.clone(), fs::read(path).unwrap()));
            }
        }
    }
    let mut current = Vec::new();
    current_files(indexes.path(), &mut current);
    assert_eq!(current.len(), 1);
    fs::write(root.path().join("new.rs"), "fn another() {}\n").unwrap();
    for args in [["index", "."], ["compact", "."], ["stats", "."]] {
        assert!(!both(&args).status.success(), "{args:?}");
        assert_eq!(fs::read(&current[0].0).unwrap(), current[0].1);
    }
    let result = both(&["-l", "-F", "absentSymbol94283", "."]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}
