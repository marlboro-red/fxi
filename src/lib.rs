//! # FXI — indexed code search
//!
//! FXI builds persistent indexes for local code search and provides a CLI,
//! terminal UI and optional daemon. Performance depends on the corpus, query,
//! output mode, cache state and update workload; measured comparisons and their
//! limitations are recorded in the repository's performance reports.
//!
//! ## Modules
//!
//! - [`index`] — building, reading and compacting immutable index generations
//! - [`query`] — bounded parsing, candidate planning, verification and ranking
//! - [`server`] — local IPC, resident readers and optional filesystem watching
//! - [`tui`] — interactive terminal search
//! - [`output`] — CLI result formatting
//! - [`utils`] — encoding, tokenization, paths and index locking
//!
//! ## Search an existing index
//!
//! ```no_run
//! use fxi::index::reader::IndexReader;
//! use fxi::query::{try_parse_query, QueryExecutor};
//! use std::path::Path;
//!
//! fn main() -> anyhow::Result<()> {
//!     // Build this root first with `fxi index /path/to/codebase`.
//!     let reader = IndexReader::open(Path::new("/path/to/codebase"))?;
//!     // Inner quotes request one exact, case-sensitive phrase.
//!     let query = try_parse_query(r#""fn main""#)?;
//!     let executor = QueryExecutor::new(&reader);
//!     for path in executor.execute_files_only(&query, 20)? {
//!         println!("{}", path.display());
//!     }
//!     Ok(())
//! }
//! ```
//!
//! Prefer [`query::try_parse_query`] to report malformed input immediately.
//! The compatibility [`query::parse_query`] API retains an error node on failure;
//! executors reject it rather than silently searching with a partial query.
//! Bare literals match case-insensitive substrings, whitespace combines terms
//! with file-level AND, and phrases/regexes have their own case semantics.
//!
//! ## Indexing and freshness
//!
//! Conservative gram constraints narrow candidate files; source verification
//! determines matches. Token/position data and immutable mapped segments support
//! additional indexed operations. An existing reader does not automatically
//! observe later generations. A stale index can miss files newly made matching,
//! even though verification removes stale positive content matches.
//!
//! The watched daemon can expose bounded memory deltas before durable publication.
//! Search visibility and persistence are distinct; use the documented daemon
//! lifecycle and shutdown behavior when durable completion matters.

pub mod index;
pub mod output;
pub mod query;
pub mod server;
pub mod tui;
pub mod utils;
