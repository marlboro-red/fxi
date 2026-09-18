//! Launch the packaged daemon helper without loading its frameworks in clients.

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

pub use super::control_unix::stop_daemon;

fn helper_command(action: &str, watch: bool) -> Result<(PathBuf, Command)> {
    let helper = std::env::current_exe()
        .context("Cannot locate the current fxi executable")?
        .with_file_name("fxid");
    anyhow::ensure!(
        helper.is_file(),
        "Daemon helper is missing at {}. Install fxi and fxid together from the same release; for a source installation, run `cargo install --path .`, or build both in the same profile with `cargo build --bins` (add `--release` for release builds)",
        helper.display()
    );
    let mut command = Command::new(&helper);
    command
        .arg("--launcher-version")
        .arg(env!("CARGO_PKG_VERSION"))
        .arg("--launcher-protocol")
        .arg(super::protocol::PROTOCOL_VERSION.to_string())
        .arg(action);
    if watch {
        command.arg("--watch");
    }
    Ok((helper, command))
}

/// The helper uses the existing double-fork implementation. The CLI retains its
/// readiness handshake, so success still means a responding daemon is running.
pub fn daemonize(watch: bool) -> Result<()> {
    let (helper, mut command) = helper_command("start", watch)?;
    let status = command
        .status()
        .with_context(|| format!("Could not launch daemon helper at {}", helper.display()))?;
    anyhow::ensure!(
        status.success(),
        "Daemon helper at {} exited with {status}; run `fxi daemon foreground` to inspect the error",
        helper.display()
    );
    Ok(())
}

/// Replace the foreground CLI so its PID, terminal and signals belong to the
/// daemon, just as they do when the daemon is compiled into the CLI.
pub fn run_foreground(watch: bool) -> Result<()> {
    let (helper, mut command) = helper_command("foreground", watch)?;
    Err(command.exec())
        .with_context(|| format!("Could not execute daemon helper at {}", helper.display()))
}
