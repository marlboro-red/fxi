mod index;
mod output;
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

    /// Search pattern (when no subcommand is given)
    pattern: Option<String>,

    /// Path to search in
    #[arg(short, long, default_value = ".")]
    path: PathBuf,

    /// Lines of context after match (-A)
    #[arg(short = 'A', long, default_value = "0")]
    after_context: u32,

    /// Lines of context before match (-B)
    #[arg(short = 'B', long, default_value = "0")]
    before_context: u32,

    /// Lines of context (both directions, -C)
    #[arg(short = 'C', long)]
    context: Option<u32>,

    /// Case insensitive search (-i)
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// Maximum results
    #[arg(short = 'n', long, default_value = "100")]
    max_count: usize,

    /// Only print filenames (-l)
    #[arg(short = 'l', long)]
    files_with_matches: bool,

    /// Print match count per file (-c)
    #[arg(short = 'c', long)]
    count: bool,

    /// Disable colored output
    #[arg(long)]
    no_color: bool,
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
    /// Interactive search TUI
    Search {
        /// Path to search in
        #[arg(default_value = ".")]
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
        Some(Commands::Search { path }) => {
            tui::run(path, None)?;
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
            if let Some(pattern) = cli.pattern {
                // Direct content search (ripgrep-like)
                handle_grep_command(
                    pattern,
                    cli.path,
                    cli.after_context,
                    cli.before_context,
                    cli.context,
                    cli.ignore_case,
                    cli.max_count,
                    cli.files_with_matches,
                    cli.count,
                    cli.no_color,
                )?;
            } else {
                // Interactive TUI mode
                tui::run(cli.path, None)?;
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

#[allow(clippy::too_many_arguments)]
fn handle_grep_command(
    pattern: String,
    path: PathBuf,
    after_context: u32,
    before_context: u32,
    context: Option<u32>,
    ignore_case: bool,
    max_count: usize,
    files_with_matches: bool,
    count: bool,
    no_color: bool,
) -> Result<()> {
    use server::protocol::ContentSearchOptions;

    // Find codebase root
    let root = utils::find_codebase_root(&path)?;

    // Resolve context flags (-C overrides -A and -B)
    let (ctx_before, ctx_after) = if let Some(c) = context {
        (c, c)
    } else {
        (before_context, after_context)
    };

    let options = ContentSearchOptions {
        context_before: ctx_before,
        context_after: ctx_after,
        case_insensitive: ignore_case,
    };

    // Try to use daemon for warm search
    let matches = if let Some(mut client) = server::IndexClient::connect() {
        match client.content_search(&pattern, &root, max_count, options) {
            Ok(response) => response.matches,
            Err(e) => {
                eprintln!("Daemon search failed, falling back to direct search: {}", e);
                do_direct_content_search(&pattern, &root, max_count, ctx_before, ctx_after, ignore_case)?
            }
        }
    } else {
        // Fall back to direct search without daemon
        do_direct_content_search(&pattern, &root, max_count, ctx_before, ctx_after, ignore_case)?
    };

    // Output results
    let color = !no_color;
    // Use heading style when results span multiple files
    let use_heading = matches.iter().map(|m| &m.path).collect::<std::collections::HashSet<_>>().len() > 1;

    if files_with_matches {
        output::print_files_only(&matches)?;
    } else if count {
        output::print_match_counts(&matches)?;
    } else {
        output::print_content_matches(&matches, color, use_heading)?;
    }

    Ok(())
}

/// Direct content search without daemon
fn do_direct_content_search(
    pattern: &str,
    root: &PathBuf,
    limit: usize,
    context_before: u32,
    context_after: u32,
    case_insensitive: bool,
) -> Result<Vec<server::protocol::ContentMatch>> {
    use crate::index::reader::IndexReader;
    use crate::query::{parse_query, QueryExecutor};

    // Load index
    let reader = IndexReader::open(root)?;

    // Build query - handle case insensitivity
    let query_str = if case_insensitive {
        format!("re:/(?i){}/", regex::escape(pattern))
    } else {
        pattern.to_string()
    };

    let parsed = parse_query(&query_str);
    if parsed.is_empty() {
        return Ok(Vec::new());
    }

    let executor = QueryExecutor::new(&reader);
    let matches = executor.execute_with_content(&parsed, context_before, context_after)?;

    // Convert to protocol type and apply limit
    let result: Vec<server::protocol::ContentMatch> = matches
        .into_iter()
        .take(limit)
        .map(|m| server::protocol::ContentMatch {
            path: m.path,
            line_number: m.line_number,
            line_content: m.line_content,
            match_start: m.match_start,
            match_end: m.match_end,
            context_before: m.context_before,
            context_after: m.context_after,
        })
        .collect();

    Ok(result)
}
