use crate::query::scorer::ScoringWeights;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Unique identifier for a document in the index
pub type DocId = u32;

/// Unique identifier for a path in the path store
pub type PathId = u32;

/// Segment identifier
pub type SegmentId = u16;

/// A trigram is a 3-byte sequence stored as u32 (only lower 24 bits used)
pub type Trigram = u32;

/// Language detection enum
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(u16)]
pub enum Language {
    #[default]
    Unknown = 0,
    Rust = 1,
    Python = 2,
    JavaScript = 3,
    TypeScript = 4,
    Go = 5,
    C = 6,
    Cpp = 7,
    Java = 8,
    Ruby = 9,
    Shell = 10,
    Markdown = 11,
    Json = 12,
    Yaml = 13,
    Toml = 14,
    Html = 15,
    Css = 16,
    Sql = 17,
    Haskell = 18,
    Scala = 19,
    Kotlin = 20,
    Swift = 21,
    Php = 22,
    CSharp = 23,
    Elixir = 24,
    Clojure = 25,
    Lua = 26,
    Perl = 27,
    R = 28,
    Zig = 29,
    Nim = 30,
    Ocaml = 31,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" | "pyw" => Language::Python,
            "js" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" | "jsx" => Language::TypeScript,
            "go" => Language::Go,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Language::Cpp,
            "java" => Language::Java,
            "rb" | "rake" => Language::Ruby,
            "sh" | "bash" | "zsh" | "fish" => Language::Shell,
            "md" | "markdown" => Language::Markdown,
            "json" => Language::Json,
            "yaml" | "yml" => Language::Yaml,
            "toml" => Language::Toml,
            "html" | "htm" => Language::Html,
            "css" | "scss" | "sass" | "less" => Language::Css,
            "sql" => Language::Sql,
            "hs" | "lhs" => Language::Haskell,
            "scala" | "sc" => Language::Scala,
            "kt" | "kts" => Language::Kotlin,
            "swift" => Language::Swift,
            "php" => Language::Php,
            "cs" => Language::CSharp,
            "ex" | "exs" => Language::Elixir,
            "clj" | "cljs" | "cljc" | "edn" => Language::Clojure,
            "lua" => Language::Lua,
            "pl" | "pm" => Language::Perl,
            "r" | "R" => Language::R,
            "zig" => Language::Zig,
            "nim" => Language::Nim,
            "ml" | "mli" => Language::Ocaml,
            _ => Language::Unknown,
        }
    }
}

/// Document flags
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DocFlags(pub u16);

impl DocFlags {
    pub const NONE: u16 = 0;
    #[allow(dead_code)]
    pub const BINARY: u16 = 1 << 0;
    #[allow(dead_code)]
    pub const GENERATED: u16 = 1 << 1;
    #[allow(dead_code)]
    pub const VENDOR: u16 = 1 << 2;
    pub const MINIFIED: u16 = 1 << 3;
    pub const STALE: u16 = 1 << 4;
    pub const TOMBSTONE: u16 = 1 << 5;

    pub fn new() -> Self {
        Self(Self::NONE)
    }

    #[allow(dead_code)]
    pub fn is_binary(&self) -> bool {
        self.0 & Self::BINARY != 0
    }

    pub fn is_stale(&self) -> bool {
        self.0 & Self::STALE != 0
    }

    pub fn is_tombstone(&self) -> bool {
        self.0 & Self::TOMBSTONE != 0
    }

    #[allow(dead_code)]
    pub fn set_binary(&mut self) {
        self.0 |= Self::BINARY;
    }

    pub fn set_stale(&mut self) {
        self.0 |= Self::STALE;
    }

    #[allow(dead_code)]
    pub fn set_tombstone(&mut self) {
        self.0 |= Self::TOMBSTONE;
    }
}

/// Document entry in the document table
#[derive(Debug, Clone)]
pub struct Document {
    pub doc_id: DocId,
    pub path_id: PathId,
    pub size: u64,
    pub mtime: u64,
    pub language: Language,
    pub flags: DocFlags,
    pub segment_id: SegmentId,
}

impl Document {
    /// Size of a document entry in bytes (fixed-size for mmap)
    #[allow(dead_code)]
    pub const SIZE: usize = 4 + 4 + 8 + 8 + 2 + 2 + 2; // 30 bytes

    pub fn is_valid(&self) -> bool {
        !self.flags.is_stale() && !self.flags.is_tombstone()
    }
}

/// Index metadata stored in meta.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub root_path: PathBuf,
    pub doc_count: u32,
    pub segment_count: u16,
    pub base_segment: Option<u16>,
    pub delta_segments: Vec<u16>,
    pub stop_grams: Vec<Trigram>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Default for IndexMeta {
    fn default() -> Self {
        Self {
            version: 1,
            root_path: PathBuf::new(),
            doc_count: 0,
            segment_count: 0,
            base_segment: None,
            delta_segments: Vec::new(),
            stop_grams: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// Posting entry - a reference to a document containing a term
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting {
    pub doc_id: DocId,
}

/// Dictionary entry mapping a term to its postings
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct DictEntry {
    pub offset: u64,
    pub length: u32,
    pub doc_freq: u32,
}

/// Search match result
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub doc_id: DocId,
    pub path: PathBuf,
    pub line_number: u32,
    pub score: f32,
}

/// Configuration for the indexer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    pub max_file_size: u64,
    pub stop_gram_count: usize,
    pub delta_threshold: usize,
    pub compaction_ratio: f32,
    pub ignored_paths: Vec<String>,
    /// Scoring weights for search result ranking
    pub scoring_weights: ScoringWeights,
    /// Number of files per segment chunk (for memory-bounded indexing)
    pub chunk_size: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_file_size: 100 * 1024 * 1024, // 100MB - matches GitHub's file size limit
            stop_gram_count: 512,
            delta_threshold: 100,
            compaction_ratio: 0.5,
            ignored_paths: vec![
                ".git".to_string(),
                "node_modules".to_string(),
                "target".to_string(),
                ".codesearch".to_string(),
            ],
            scoring_weights: ScoringWeights::default(),
            chunk_size: 50000, // Files per segment chunk (larger = fewer segments = faster)
        }
    }
}

/// Convert 3 bytes to a trigram
#[inline]
pub fn bytes_to_trigram(b0: u8, b1: u8, b2: u8) -> Trigram {
    ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32)
}

/// Convert trigram back to bytes
#[inline]
#[allow(dead_code)]
pub fn trigram_to_bytes(t: Trigram) -> [u8; 3] {
    [
        ((t >> 16) & 0xFF) as u8,
        ((t >> 8) & 0xFF) as u8,
        (t & 0xFF) as u8,
    ]
}
