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
#[command(
    name = "fxi",
    version,
    after_help = "Examples:\n  fxi error                 Case-insensitive substring\n  fxi 'foo bar'             Both terms in the same file\n  fxi '\"foo bar\"'         Exact phrase\n  fxi --regex 'foo.*bar'    Regular expression\n  fxi -F 'foo-bar'          Literal text\n  fxi -l error src           Restrict results to src\n  fxi -F -- -excluded        Search literal leading punctuation\n\nNo matches exits successfully (0); invalid input and operation failures are errors."
)]
#[command(about = "Terminal-first, ultra-fast code search engine")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Search pattern (when no subcommand is given)
    pattern: Option<String>,

    /// Alternative patterns in the selected mode (-e, repeat for OR)
    #[arg(short = 'e', long = "regexp", action = clap::ArgAction::Append)]
    patterns: Vec<String>,

    /// Optional file or directory to restrict the search to
    search_path: Option<PathBuf>,

    /// Treat patterns as literal text (case-sensitive unless -i)
    #[arg(short = 'F', long, conflicts_with = "regex")]
    fixed_strings: bool,

    /// Treat patterns as regular expressions (case-sensitive unless -i)
    #[arg(long)]
    regex: bool,

    /// Emit structured JSON instead of terminal text
    #[arg(long, conflicts_with = "null")]
    json: bool,

    /// Terminate filenames with NUL (requires -l)
    #[arg(short = '0', long, requires = "files_with_matches")]
    null: bool,

    /// Group matching lines under filename headings
    #[arg(long, conflicts_with = "no_heading")]
    heading: bool,

    /// Always print path:line:text, including in a terminal
    #[arg(long)]
    no_heading: bool,

    /// File or directory to restrict the search to (index root is detected separately)
    #[arg(short, long, default_value = ".", conflicts_with = "search_path")]
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

    /// Unsupported: inverse line matching is not available with indexed search
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
    Stop {
        /// Terminate immediately; pending watcher changes may be lost
        #[arg(long)]
        force: bool,
    },
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
    /// Print the daemon socket/pipe path
    SocketPath,
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
    fixed_strings: bool,
    regex: bool,
    json: bool,
    null: bool,
    heading: bool,
    no_heading: bool,
}

impl GrepOptions {
    fn from_cli(cli: &Cli) -> Self {
        let mut patterns = cli.patterns.clone();
        if let Some(ref p) = cli.pattern {
            patterns.insert(0, p.clone());
        }

        Self {
            patterns,
            path: cli.search_path.clone().unwrap_or_else(|| cli.path.clone()),
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
            fixed_strings: cli.fixed_strings,
            regex: cli.regex,
            json: cli.json,
            null: cli.null,
            heading: cli.heading,
            no_heading: cli.no_heading,
        }
    }
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) if is_broken_pipe(&error) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
        || error
            .downcast_ref::<serde_json::Error>()
            .is_some_and(|e| e.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe))
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Index {
            path,
            force,
            chunk_size,
        }) => {
            // Auto-detect codebase root. The write lock serializes against
            // a daemon flush or another fxi index on the same root.
            let root = utils::find_codebase_root(&path)?;
            let _lock = utils::IndexLock::acquire(&root)?;
            index::build::build_index_auto(&path, force, chunk_size)?;
            drop(_lock);
            reload_running_daemon(&root)?;
        }
        Some(Commands::Search { path }) => {
            tui::run(path, None)?;
        }
        Some(Commands::Stats { path }) => {
            index::stats::show_stats(&path)?;
        }
        Some(Commands::Compact { path }) => {
            let root = utils::find_codebase_root(&path)?;
            let _lock = utils::IndexLock::acquire(&root)?;
            index::compact::compact_segments(&path)?;
            drop(_lock);
            reload_running_daemon(&root)?;
        }
        Some(Commands::List) => {
            index::stats::list_indexes()?;
        }
        Some(Commands::Remove { path }) => {
            let root = utils::find_codebase_root(&path)?;
            if let Some(mut client) = server::IndexClient::connect() {
                client.remove(&root)?;
            } else {
                let _lock = utils::IndexLock::acquire(&root)?;
                utils::remove_index(&root)?;
            }
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
                use std::io::IsTerminal;
                anyhow::ensure!(
                    std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                    "Interactive search needs a terminal. Supply a pattern, or run `fxi --help`; stdin search is not supported"
                );
                tui::run(cli.path, None)?;
            }
        }
    }

    Ok(())
}

fn reload_running_daemon(root: &std::path::Path) -> Result<()> {
    if let Some(mut client) = server::IndexClient::connect() {
        let (success, message) = client.reload(Some(root))?;
        anyhow::ensure!(
            success,
            "Index published, but daemon reload failed: {message}"
        );
    } else {
        anyhow::ensure!(
            !server::is_daemon_running(),
            "Index published, but the running daemon is unresponsive; restart it before searching"
        );
    }
    Ok(())
}

