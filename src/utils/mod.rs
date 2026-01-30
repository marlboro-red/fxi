//! Utility functions and data structures.
//!
//! This module provides shared utilities used throughout FXI:
//!
//! ## Modules
//!
//! - [`app_data`] - Application data directory management (XDG-compliant)
//! - [`bloom`] - Bloom filter for fast negative lookups
//! - [`encoding`] - Variable-length integer encoding (varint)
//! - [`trigram`] - 3-byte sequence extraction for indexing
//! - [`tokenizer`] - Identifier extraction (camelCase, snake_case)
//!
//! ## Key Functions
//!
//! ```no_run
//! use fxi::utils::{extract_trigrams, extract_tokens};
//!
//! // Extract trigrams for substring matching
//! let trigrams = extract_trigrams(b"hello world");
//! // Returns: ["hel", "ell", "llo", "lo ", ...]
//!
//! // Extract tokens/identifiers
//! let tokens = extract_tokens("getUserById");
//! // Returns: ["get", "user", "by", "id", "getuserbyid"]
//! ```

pub mod app_data;
pub mod bloom;
pub mod encoding;
pub mod trigram;
pub mod tokenizer;

pub use app_data::*;
pub use bloom::*;
pub use encoding::*;
pub use trigram::*;
pub use tokenizer::*;
