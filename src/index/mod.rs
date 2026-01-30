//! Index management for code search.
//!
//! This module provides the core indexing infrastructure:
//!
//! - [`build`] - Index construction from filesystem
//! - [`reader`] - Memory-mapped index reading
//! - [`writer`] - Streaming index writing
//! - [`types`] - Data structures (Document, Trigram, etc.)
//! - [`compact`] - Segment compaction
//! - [`stats`] - Index statistics
//!
//! ## Index Structure
//!
//! The on-disk index layout:
//!
//! ```text
//! ~/.local/share/fxi/indexes/{hash}/
//! ├── meta.json           # Index metadata
//! ├── docs.bin            # Document table (mmap'd)
//! ├── paths.bin           # Path store
//! └── segments/
//!     └── seg_0001/
//!         ├── grams.dict     # Trigram dictionary
//!         ├── grams.postings # Trigram postings
//!         ├── tokens.dict    # Token dictionary
//!         ├── tokens.postings# Token postings
//!         └── bloom.bin      # Bloom filter
//! ```
//!
//! ## Usage
//!
//! ```ignore
//! use fxi::index::build::build_index;
//! use fxi::index::reader::IndexReader;
//! use std::path::PathBuf;
//!
//! // Build index for a codebase
//! let path = PathBuf::from("/path/to/code");
//! build_index(&path, false).unwrap();
//!
//! // Read the index
//! let reader = IndexReader::open(&path).unwrap();
//! println!("Indexed {} files", reader.meta.doc_count);
//! ```

pub mod build;
pub mod compact;
pub mod reader;
pub mod stats;
pub mod types;
pub mod writer;

// Re-exports for public API
#[allow(unused_imports)]
pub use reader::IndexReader;
#[allow(unused_imports)]
pub use types::*;
#[allow(unused_imports)]
pub use writer::IndexWriter;
