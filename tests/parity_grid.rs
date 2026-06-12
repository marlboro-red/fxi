//! Table-driven parity grid: fxi vs ripgrep across query shapes × flag
//! combinations.
//!
//! Every entry states an explicit expectation:
//! - `Same(rg_args)`: fxi's matched-file set must equal ripgrep's
//! - `Diverges(rg_args, reason)`: a *documented* semantic divergence; the
//!   test asserts fxi's set is a superset or subset as stated by the reason,
//!   so accidental drift still fails
//!
//! The historical bugs this grid guards against all lived in flag
//! combinations: -i with a quoted phrase, -i with a regex, -l only being
//! optimized on one of the two execution paths.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static FIXTURE_DIR: OnceLock<PathBuf> = OnceLock::new();

fn fixture_dir() -> PathBuf {
    FIXTURE_DIR.get_or_init(create_fixtures).clone()
}

fn create_fixtures() -> PathBuf {
    let dir = std::env::temp_dir()
        .join("fxi_parity_fixtures")
        .join(format!("test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create fixture dir");

    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .output()
        .expect("git init");

    // Mixed-case identifiers and substring-inside-token cases
    fs::write(
        dir.join("mixed_case.rs"),
        r#"fn HandleError() {}
fn handle_error_retry() {}
const ERROR_CODE: i32 = 1;
fn no_match_here() {}
"#,
    )
    .unwrap();

    // Substring inside a larger token ("rintln" inside println/eprintln)
    fs::write(
        dir.join("substring.rs"),
        r#"fn main() {
    println!("hello, world");
    eprintln!("oops");
}
"#,
    )
    .unwrap();

    // Compound snake_case identifiers
    fs::write(
        dir.join("compound.rs"),
        r#"fn parse_query_filters() {}
fn parse_other() {}
static QUERY_FILTERS_MAX: usize = 8;
"#,
    )
    .unwrap();

    // Punctuation-heavy content for phrase/regex queries
    fs::write(
        dir.join("punct.cc"),
        r#"void f() {
    obj->method(arg);
    std::vector<int> v;
    if (x != y) { return; }
}
"#,
    )
    .unwrap();

    // Non-ASCII UTF-8 content
    fs::write(
        dir.join("unicode.rs"),
        "// gr\u{00fc}\u{00df}e from M\u{00fc}nchen\nfn unicode_marker() {}\n",
    )
    .unwrap();

    // Plain text with repeated words on one line (count semantics)
    fs::write(
        dir.join("notes.txt"),
        "alpha beta alpha\nbeta\ngamma alpha\n",
    )
    .unwrap();

    // Build the index
    let out = Command::new(fxi_binary())
        .args(["index", "--force"])
        .arg(&dir)
        .output()
        .expect("fxi index");
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    dir
}

fn fxi_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join(if cfg!(windows) { "fxi.exe" } else { "fxi" })
}

/// Files-with-matches set from fxi
fn fxi_files(args: &[&str], dir: &Path) -> HashSet<String> {
    let out = Command::new(fxi_binary())
        .args(["-l", "--color=never", "-p"])
        .arg(dir)
        .args(args)
        .output()
        .expect("run fxi");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect()
}

/// Files-with-matches set from ripgrep
fn rg_files(args: &[&str], dir: &Path) -> HashSet<String> {
    let out = Command::new("rg")
        .args(["-l", "--color=never"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run rg");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect()
}

enum Expect {
    /// fxi's file set must equal ripgrep's with these args
    Same(&'static [&'static str]),
    /// Documented divergence: fxi must be a strict-or-equal SUPERSET of rg
    /// (e.g. bare tokens are case-insensitive in fxi)
    SupersetOf(&'static [&'static str], &'static str),
}

struct Case {
    name: &'static str,
    fxi: &'static [&'static str],
    expect: Expect,
}

