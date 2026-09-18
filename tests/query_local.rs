use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, indexes: &Path, enabled: bool, args: &[&str]) -> Output {
    run_with_routing(root, indexes, enabled, false, args)
}
fn run_with_routing(
    root: &Path,
    indexes: &Path,
    enabled: bool,
    routing: bool,
    args: &[&str],
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fxi"))
        .args(args)
        .current_dir(root)
        .env("FXI_APP_DATA", indexes.join("app-data"))
        .env("FXI_INDEXES", indexes)
        .env("FXI_SOCKET", indexes.join("unused.sock"))
        .env("XDG_RUNTIME_DIR", indexes)
        .env("FXI_QUERY_LOCAL", if enabled { "1" } else { "0" })
        .env("FXI_GENERATION_ROUTING", if routing { "1" } else { "0" })
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
            .env("FXI_APP_DATA", indexes.path().join("app-data"))
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

#[test]
fn invalid_optional_routing_manifest_falls_back_to_checked_search() {
    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    fs::write(root.path().join("source.rs"), "needle\n").unwrap();
    success(run(root.path(), indexes.path(), true, &["index", "."]));
    fn find(path: &Path) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == "query-routing.bin" {
                return Some(path);
            }
            if path.is_dir()
                && let Some(found) = find(&path)
            {
                return Some(found);
            }
        }
        None
    }
    let manifest = find(indexes.path()).expect("checked routing evidence issued");
    fs::write(&manifest, b"damaged").unwrap();
    for exists in [true, false] {
        if !exists {
            fs::remove_file(&manifest).unwrap();
        }
        assert!(
            records(run(
                root.path(),
                indexes.path(),
                true,
                &["-l", "re:/zzzAbsent123/", "-p", "."]
            ))
            .is_empty()
        );
        assert_eq!(
            records(run(
                root.path(),
                indexes.path(),
                true,
                &["-l", "re:/needle/", "-p", "."]
            ))
            .len(),
            1
        );
        assert!(
            !run(
                root.path(),
                indexes.path(),
                true,
                &["-l", "re:/[/", "-p", "."]
            )
            .status
            .success()
        );
    }
}

#[test]
fn generation_routes_match_strict_and_fall_back_after_damage_updates_and_compaction() {
    for profile in ["lean", "full"] {
        let root = tempfile::tempdir().unwrap();
        let indexes = tempfile::tempdir().unwrap();
        fs::write(root.path().join("one.rs"), "rareNeedle alpha beta\n").unwrap();
        fs::write(root.path().join("two.rs"), "other text K\n").unwrap();
        let run = |args: &[&str]| run_with_routing(root.path(), indexes.path(), true, true, args);
        success(run(&[
            "index",
            ".",
            "--force",
            "--profile",
            profile,
            "--chunk-size",
            "1",
        ]));
        for step in 0..3 {
            for query in [
                "re:/rareNeedle/",
                "re:/absentSymbol94283/",
                "re:/alpha|other/",
                "re:/(?i)rareneedle/",
                "re:/rareNeedle/ ext:rs",
                "alpha beta",
            ] {
                for mode in ["-l", "-c"] {
                    let args = [mode, query, "-p", "."];
                    assert_eq!(
                        records(run(&args)),
                        records(run_with_routing(
                            root.path(),
                            indexes.path(),
                            false,
                            false,
                            &args
                        )),
                        "{profile} step{step} {query}"
                    );
                }
            }
            if step == 0 {
                fs::write(root.path().join("two.rs"), "rareNeedle modified\n").unwrap();
                fs::remove_file(root.path().join("one.rs")).unwrap();
                success(run(&["index", "."]));
            } else if step == 1 {
                success(run(&["compact", "."]));
            }
        }
        let container = fs::read_dir(indexes.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.join("CURRENT").is_file())
            .unwrap();
        let current = fs::read_to_string(container.join("CURRENT")).unwrap();
        let index = container.join("generations").join(current.trim());
        let route = index.join("generation-routing.bin");
        assert!(route.is_file());
        fs::write(&route, b"damaged").unwrap();
        assert_eq!(records(run(&["-l", "re:/rareNeedle/", "-p", "."])).len(), 1);
        assert!(records(run(&["-l", "re:/absentSymbol94283/", "-p", "."])).is_empty());
        assert!(
            !run(&["stats", "."]).status.success(),
            "strict integrity check must reject damaged router"
        );
        fs::remove_file(route).unwrap();
        assert_eq!(records(run(&["-l", "re:/rareNeedle/", "-p", "."])).len(), 1);
    }
}
