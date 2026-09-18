//! macOS executable boundaries: relocated installations, native watching and
//! process ownership must keep working after moving daemon code into `fxid`.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    dir: tempfile::TempDir,
    cli: PathBuf,
    helper: PathBuf,
    root: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new(with_helper: bool) -> Self {
        // Keep Unix socket paths below macOS's limit, including under CI.
        let dir = tempfile::Builder::new()
            .prefix("fxi-helper-")
            .tempdir_in("/tmp")
            .unwrap();
        let install = dir.path().join("installed binaries with spaces");
        fs::create_dir(&install).unwrap();
        let cli = install.join("fxi");
        let helper = install.join("fxid");
        fs::copy(env!("CARGO_BIN_EXE_fxi"), &cli).unwrap();
        if with_helper {
            fs::copy(env!("CARGO_BIN_EXE_fxid"), &helper).unwrap();
        }
        let root = dir.path().join("repository");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("initial.txt"), "initialHelperMarker\n").unwrap();
        let socket = dir.path().join("s.sock");
        Self {
            dir,
            cli,
            helper,
            root,
            socket,
        }
    }

    fn command_for(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .current_dir(&self.root)
            .env("FXI_APP_DATA", self.dir.path().join("data"))
            .env("FXI_INDEXES", self.dir.path().join("indexes"))
            .env("FXI_SOCKET", &self.socket)
            .env("NO_COLOR", "1");
        command
    }

    fn command(&self) -> Command {
        self.command_for(&self.cli)
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

    fn write_helper(&self, script: &str) {
        fs::write(&self.helper, script).unwrap();
        fs::set_permissions(&self.helper, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self
            .command()
            .args(["daemon", "stop", "--force"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct Foreground(Child);

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn missing_sibling_does_not_break_direct_search_or_run_a_path_helper() {
    let fixture = Fixture::new(false);
    fixture.ok(&["--version"]);
    fixture.ok(&["index", "--force"]);
    assert!(
        String::from_utf8_lossy(&fixture.ok(&["initialHelperMarker", "-l"]).stdout)
            .contains("initial.txt")
    );

    let path_dir = fixture.dir.path().join("path");
    fs::create_dir(&path_dir).unwrap();
    let marker = fixture.dir.path().join("wrong-helper-ran");
    let path_helper = path_dir.join("fxid");
    fs::write(
        &path_helper,
        "#!/bin/sh\nprintf wrong > \"$FXI_HELPER_MARKER\"\n",
    )
    .unwrap();
    fs::set_permissions(path_helper, fs::Permissions::from_mode(0o755)).unwrap();
    for action in ["start", "foreground"] {
        let output = fixture
            .command()
            .args(["daemon", action])
            .env("PATH", &path_dir)
            .env("FXI_HELPER_MARKER", &marker)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("Daemon helper is missing"), "{error}");
        assert!(error.contains(fixture.helper.to_str().unwrap()), "{error}");
        assert!(error.contains("cargo install --path ."), "{error}");
        assert!(!marker.exists());
    }
}

#[test]
fn relocated_native_helper_preserves_foreground_pid_watching_and_background_start() {
    let fixture = Fixture::new(true);
    fixture.ok(&["index", "--force"]);
    let mut daemon = Foreground(
        fixture
            .command()
            .args(["daemon", "foreground", "--watch"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(daemon.0.try_wait().unwrap().is_none());
        let output = fixture.run(&["daemon", "status"]);
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("Watching enabled: true")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "foreground helper never became ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let daemon_pid: u32 = fs::read_to_string(fixture.socket.with_extension("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        daemon_pid,
        daemon.0.id(),
        "foreground must replace the CLI process"
    );
    fixture.ok(&["initialHelperMarker", "-l"]);
    fs::write(fixture.root.join("final.txt"), "finalHelperMarker\n").unwrap();
    fixture.ok(&["daemon", "stop"]);
    assert!(daemon.0.wait().unwrap().success());
    assert!(!fixture.socket.with_extension("pid").exists());
    assert!(
        String::from_utf8_lossy(&fixture.ok(&["finalHelperMarker", "-l"]).stdout)
            .contains("final.txt")
    );

    fixture.ok(&["daemon", "start", "--watch"]);
    // Start must retain the CLI's readiness guarantee, including after relocation.
    let status = fixture.ok(&["daemon", "status"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("Watching enabled: true"));
    fixture.ok(&["finalHelperMarker", "-l"]);
    fixture.ok(&["daemon", "stop"]);
}

#[test]
fn helper_failures_propagate_and_foreground_preserves_exit_status_and_signals() {
    use std::os::unix::process::ExitStatusExt;

    let fixture = Fixture::new(false);
    fixture.write_helper("#!/bin/sh\nprintf 'helper refused launch\\n' >&2\nexit 37\n");
    let output = fixture.run(&["daemon", "start"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("helper refused launch"), "{error}");
    assert!(error.contains("37"), "{error}");
    assert_eq!(
        fixture.run(&["daemon", "foreground"]).status.code(),
        Some(37)
    );

    fixture.write_helper("#!/bin/sh\nkill -TERM \"$$\"\n");
    assert_eq!(
        fixture.run(&["daemon", "foreground"]).status.signal(),
        Some(libc::SIGTERM)
    );
    assert!(!fixture.socket.exists());
}

#[test]
fn mismatched_helper_versions_fail_before_creating_a_server() {
    let fixture = Fixture::new(true);
    for args in [
        ["--launcher-version", "different-release", "foreground"],
        ["--launcher-protocol", "0", "foreground"],
    ] {
        let output = fixture
            .command_for(&fixture.helper)
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("install both executables from the same release")
        );
        assert!(!fixture.socket.exists());
        assert!(!fixture.socket.with_extension("pid").exists());
    }
}
