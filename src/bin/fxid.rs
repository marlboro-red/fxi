//! Companion daemon executable. Public daemon commands remain `fxi daemon …`.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fxid",
    version,
    about = "FXI daemon helper; use `fxi daemon` to manage the server"
)]
struct Cli {
    #[arg(long, hide = true)]
    launcher_version: Option<String>,
    #[arg(long, hide = true)]
    launcher_protocol: Option<u32>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Start the daemon in the background
    #[cfg(unix)]
    Start {
        #[arg(long)]
        watch: bool,
    },
    /// Run the daemon in this process
    Foreground {
        #[arg(long)]
        watch: bool,
    },
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    anyhow::ensure!(
        cli.launcher_version
            .as_deref()
            .is_none_or(|version| version == env!("CARGO_PKG_VERSION"))
            && cli
                .launcher_protocol
                .is_none_or(|version| version == fxi::server::protocol::PROTOCOL_VERSION),
        "fxi and fxid versions differ; install both executables from the same release"
    );
    anyhow::ensure!(
        !fxi::server::is_daemon_running(),
        "Daemon is already running; stop it with `fxi daemon stop` first"
    );
    match cli.action {
        #[cfg(unix)]
        Action::Start { watch } => fxi::server::daemon::daemonize(watch),
        Action::Foreground { watch } => fxi::server::daemon::run_foreground(watch),
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