const GRID: &[Case] = &[
    // ── bare tokens ────────────────────────────────────────────────
    Case {
        name: "token, exact case",
        fxi: &["alpha"],
        expect: Expect::SupersetOf(
            &["-F", "alpha"],
            "bare token search is case-insensitive by default",
        ),
    },
    Case {
        name: "token equals rg -i",
        fxi: &["error"],
        expect: Expect::Same(&["-i", "-F", "error"]),
    },
    Case {
        name: "mixed-case query token",
        fxi: &["HandleError"],
        expect: Expect::Same(&["-i", "-F", "HandleError"]),
    },
    Case {
        name: "compound identifier",
        fxi: &["parse_query_filters"],
        expect: Expect::Same(&["-i", "-F", "parse_query_filters"]),
    },
    Case {
        name: "substring inside token",
        fxi: &["rintln"],
        expect: Expect::Same(&["-i", "-F", "rintln"]),
    },
    Case {
        name: "unicode token",
        fxi: &["unicode_marker"],
        expect: Expect::Same(&["-i", "-F", "unicode_marker"]),
    },
    // ── -i (must be a no-op relative to the CI default for tokens) ─
    Case {
        name: "-i token",
        fxi: &["-i", "error"],
        expect: Expect::Same(&["-i", "-F", "error"]),
    },
    Case {
        name: "-i mixed-case query",
        fxi: &["-i", "ERROR_CODE"],
        expect: Expect::Same(&["-i", "-F", "ERROR_CODE"]),
    },
    // ── phrases ────────────────────────────────────────────────────
    Case {
        name: "phrase, exact case",
        fxi: &["\"hello, world\""],
        expect: Expect::Same(&["-F", "hello, world"]),
    },
    Case {
        name: "phrase, no match wrong case",
        fxi: &["\"HELLO, WORLD\""],
        expect: Expect::Same(&["-F", "HELLO, WORLD"]),
    },
    Case {
        name: "-i phrase (regression: quotes were regex-escaped)",
        fxi: &["-i", "\"HELLO, WORLD\""],
        expect: Expect::Same(&["-i", "-F", "HELLO, WORLD"]),
    },
    Case {
        name: "phrase with punctuation",
        fxi: &["\"obj->method(arg)\""],
        expect: Expect::Same(&["-F", "obj->method(arg)"]),
    },
    Case {
        name: "phrase multi-word AND of files",
        fxi: &["\"alpha beta\""],
        expect: Expect::Same(&["-F", "alpha beta"]),
    },
    // ── regex ──────────────────────────────────────────────────────
    Case {
        name: "regex",
        fxi: &["re:/fn \\w+_retry/"],
        expect: Expect::Same(&["fn \\w+_retry"]),
    },
    Case {
        name: "-i regex (regression: -i was ignored for regex)",
        fxi: &["-i", "re:/handleerror/"],
        expect: Expect::Same(&["-i", "handleerror"]),
    },
    Case {
        name: "regex with punctuation class",
        fxi: &["re:/std::vector<\\w+>/"],
        expect: Expect::Same(&["std::vector<\\w+>"]),
    },
    // ── -w word boundary ───────────────────────────────────────────
    Case {
        name: "-w token",
        fxi: &["-w", "alpha"],
        expect: Expect::Same(&["-w", "-F", "alpha"]),
    },
    Case {
        name: "-w no partial match",
        fxi: &["-w", "rintln"],
        expect: Expect::Same(&["-w", "-F", "rintln"]),
    },
    Case {
        name: "-w -i combination",
        fxi: &["-w", "-i", "handleerror"],
        expect: Expect::Same(&["-w", "-i", "handleerror"]),
    },
    // ── multiple patterns (-e OR) ──────────────────────────────────
    Case {
        name: "-e multi-pattern OR",
        fxi: &["-e", "gamma", "-e", "unicode_marker"],
        expect: Expect::Same(&["-e", "gamma", "-e", "unicode_marker", "-i"]),
    },
];

fn run_grid(fxi_files: impl Fn(&[&str], &Path) -> HashSet<String>, dir: &Path) -> Vec<String> {
    let mut failures = Vec::new();

    for case in GRID {
        let fxi = fxi_files(case.fxi, dir);
        match &case.expect {
            Expect::Same(rg_args) => {
                let rg = rg_files(rg_args, dir);
                if fxi != rg {
                    failures.push(format!(
                        "[{}] fxi {:?} != rg {:?}\n  fxi: {:?}\n  rg:  {:?}",
                        case.name, case.fxi, rg_args, fxi, rg
                    ));
                }
            }
            Expect::SupersetOf(rg_args, reason) => {
                let rg = rg_files(rg_args, dir);
                if !rg.is_subset(&fxi) {
                    failures.push(format!(
                        "[{}] fxi {:?} must be a superset of rg {:?} ({})\n  fxi: {:?}\n  rg:  {:?}",
                        case.name, case.fxi, rg_args, reason, fxi, rg
                    ));
                }
            }
        }
    }

    failures
}

#[test]
fn parity_grid_direct() {
    let dir = fixture_dir();
    let failures = run_grid(fxi_files, &dir);
    assert!(
        failures.is_empty(),
        "{} parity failures (direct path):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Daemon guard: stops the daemon when dropped, even on test failure
#[cfg(unix)]
struct DaemonGuard {
    env: Vec<(String, String)>,
}

#[cfg(unix)]
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let mut cmd = Command::new(fxi_binary());
        cmd.args(["daemon", "stop"]);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let _ = cmd.output();
    }
}

/// The same grid through a running daemon: the direct and daemon execution
/// paths have diverged before (-l fast path, (?i) wrapping); this keeps them
/// honest against each other.
#[cfg(unix)]
#[test]
fn parity_grid_daemon() {
    let dir = fixture_dir();

    let run_dir = std::env::temp_dir().join(format!("fxi_parity_daemon_{}", std::process::id()));
    fs::create_dir_all(&run_dir).unwrap();
    let env: Vec<(String, String)> = vec![
        (
            "FXI_SOCKET".into(),
            run_dir.join("fxi.sock").to_string_lossy().into_owned(),
        ),
        (
            "XDG_RUNTIME_DIR".into(),
            run_dir.to_string_lossy().into_owned(),
        ),
    ];

    let with_env = |cmd: &mut Command, env: &[(String, String)]| {
        for (k, v) in env {
            cmd.env(k, v);
        }
    };

    let _guard = DaemonGuard { env: env.clone() };

    let mut start = Command::new(fxi_binary());
    start.args(["daemon", "start"]);
    with_env(&mut start, &env);
    let out = start.output().expect("daemon start");
    assert!(
        out.status.success(),
        "daemon start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let env_query = env.clone();
    let failures = run_grid(
        move |args, dir| {
            let mut cmd = Command::new(fxi_binary());
            cmd.args(["-l", "--color=never", "-p"]).arg(dir).args(args);
            with_env(&mut cmd, &env_query);
            let out = cmd.output().expect("run fxi via daemon");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !stderr.contains("falling back"),
                "query silently fell back to the direct path: {}",
                stderr
            );
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.replace('\\', "/"))
                .collect()
        },
        &dir,
    );

    assert!(
        failures.is_empty(),
        "{} parity failures (daemon path):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
