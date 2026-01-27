mod index;
mod query;
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
