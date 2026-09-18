//! Executable contracts: these assert expected answers, not two shared implementations.
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    indexes: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let indexes = dir.path().join("indexes");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("src-other")).unwrap();
        fs::write(
            root.join("src/a.txt"),
            "before\nAlpha beta\nalpha\nafter\nfoo/bar\nfoo-bar\n",
        )
        .unwrap();
        fs::write(root.join("src-other/b.txt"), "ALPHA\n").unwrap();
        #[cfg(unix)]
        let odd_name = "odd\nname.txt";
        #[cfg(not(unix))]
        let odd_name = "odd name.txt";
        fs::write(root.join(odd_name), "alpha\n").unwrap();
        for n in 0..20 {
            fs::write(root.join(format!("filler{n}.txt")), "unrelated\n").unwrap();
        }
        let this = Self { dir, root, indexes };
        this.ok(&["index", "--force"]);
        this
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fxi"));
        command
            .current_dir(&self.root)
            .env("FXI_INDEXES", &self.indexes)
            .env("FXI_SOCKET", self.dir.path().join("isolated.sock"))
            .env("NO_COLOR", "1");
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut command = self.command();
        let output = command.args(args).arg("--json").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn modes_scope_case_and_machine_output_have_explicit_contracts() {
    let f = Fixture::new();
    let rows = f.json(&["alpha", "src"]);
    assert_eq!(rows["matches"].as_array().unwrap().len(), 2);
    assert!(rows["matches"].as_array().unwrap().iter().all(|row| {
        std::path::Path::new(row["path"].as_str().unwrap())
            .ends_with(std::path::Path::new("src").join("a.txt"))
    }));
    let rows = f.json(&["alpha", "src/a.txt", "-w"]);
    assert_eq!(rows["matches"].as_array().unwrap().len(), 2);
    let rows = f.json(&["-e", "alpha", "-e", "missing", "-p", "src"]);
    assert_eq!(
        rows["matches"].as_array().unwrap().len(),
        2,
        "OR preserves bare-term case handling"
    );
    for mode in ["-F", "--regex"] {
        let rows = f.json(&[mode, "foo/bar", "src"]);
        assert_eq!(rows["matches"].as_array().unwrap().len(), 1);
    }
    let rows = f.json(&["-F", "Alpha beta", "src"]);
    assert_eq!(rows["matches"].as_array().unwrap().len(), 1);
    let rows = f.json(&["alpha alpha", "src", "-c"]);
    assert_eq!(rows["file_counts"][0][1], 2);
    let paths = f.ok(&["alpha", "-l", "-0"]).stdout;
    let paths: Vec<_> = paths
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .collect();
    assert_eq!(paths.len(), 3);
    #[cfg(unix)]
    assert!(paths.iter().any(|path| path.ends_with(b"odd\nname.txt")));
    #[cfg(not(unix))]
    assert!(paths.iter().any(|path| path.ends_with(b"odd name.txt")));
    assert!(f.ok(&["definitelyabsent"]).stdout.is_empty());
}

