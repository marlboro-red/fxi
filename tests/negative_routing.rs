#![cfg(unix)]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn find(directory: &Path, name: &str) -> PathBuf {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() == name {
            return path;
        }
        if path.is_dir() {
            let result = find(&path, name);
            if !result.as_os_str().is_empty() {
                return result;
            }
        }
    }
    PathBuf::new()
}

#[test]
fn certified_cli_preserves_core_errors_invalid_regexes_and_updated_matches() {
    let root = tempfile::tempdir().unwrap();
    let indexes = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "abc\n").unwrap();
    fs::write(root.path().join("b.txt"), "bcd\n").unwrap();
    for i in 0..20 {
        fs::write(
            root.path().join(format!("filler-{i}.txt")),
            "unrelated text\n",
        )
        .unwrap();
    }
    let run = |args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_fxi"))
            .args(args)
            .current_dir(root.path())
            .env("FXI_APP_DATA", indexes.path().join("app-data"))
            .env("FXI_INDEXES", indexes.path())
            .env("FXI_NEGATIVE_ROUTING", "1")
            .env("FXI_SOCKET", indexes.path().join("absent.sock"))
            .env("XDG_RUNTIME_DIR", indexes.path())
            .output()
            .unwrap()
    };
    let build = || {
        let output = run(&["index", "--force", "--chunk-size", "1", "."]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let search = |query| run(&["-l", "--color=never", query, "-p", "."]);
    build();
    assert!(find(indexes.path(), "negative-routing.bin").is_file());
    let empty = search("re:/abcd/");
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
    assert!(!search("re:/[/").status.success());
    let docs = find(indexes.path(), "docs.bin");
    let modified = fs::metadata(&docs).unwrap().modified().unwrap();
    let mut corrupt = fs::read(&docs).unwrap();
    corrupt[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&docs, corrupt).unwrap();
    fs::File::options()
        .write(true)
        .open(&docs)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(
        !search("re:/abcd/").status.success(),
        "negative proof must not hide core corruption"
    );
    build();
    fs::write(root.path().join("new.txt"), "abcd\n").unwrap();
    let updated = run(&["index", "."]);
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let output = search("re:/abcd/");
    assert!(output.status.success());
    let paths = String::from_utf8(output.stdout).unwrap();
    assert_eq!(paths.lines().count(), 1);
    assert_eq!(Path::new(paths.trim()).file_name().unwrap(), "new.txt");
}
