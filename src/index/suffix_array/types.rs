//! Types for suffix array indexing
//!
//! This module defines the core types used for suffix array-based search,
//! which provides O(m log n) exact substring matching.

use crate::index::types::DocId;
use serde::{Deserialize, Serialize};

/// Position in concatenated text (supports up to 16 exabytes)
pub type TextPosition = u64;

/// Suffix array entry - position in concatenated text
pub type SuffixEntry = u64;

/// Magic number for suffix array files
pub const SA_MAGIC: u32 = 0x46585341; // "FXSA" in little-endian

/// Current version of the suffix array format
pub const SA_VERSION: u32 = 1;

/// Sentinel byte used to separate documents in concatenated text
/// Using 0x00 as it's invalid in most text and won't appear in code
pub const SENTINEL_BYTE: u8 = 0x00;

/// Document boundary in concatenated text
#[derive(Debug, Clone, Copy)]
pub struct DocBoundary {
    /// Document ID from the main index
    pub doc_id: DocId,
    /// Start position in concatenated text (inclusive)
    pub start: TextPosition,
    /// End position in concatenated text (exclusive, before sentinel)
    pub end: TextPosition,
}

/// Suffix array search result with position information
#[derive(Debug, Clone)]
pub struct SuffixMatch {
    /// Document ID where match was found
    pub doc_id: DocId,
    /// Byte position within the original document
    pub position: usize,
    /// Global position in concatenated text (for debugging)
    pub global_position: TextPosition,
}

/// Configuration for suffix array building
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuffixArrayConfig {
    /// Enable suffix array index (default: true)
    pub enabled: bool,
    /// Maximum file size to include in SA (bytes, default: 10MB)
    /// Files larger than this use trigram fallback
    pub max_file_size: u64,
    /// Build case-insensitive SA by lowercasing text (default: true)
    pub case_insensitive: bool,
}

impl Default for SuffixArrayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_size: 10 * 1024 * 1024, // 10MB
            case_insensitive: true,
        }
    }
}

/// Suffix array metadata stored in meta.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SuffixArrayMeta {
    /// Whether suffix array is enabled
    pub enabled: bool,
    /// Total size of concatenated text
    pub total_text_size: u64,
    /// Number of suffixes (equals total_text_size)
    pub suffix_count: u64,
    /// Number of documents included in SA
    pub doc_count: u32,
    /// Number of documents excluded (too large, binary, etc.)
    pub excluded_count: u32,
    /// Whether SA was built case-insensitive
    pub case_insensitive: bool,
}

/// Header for sa.bin file
#[derive(Debug, Clone, Copy)]
pub struct SuffixArrayHeader {
    /// Magic number (SA_MAGIC)
    pub magic: u32,
    /// Version number
    pub version: u32,
    /// Number of suffix entries
    pub suffix_count: u64,
    /// Flags (reserved for future use)
    pub flags: u32,
}

impl SuffixArrayHeader {
    /// Size of header in bytes
    pub const SIZE: usize = 4 + 4 + 8 + 4; // 20 bytes

    pub fn new(suffix_count: u64) -> Self {
        Self {
            magic: SA_MAGIC,
            version: SA_VERSION,
            suffix_count,
            flags: 0,
        }
    }
}

/// Header for concat.idx file
#[derive(Debug, Clone, Copy)]
pub struct ConcatIndexHeader {
    /// Magic number (SA_MAGIC)
    pub magic: u32,
    /// Version number
    pub version: u32,
    /// Number of documents
    pub doc_count: u32,
    /// Total size of concatenated text
    pub total_size: u64,
    /// Flags (reserved)
    pub flags: u32,
}

impl ConcatIndexHeader {
    /// Size of header in bytes
    pub const SIZE: usize = 4 + 4 + 4 + 8 + 4; // 24 bytes

    pub fn new(doc_count: u32, total_size: u64) -> Self {
        Self {
            magic: SA_MAGIC,
            version: SA_VERSION,
            doc_count,
            total_size,
            flags: 0,
        }
    }
}

/// Entry in concat.idx for each document
#[derive(Debug, Clone, Copy)]
pub struct ConcatIndexEntry {
    /// Document ID
    pub doc_id: DocId,
    /// Start offset in concat.bin
    pub start: TextPosition,
    /// End offset in concat.bin (exclusive)
    pub end: TextPosition,
}

impl ConcatIndexEntry {
    /// Size of each entry in bytes
    pub const SIZE: usize = 4 + 8 + 8; // 20 bytes
}