fn handle_daemon_command(action: DaemonAction) -> Result<()> {
    use server::{IndexClient, is_daemon_running};
    use std::time::{Duration, Instant};
    match action {
        DaemonAction::Start { watch } => {
            if is_daemon_running() {
                let mut client = IndexClient::connect()
                    .ok_or_else(|| anyhow::anyhow!("Daemon is running but not responding"))?;
                let status = client.status()?;
                anyhow::ensure!(
                    !watch || status.watch_enabled,
                    "Daemon is running without watching; run `fxi daemon stop` then `fxi daemon start --watch`"
                );
                println!(
                    "Daemon is already running (watching: {})",
                    status.watch_enabled
                );
                return Ok(());
            }
            server::daemon::daemonize(watch)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(mut client) = IndexClient::connect()
                    && client.status().is_ok()
                {
                    println!("Daemon started (watching: {watch})");
                    break;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Daemon did not become ready within 10 seconds; run `fxi daemon foreground` to inspect the error"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        DaemonAction::Stop { force } => {
            if !is_daemon_running() {
                println!("Daemon is not running");
                return Ok(());
            }
            if force {
                server::daemon::stop_daemon()?;
            } else {
                let mut client = IndexClient::connect().ok_or_else(|| anyhow::anyhow!("Daemon is unresponsive; use `fxi daemon stop --force` to terminate without flushing pending changes"))?;
                client.shutdown()?;
                let deadline = Instant::now() + Duration::from_secs(5);
                while is_daemon_running() {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "Daemon acknowledged shutdown but has not exited; use `fxi daemon stop --force` if necessary"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            println!("Daemon stopped");
        }
        DaemonAction::Status => {
            if !is_daemon_running() {
                println!("Daemon is not running");
                return Ok(());
            }
            let mut client = IndexClient::connect()
                .ok_or_else(|| anyhow::anyhow!("Daemon is running but not responding"))?;
            let status = client.status()?;
            println!("fxid daemon status:");
            println!("  Uptime: {}s", status.uptime_secs);
            println!("  Watching enabled: {}", status.watch_enabled);
            println!("  Indexes loaded: {}", status.indexes_loaded);
            println!("  Live documents: {}", status.total_docs);
            println!("  Queries served: {}", status.queries_served);
            println!("  Protocol version: {}", status.protocol_version);
            println!("  Server version: {}", status.server_version);
            for root in &status.loaded_roots {
                println!(
                    "  {} (watching: {})",
                    root.display(),
                    status.watched_roots.contains(root)
                );
            }
        }
        DaemonAction::Foreground { watch } => {
            anyhow::ensure!(
                !is_daemon_running(),
                "Daemon is already running; stop it with `fxi daemon stop` first"
            );
            server::daemon::run_foreground(watch)?;
        }
        DaemonAction::SocketPath => {
            #[cfg(unix)]
            println!("{}", server::get_socket_path().display());
            #[cfg(windows)]
            println!("{}", server::get_pipe_name());
        }
        DaemonAction::Reload { path } => {
            let root = utils::find_codebase_root(&path)?;
            let mut client = IndexClient::connect().ok_or_else(|| {
                anyhow::anyhow!("Daemon is not responding; start it with `fxi daemon start`")
            })?;
            let (success, message) = client.reload(Some(&root))?;
            anyhow::ensure!(success, "Reload failed: {message}");
            println!("Reloaded: {message}");
        }
    }
    Ok(())
}

fn handle_grep_command(opts: GrepOptions) -> Result<()> {
    use server::protocol::{ContentSearchOptions, ContentSearchResponse};
    use std::io::{IsTerminal, Write};
    anyhow::ensure!(
        !opts.invert_match,
        "--invert-match (-v) is not supported: indexed search only returns matching lines"
    );
    let requested = opts.path.canonicalize()?;
    let root = utils::find_codebase_root(&requested)?;
    let scope = requested.strip_prefix(&root)?.to_path_buf();
    let combined_pattern = build_pattern(&opts.patterns, opts.fixed_strings, opts.regex)?;
    // Reject invalid queries locally as well, rather than falling back after a server error.
    let mut parsed = query::try_parse_query(&combined_pattern)?;
    if opts.word_regexp {
        parsed.apply_word_boundaries()?;
    }
    anyhow::ensure!(
        !parsed.options.explicit_limit,
        "top:N applies to ranked interactive search; use --max-count N for CLI output"
    );
    parsed.options.case_insensitive = opts.ignore_case;
    parsed.filters.search_scope = (!scope.as_os_str().is_empty()).then_some(scope);
    let (before, after) = opts
        .context
        .map(|n| (n, n))
        .unwrap_or((opts.before_context, opts.after_context));
    let options = ContentSearchOptions {
        context_before: before,
        context_after: after,
        case_insensitive: opts.ignore_case,
        files_only: opts.files_with_matches,
        compact_files: opts.files_with_matches,
        counts_only: opts.count && !opts.files_with_matches,
        word_regexp: opts.word_regexp,
    };
    let daemon_response = if let Some(mut client) = server::IndexClient::connect() {
        match client.content_search(&combined_pattern, Some(&requested), opts.max_count, options) {
            Ok(response) => Some(response),
            Err(error) => {
                eprintln!("Daemon search failed, falling back to direct search: {error}");
                None
            }
        }
    } else {
        None
    };
    let response = if let Some(response) = daemon_response {
        response
    } else {
        let started = std::time::Instant::now();
        let mut response = ContentSearchResponse {
            matches: Vec::new(),
            file_paths: None,
            file_counts: None,
            duration_ms: 0.0,
            files_with_matches: 0,
            resolved_root: Some(root.clone()),
        };
        if opts.files_with_matches
            && let Some(meta) = index::negative_routing::preflight(&root, &parsed)
        {
            warn_if_stale_metadata(&meta, &root);
            response.file_paths = Some(Vec::new());
        } else {
            let reader = index::reader::IndexReader::open_for_search_uncached(&root)?;
            warn_if_stale(&reader, &root);
            let executor = query::QueryExecutor::new(&reader);
            if opts.files_with_matches {
                response.file_paths = Some(if parsed.is_empty() {
                    Vec::new()
                } else {
                    executor.execute_files_only(&parsed, opts.max_count)?
                });
            } else if opts.count {
                response.file_counts = Some(if parsed.is_empty() {
                    Vec::new()
                } else {
                    executor.execute_match_counts(&parsed, opts.max_count)?
                });
            } else if !parsed.is_empty() {
                response.matches = executor
                    .execute_with_content(&parsed, before, after)?
                    .into_iter()
                    .take(if opts.max_count == 0 {
                        usize::MAX
                    } else {
                        opts.max_count
                    })
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
            }
        }
        response.files_with_matches = response
            .file_paths
            .as_ref()
            .map(Vec::len)
            .or_else(|| response.file_counts.as_ref().map(Vec::len))
            .unwrap_or_else(|| {
                response
                    .matches
                    .iter()
                    .map(|m| &m.path)
                    .collect::<std::collections::HashSet<_>>()
                    .len()
            });
        response.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        response
    };
    if opts.json {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        return Ok(());
    }
    let color = match opts.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").is_ok_and(|term| term != "dumb")
        }
    };
    if opts.files_with_matches {
        let paths = response
            .file_paths
            .unwrap_or_else(|| response.matches.into_iter().map(|m| m.path).collect());
        if opts.null {
            let mut stdout = std::io::stdout().lock();
            for path in paths {
                stdout.write_all(path.as_os_str().as_encoded_bytes())?;
                stdout.write_all(&[0])?;
            }
        } else {
            output::print_file_paths(&paths, color)?;
        }
    } else if opts.count {
        if let Some(counts) = response.file_counts {
            output::print_file_counts(&counts, color)?;
        } else {
            output::print_match_counts(&response.matches, color)?;
        }
    } else {
        let heading = opts.heading || (!opts.no_heading && std::io::stdout().is_terminal());
        output::print_content_matches(&response.matches, color, heading)?;
    }
    Ok(())
}

/// Searching without a daemon means results reflect the index as of its
/// last update; surface that when the index looks old instead of silently
/// missing recent changes. Tunable via FXI_STALE_WARN_SECS (0 disables).
fn warn_if_stale(reader: &index::reader::IndexReader, root: &Path) {
    warn_if_stale_metadata(&reader.meta, root);
}

fn warn_if_stale_metadata(meta: &index::types::IndexMeta, root: &Path) {
    use std::io::IsTerminal;

    if !std::io::stderr().is_terminal() {
        return;
    }
    let threshold = std::env::var("FXI_STALE_WARN_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3600);
    if threshold == 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(meta.updated_at);
    if age > threshold {
        eprintln!(
            "note: index for {} was last updated {}h {}m ago; run `fxi index` or `fxi daemon start --watch` to keep it fresh",
            root.display(),
            age / 3600,
            (age % 3600) / 60,
        );
    }
}

/// Preserve query semantics when composing OR alternatives; literal/regex modes
/// encode slash delimiters without depending on the query parser's lexer state.
fn build_pattern(patterns: &[String], fixed: bool, regex_mode: bool) -> Result<String> {
    let patterns: Vec<String> = patterns
        .iter()
        .map(|pattern| {
            if fixed || regex_mode {
                let expression = if fixed {
                    regex::escape(pattern)
                } else {
                    pattern.clone()
                };
                // Preserve backslash parity and encode delimiter slashes, including
                // slashes already escaped by the caller or inside character classes.
                let mut encoded = String::new();
                let mut chars = expression.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\\' {
                        if let Some(next) = chars.next() {
                            if next == '/' {
                                encoded.push_str("\\x2f");
                            } else {
                                encoded.push('\\');
                                encoded.push(next);
                            }
                        } else {
                            encoded.push(ch);
                        }
                    } else if ch == '/' {
                        encoded.push_str("\\x2f");
                    } else {
                        encoded.push(ch);
                    }
                }
                let expression = encoded;
                format!("re:/{expression}/")
            } else {
                pattern.clone()
            }
        })
        .collect();
    for pattern in &patterns {
        query::try_parse_query(pattern)?;
    }
    Ok(if patterns.len() == 1 {
        patterns[0].clone()
    } else {
        patterns
            .iter()
            .map(|p| format!("({p})"))
            .collect::<Vec<_>>()
            .join(" | ")
    })
}
