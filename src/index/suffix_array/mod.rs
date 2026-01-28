//! Suffix array indexing module
//!
//! This module provides O(m log n) exact substring search using suffix arrays.
//! It complements the trigram index by providing faster literal searches while
//! maintaining the trigram index for regex and fuzzy queries.
//!
//! ## Architecture
//!
//! - `builder`: Constructs suffix arrays from documents
//! - `writer`: Persists suffix arrays to disk
//! - `reader`: Memory-mapped reading and searching
//! - `types`: Core type definitions
//!
//! ## File Format
//!
//! Per segment, three files are created:
//! - `concat.bin`: Concatenated document text with sentinel separators
//! - `concat.idx`: Document boundary index for position-to-doc mapping
//! - `sa.bin`: The sorted suffix array (positions into concat.bin)

pub mod builder;
pub mod reader;
pub mod types;
pub mod writer;

// Re-exports for convenience
pub use builder::{BuiltSuffixArray, SuffixArrayBuilder};
pub use reader::{SuffixArrayReader, SuffixArrayStats};
pub use types::{SuffixArrayConfig, SuffixArrayMeta, SuffixMatch};
pub use writer::SuffixArrayWriter;
