//! # FXI - Fast Code Search Engine
//!
//! FXI is a terminal-first, ultra-fast code search engine that achieves
//! 100-400x faster search performance than ripgrep on large codebases
//! through persistent indexing.
//!
//! ## Architecture
//!
//! The crate is organized into these main modules:
//!
//! - [`index`] - Index building and reading (trigram + token indexes)
//! - [`query`] - Query parsing, planning, and execution
//! - [`server`] - Persistent daemon for instant searches
//! - [`tui`] - Interactive terminal UI
//! - [`output`] - Result formatting (ripgrep-compatible)
//! - [`utils`] - Utility functions (trigrams, encoding, bloom filters)
//!
//! ## Quick Start
//!
//! ```ignore
//! use fxi::index::reader::IndexReader;
//! use fxi::query::{parse_query, QueryExecutor};
//! use std::path::PathBuf;
//!
//! // Open an existing index
//! let reader = IndexReader::open(&PathBuf::from("/path/to/codebase")).unwrap();
//!
//! // Parse and execute a query
//! let query = parse_query("fn main");
//! let executor = QueryExecutor::new(&reader);
//! let results = executor.execute(&query).unwrap();
//!
//! for result in results {
//!     println!("{}:{}", result.path.display(), result.line_number);
//! }
//! ```
//!
//! ## Performance
//!
//! FXI uses a hybrid two-tier indexing strategy:
//!
//! 1. **Trigram Index** - 3-byte substring sequences for fast candidate narrowing
//! 2. **Token Index** - Extracted identifiers for exact word matching
//!
//! Combined with memory-mapped I/O, parallel processing, and LRU caching,
//! this enables sub-100ms searches on million-file codebases.

pub mod index;
pub mod output;
pub mod query;
pub mod server;
pub mod tui;
pub mod utils;