#[test]
fn context_is_unique_and_piped_output_is_stable() {
    let f = Fixture::new();
    let output = f.ok(&["alpha", "src", "-C", "1", "--color=never"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.lines().count(),
        4,
        "overlapping context must merge: {text}"
    );
    assert!(
        text.lines().all(|line| line.contains("a.txt")),
        "piped output always includes paths: {text}"
    );
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn bad_input_and_operational_errors_fail_without_launching_a_tui() {
    let f = Fixture::new();
    for args in [
        vec!["alpha", "-v"],
        vec!["size:nope"],
        vec!["alpha top:4"],
        vec!["--null", "alpha"],
        vec!["daemon", "reload"],
    ] {
        let output = f.run(&args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(!output.stderr.is_empty());
    }
    let output = f.run(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminal"));
    assert!(String::from_utf8_lossy(&f.ok(&["--version"]).stdout).starts_with("fxi "));
    let help = String::from_utf8(f.ok(&["--help"]).stdout).unwrap();
    assert!(
        help.contains("--fixed-strings") && help.contains("--regex") && help.contains("--json")
    );
    assert!(help.contains("--pattern <PATTERN>"));
    assert!(help.contains("--regexp is a legacy alias, not a regex-mode switch"));
}

#[cfg(unix)]
#[test]
fn daemon_mutations_are_visible_and_graceful_stop_persists_last_save() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    struct Daemon(std::process::Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let f = Fixture::new();
    let start = |watch: bool| {
        let mut command = f.command();
        command.args(["daemon", "foreground"]);
        if watch {
            command.arg("--watch");
        }
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut daemon = Daemon(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            let out = f.ok(&["daemon", "status"]);
            if String::from_utf8_lossy(&out.stdout).contains("Uptime:") {
                break;
            }
            assert!(Instant::now() < deadline, "daemon startup timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    };
    let mut daemon = start(false);
    {
        use fxi::server::protocol::{
            Request, Response, read_message_with_id, write_message_with_id,
        };
        let mut socket =
            std::os::unix::net::UnixStream::connect(f.dir.path().join("isolated.sock")).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        for query in [
            format!("{}alpha{}", "(".repeat(10_000), ")".repeat(10_000)),
            format!("^{}:alpha", "9".repeat(100)),
        ] {
            write_message_with_id(
                &mut socket,
                &Request::Search {
                    query,
                    root_path: Some(f.root.clone()),
                    limit: 10,
                },
                Some("invalid"),
            )
            .unwrap();
            let (response, id): (Response, _) = read_message_with_id(&mut socket).unwrap();
            assert!(matches!(response, Response::Error { .. }));
            assert_eq!(id.as_deref(), Some("invalid"));
        }
        write_message_with_id(&mut socket, &Request::Ping, Some("alive")).unwrap();
        let (response, _): (Response, _) = read_message_with_id(&mut socket).unwrap();
        assert!(matches!(response, Response::Pong));
    }
    assert!(
        !f.run(&["daemon", "start", "--watch"]).status.success(),
        "watch upgrade must not falsely succeed"
    );
    f.ok(&["alpha", "-l"]); // Warm the cached generation.
    fs::write(f.root.join("added.txt"), "uniqueNewMarker\n").unwrap();
    f.ok(&["index"]);
    let output = f.ok(&["uniqueNewMarker", "-l"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("added.txt"));
    assert!(
        output.stderr.is_empty(),
        "must not mask daemon failure with direct fallback"
    );
    f.ok(&["compact"]);
    assert!(!f.ok(&["uniqueNewMarker", "-l"]).stdout.is_empty());
    f.ok(&["remove", "."]);
    let status = String::from_utf8(f.ok(&["daemon", "status"]).stdout).unwrap();
    assert!(status.contains("Indexes loaded: 0"), "{status}");
    assert!(!f.run(&["uniqueNewMarker", "-l"]).status.success());
    f.ok(&["daemon", "stop"]);
    assert!(daemon.0.wait().unwrap().success());
    assert!(!f.dir.path().join("isolated.pid").exists());
    f.ok(&["index"]);
    let mut daemon = start(true);
    f.ok(&["alpha", "-l"]); // Starts and reconciles this root's watcher.
    fs::write(f.root.join("last-save.txt"), "lastSaveBeforeShutdown\n").unwrap();
    f.ok(&["daemon", "stop"]); // No waiting for a debounce or helper connection.
    assert!(daemon.0.wait().unwrap().success());
    let output = f.ok(&["lastSaveBeforeShutdown", "-l"]);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("last-save.txt"),
        "shutdown must persist the final save"
    );
}

#[cfg(unix)]
#[test]
fn closed_output_pipe_is_quiet_success_in_text_and_json_modes() {
    use std::{os::fd::FromRawFd, process::Stdio};
    let f = Fixture::new();
    for extra in [None, Some("--json")] {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        unsafe {
            libc::close(fds[0]);
        }
        let writer = unsafe { fs::File::from_raw_fd(fds[1]) };
        let mut command = f.command();
        command.arg("alpha").stdout(Stdio::from(writer));
        if let Some(arg) = extra {
            command.arg(arg);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn explicit_patterns_use_a_lone_positional_as_scope() {
    let f = Fixture::new();
    for args in [
        vec!["-e", "alpha", "src", "-l"],
        vec!["-e", "alpha", "-e", "missing", "src", "-l"],
        vec!["--pattern", "alpha", "src", "-l"],
        vec!["--regexp", "alpha", "src", "-l"],
        vec!["-e", "alpha", "src/a.txt", "-l"],
    ] {
        let rows = f.json(&args);
        let paths = rows["file_paths"].as_array().unwrap();
        assert_eq!(paths.len(), 1, "{args:?}: {rows}");
        assert!(
            std::path::Path::new(paths[0].as_str().unwrap())
                .ends_with(std::path::Path::new("src").join("a.txt"))
        );
    }
    assert_eq!(
        f.json(&["--regexp", "^Alpha", "src"])["matches"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "legacy --regexp alias retains query mode unless --regex is selected"
    );
    assert_eq!(
        f.json(&["--regex", "--pattern", "^Alpha", "src"])["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let missing = f.run(&["-e", "alpha", "nonexistent-directory"]);
    assert!(
        !missing.status.success(),
        "must not silently search a nonexistent scope as an OR term"
    );
    assert!(String::from_utf8_lossy(&missing.stderr).contains("nonexistent-directory"));
    // Unambiguous mixed legacy forms keep accepting a positional alternative.
    assert_eq!(
        f.json(&["missing", "-e", "alpha", "-p", "src", "-l"])["file_paths"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.json(&["missing", "-e", "alpha", "src", "-l"])["file_paths"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn search_flags_without_a_pattern_are_not_interactive_requests() {
    let f = Fixture::new();
    for args in [
        vec!["--json"],
        vec!["-l"],
        vec!["-c"],
        vec!["--regex"],
        vec!["-F"],
        vec!["-m", "0"],
        vec!["-C", "0"],
        vec!["--color", "auto"],
        vec!["--heading"],
    ] {
        let output = f.run(&args);
        assert!(!output.status.success(), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("pattern"), "{args:?}: {error}");
        assert!(
            !error.contains("Interactive search needs a terminal"),
            "{args:?}: {error}"
        );
        assert!(output.stdout.is_empty());
    }
    let output = f.run(&["search"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Interactive search needs a terminal"),
        "{error}"
    );
    assert!(
        output.stdout.is_empty(),
        "must not emit terminal control sequences"
    );
    let escaped = f.ok(&["--json", "--", "stats"]);
    assert!(
        serde_json::from_slice::<serde_json::Value>(&escaped.stdout).unwrap()["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !f.run(&["--json", "stats"]).status.success(),
        "search flags must not be silently ignored by subcommands"
    );
}

#[test]
fn missing_index_error_explains_how_to_create_it() {
    let f = Fixture::new();
    f.ok(&["remove", "."]);
    let output = f.run(&["alpha", "-l"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("fxi index"), "{error}");
    // Diagnostics use the resolved root. On Windows, canonicalization also
    // expands short path names and adds the verbatim-path prefix.
    let resolved_root = f.root.canonicalize().unwrap();
    assert!(error.contains(resolved_root.to_str().unwrap()), "{error}");
}

#[cfg(unix)]
#[test]
fn daemon_overload_never_bypasses_admission_with_direct_fallback() {
    use fxi::server::protocol::{
        Request, Response, SEARCH_OVERLOADED_PREFIX, read_message_with_id, write_message_with_id,
    };
    use std::os::unix::net::UnixListener;
    let fixture = Fixture::new();
    let listener = UnixListener::bind(fixture.dir.path().join("isolated.sock")).unwrap();
    let responder = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let (request, id): (Request, _) = read_message_with_id(&mut stream).unwrap();
        assert!(matches!(request, Request::ContentSearch { .. }));
        write_message_with_id(
            &mut stream,
            &Response::Error {
                message: format!(
                    "{SEARCH_OVERLOADED_PREFIX}Daemon search capacity exhausted; retry later"
                ),
            },
            id.as_deref(),
        )
        .unwrap();
    });
    let output = fixture.run(&["alpha", "--json"]);
    responder.join().unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "direct fallback would return matching indexed lines"
    );
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("capacity exhausted"), "{error}");
    assert!(!error.contains("falling back"), "{error}");
}
