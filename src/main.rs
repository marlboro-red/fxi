mod index;
mod output;
mod query;
mod server;
mod tui;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ColorChoice {
    /// Always use colors
    Always,
    /// Never use colors
    Never,
    /// Auto-detect based on terminal
    #[default]
    Auto,
}

#[derive(Parser)]
#[command(name = "fxi")]
#[command(about = "Terminal-first, ultra-fast code search engine")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Search pattern (when no subcommand is given)
    pattern: Option<String>,

    /// Additional patterns to search for (-e, can be repeated)
    #[arg(short = 'e', long = "regexp", action = clap::ArgAction::Append)]
    patterns: Vec<String>,

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

    /// Invert match: show non-matching lines (-v)
    #[arg(short = 'v', long)]
    invert_match: bool,

    /// Match whole words only (-w)
    #[arg(short = 'w', long)]
    word_regexp: bool,

    /// Maximum number of results (-m), 0 for unlimited
    #[arg(short = 'm', long, default_value = "0")]
    max_count: usize,

    /// Only print filenames (-l)
    #[arg(short = 'l', long)]
    files_with_matches: bool,

    /// Print match count per file (-c)
    #[arg(short = 'c', long)]
    count: bool,

    /// When to use colors: always, never, auto
    #[arg(long, default_value = "auto", value_enum)]
    color: ColorChoice,
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

        /// Files per chunk (0 = all in one chunk)
        #[arg(long)]
        chunk_size: Option<usize>,
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
    Start {
        /// Enable file watching for automatic index updates
        #[arg(long)]
        watch: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Check daemon status
    Status,
    /// Run daemon in foreground (for debugging)
    Foreground {
        /// Enable file watching for automatic index updates
        #[arg(long)]
        watch: bool,
    },
    /// Reload index for a path
    Reload {
        /// Path to the codebase to reload
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// Options for grep-style content search (ripgrep-compatible)
struct GrepOptions {
    patterns: Vec<String>,
    path: PathBuf,
    after_context: u32,
    before_context: u32,
    context: Option<u32>,
    ignore_case: bool,
    invert_match: bool,
    word_regexp: bool,
    max_count: usize,
    files_with_matches: bool,
    count: bool,
    color: ColorChoice,
}

impl GrepOptions {
    fn from_cli(cli: &Cli) -> Self {
        let mut patterns = cli.patterns.clone();
        if let Some(ref p) = cli.pattern {
            patterns.insert(0, p.clone());
        }

        Self {
            patterns,
            path: cli.path.clone(),
            after_context: cli.after_context,
            before_context: cli.before_context,
            context: cli.context,
            ignore_case: cli.ignore_case,
            invert_match: cli.invert_match,
            word_regexp: cli.word_regexp,
            max_count: cli.max_count,
            files_with_matches: cli.files_with_matches,
            count: cli.count,
            color: cli.color,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Index { path, force, chunk_size }) => {
            // Auto-detect codebase root
            index::build::build_index_auto(&path, force, chunk_size)?;
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
            let opts = GrepOptions::from_cli(&cli);

            if !opts.patterns.is_empty() {
                // Direct content search (ripgrep-like)
                handle_grep_command(opts)?;
            } else {
                // Interactive TUI mode
                tui::run(cli.path, None)?;
            }
        }
    }

    Ok(())
}

fn handle_daemon_command(action: DaemonAction) -> Result<()> {
    use server::{is_daemon_running, IndexClient};

    match action {
        DaemonAction::Start { watch } => {
            if is_daemon_running() {
                println!("Daemon is already running");
                return Ok(());
            }

            println!("Starting fxid daemon...");
            server::daemon::daemonize(watch)?;

            // Wait a moment for daemon to start
            std::thread::sleep(std::time::Duration::from_millis(500));

            if is_daemon_running() {
                #[cfg(unix)]
                println!("Daemon started (socket: {})", server::get_socket_path().display());
                #[cfg(windows)]
                println!("Daemon started (pipe: {})", server::get_pipe_name());
            } else {
                #[cfg(unix)]
                println!("Daemon may have failed to start. Check /tmp/fxid-error.log");
                #[cfg(windows)]
                println!("Daemon may have failed to start.");
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

        DaemonAction::Foreground { watch } => {
            if is_daemon_running() {
                println!("Daemon is already running in background. Stop it first with 'fxi daemon stop'");
                return Ok(());
            }

            println!("Running daemon in foreground (Ctrl+C to stop)...");
            server::daemon::run_foreground(watch)?;
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

fn handle_grep_command(opts: GrepOptions) -> Result<()> {
    use server::protocol::ContentSearchOptions;
    use std::io::IsTerminal;

    // -v (invert match) is not supported with indexed search
    if opts.invert_match {
        anyhow::bail!("--invert-match (-v) is not supported: indexed search only returns matching lines");
    }

    // Find codebase root
    let root = utils::find_codebase_root(&opts.path)?;

    // Build combined pattern for multiple -e flags (OR them together)
    let combined_pattern = build_pattern(&opts.patterns, opts.ignore_case, opts.word_regexp);

    // Resolve context flags (-C overrides -A and -B)
    let (ctx_before, ctx_after) = if let Some(c) = opts.context {
        (c, c)
    } else {
        (opts.before_context, opts.after_context)
    };

    let search_options = ContentSearchOptions {
        context_before: ctx_before,
        context_after: ctx_after,
        case_insensitive: opts.ignore_case,
        files_only: opts.files_with_matches,  // Optimize for -l mode
    };

    // Try to use daemon for warm search
    let matches = if let Some(mut client) = server::IndexClient::connect() {
        match client.content_search(&combined_pattern, &root, opts.max_count, search_options) {
            Ok(response) => response.matches,
            Err(e) => {
                eprintln!("Daemon search failed, falling back to direct search: {}", e);
                do_direct_content_search(&combined_pattern, &root, opts.max_count, ctx_before, ctx_after, opts.ignore_case, opts.word_regexp)?
            }
        }
    } else {
        // Fall back to direct search without daemon
        do_direct_content_search(&combined_pattern, &root, opts.max_count, ctx_before, ctx_after, opts.ignore_case, opts.word_regexp)?
    };

    // Output results
    let color = match opts.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => std::io::stdout().is_terminal(),
    };
    // Use heading style when results span multiple files
    let use_heading = matches.iter().map(|m| &m.path).collect::<std::collections::HashSet<_>>().len() > 1;

    if opts.files_with_matches {
        output::print_files_only(&matches, color)?;
    } else if opts.count {
        output::print_match_counts(&matches, color)?;
    } else {
        output::print_content_matches(&matches, color, use_heading)?;
    }

    Ok(())
}

/// Build combined search pattern from multiple patterns
fn build_pattern(patterns: &[String], ignore_case: bool, word_regexp: bool) -> String {
    if patterns.is_empty() {
        return String::new();
    }

    // For single pattern without special flags, return as-is
    if patterns.len() == 1 && !ignore_case && !word_regexp {
        return patterns[0].clone();
    }

    // Build regex pattern for -w (word boundary) and/or multiple patterns
    let escaped: Vec<String> = patterns
        .iter()
        .map(|p| {
            let escaped = regex::escape(p);
            if word_regexp {
                format!(r"\b{}\b", escaped)
            } else {
                escaped
            }
        })
        .collect();

    // Join multiple patterns with OR
    let combined = if escaped.len() > 1 {
        escaped.join("|")
    } else {
        escaped.into_iter().next().unwrap_or_default()
    };

    // Wrap in regex syntax with case flag if needed
    if ignore_case || word_regexp || patterns.len() > 1 {
        if ignore_case {
            format!("re:/(?i){}/", combined)
        } else {
            format!("re:/{}/", combined)
        }
    } else {
        combined
    }
}

/// Direct content search without daemon
fn do_direct_content_search(
    pattern: &str,
    root: &Path,
    limit: usize,
    context_before: u32,
    context_after: u32,
    case_insensitive: bool,
    word_regexp: bool,
) -> Result<Vec<server::protocol::ContentMatch>> {
    use crate::index::reader::IndexReader;
    use crate::query::{parse_query, QueryExecutor};

    // Load index
    let reader = IndexReader::open(root)?;

    // Pattern is already built with appropriate regex wrapping
    // Only add case-insensitive wrapper if not already a regex pattern
    let query_str = if case_insensitive && !pattern.starts_with("re:/") && !word_regexp {
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

    // Convert to protocol type and apply limit (0 = unlimited)
    let iter = matches.into_iter();
    let limited: Box<dyn Iterator<Item = _>> = if limit == 0 {
        Box::new(iter)
    } else {
        Box::new(iter.take(limit))
    };
    let result: Vec<server::protocol::ContentMatch> = limited
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
