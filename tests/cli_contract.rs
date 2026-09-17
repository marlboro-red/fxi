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
        fs::write(root.join("odd\nname.txt"), "alpha\n").unwrap();
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
    assert!(
        rows["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["path"].as_str().unwrap().ends_with("src/a.txt"))
    );
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
    assert!(paths.iter().any(|path| path.ends_with(b"odd\nname.txt")));
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
