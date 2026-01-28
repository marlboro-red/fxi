mod index;
mod query;
mod server;
mod tui;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "fxi")]
#[command(about = "Terminal-first, ultra-fast code search engine")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Search query (when no subcommand is given)
    #[arg(trailing_var_arg = true)]
    query: Vec<String>,

    /// Path to search in
    #[arg(short, long, default_value = ".")]
    path: PathBuf,
}

#[derive(Subcommand)]
enum Commands {
    /// Build or rebuild the index
    Index {
        /// Path to index (auto-detects git root)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Force full rebuild
        #[arg(short, long)]
        force: bool,
    },
    /// Search the index (interactive TUI mode)
    Search {
        /// Initial query
        query: Option<String>,

        /// Path to search in
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Show index statistics
    Stats {
        /// Path to index
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Compact delta segments
    Compact {
        /// Path to index
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List all indexed codebases
    List,
    /// Remove an index
    Remove {
        /// Path to the codebase to remove index for
        path: PathBuf,
    },
    /// Start the index server daemon (keeps indexes warm for fast searches)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon in background
    Start,
    /// Stop the running daemon
    Stop,
    /// Check daemon status
    Status,
    /// Run daemon in foreground (for debugging)
    Foreground,
    /// Reload index for a path
    Reload {
        /// Path to the codebase to reload
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Index { path, force }) => {
            // Auto-detect codebase root
            index::build::build_index_auto(&path, force)?;
        }
        Some(Commands::Search { query, path }) => {
            tui::run(path, query)?;
        }
        Some(Commands::Stats { path }) => {
            index::stats::show_stats(&path)?;
        }
        Some(Commands::Compact { path }) => {
            index::compact::compact_segments(&path)?;
        }
        Some(Commands::List) => {
            index::stats::list_indexes()?;
        }
        Some(Commands::Remove { path }) => {
            let root = utils::find_codebase_root(&path)?;
            utils::remove_index(&root)?;
            println!("Removed index for: {}", root.display());
        }
        Some(Commands::Daemon { action }) => {
            handle_daemon_command(action)?;
        }
        None => {
            if cli.query.is_empty() {
                // Interactive mode
                tui::run(cli.path, None)?;
            } else {
                // Direct query mode
                let query_str = cli.query.join(" ");
                tui::run(cli.path, Some(query_str))?;
            }
        }
    }

    Ok(())
}

fn handle_daemon_command(action: DaemonAction) -> Result<()> {
    use server::{get_socket_path, is_daemon_running, IndexClient};

    match action {
        DaemonAction::Start => {
            if is_daemon_running() {
                println!("Daemon is already running");
                return Ok(());
            }

            println!("Starting fxid daemon...");
            server::daemon::daemonize()?;

            // Wait a moment for daemon to start
            std::thread::sleep(std::time::Duration::from_millis(500));

            if is_daemon_running() {
                println!("Daemon started (socket: {})", get_socket_path().display());
            } else {
                println!("Daemon may have failed to start. Check /tmp/fxid-error.log");
            }
        }

        DaemonAction::Stop => {
            if !is_daemon_running() {
                println!("Daemon is not running");
                return Ok(());
            }

            println!("Stopping daemon...");

            // Try graceful shutdown via client first
            if let Some(mut client) = IndexClient::connect() {
                let _ = client.shutdown();
                std::thread::sleep(std::time::Duration::from_millis(500));
            }

            // Force stop if still running
            if is_daemon_running() {
                server::daemon::stop_daemon()?;
            }

            println!("Daemon stopped");
        }

        DaemonAction::Status => {
            if !is_daemon_running() {
                println!("Daemon is not running");
                return Ok(());
            }

            match IndexClient::connect() {
                Some(mut client) => {
                    match client.status() {
                        Ok(status) => {
                            println!("fxid daemon status:");
                            println!("  Uptime: {}s", status.uptime_secs);
                            println!("  Indexes loaded: {}", status.indexes_loaded);
                            println!("  Total documents: {}", status.total_docs);
                            println!("  Queries served: {}", status.queries_served);
                            println!("  Cache hit rate: {:.1}%", status.cache_hit_rate * 100.0);
                            println!("  Memory (approx): {:.1} MB", status.memory_bytes as f64 / 1024.0 / 1024.0);
                            if !status.loaded_roots.is_empty() {
                                println!("  Loaded codebases:");
                                for root in &status.loaded_roots {
                                    println!("    - {}", root.display());
                                }
                            }
                        }
                        Err(e) => {
                            println!("Failed to get status: {}", e);
                        }
                    }
                }
                None => {
                    println!("Daemon is running but not responding");
                }
            }
        }

        DaemonAction::Foreground => {
            if is_daemon_running() {
                println!("Daemon is already running in background. Stop it first with 'fxi daemon stop'");
                return Ok(());
            }

            println!("Running daemon in foreground (Ctrl+C to stop)...");
            server::daemon::run_foreground()?;
        }

        DaemonAction::Reload { path } => {
            let root = utils::find_codebase_root(&path)?;

            if !is_daemon_running() {
                println!("Daemon is not running. Start it with 'fxi daemon start'");
                return Ok(());
            }

            match IndexClient::connect() {
                Some(mut client) => {
                    match client.reload(&root) {
                        Ok((success, message)) => {
                            if success {
                                println!("Reloaded: {}", message);
                            } else {
                                println!("Reload failed: {}", message);
                            }
                        }
                        Err(e) => {
                            println!("Failed to reload: {}", e);
                        }
                    }
                }
                None => {
                    println!("Failed to connect to daemon");
                }
            }
        }
    }

    Ok(())
}
