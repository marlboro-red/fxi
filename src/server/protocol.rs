//! Protocol messages for client-server communication
//!
//! Uses a simple length-prefixed JSON protocol:
//! - 4 bytes (little-endian u32): message length
//! - N bytes: JSON-encoded message

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

// Re-export ContentMatch from output module
pub use crate::output::ContentMatch;

/// Options for content search
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContentSearchOptions {
    /// Lines of context before match (-B flag)
    pub context_before: u32,
    /// Lines of context after match (-A flag)
    pub context_after: u32,
    /// Case insensitive search (-i flag)
    pub case_insensitive: bool,
    /// Only return first match per file (for -l mode optimization)
    #[serde(default)]
    pub files_only: bool,
}

/// Request from client to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Execute a search query
    Search {
        /// The search query string
        query: String,
        /// Root path of the codebase to search
        root_path: PathBuf,
        /// Maximum number of results
        limit: usize,
    },

    /// Execute a content search query (ripgrep-like)
    ContentSearch {
        /// The search pattern
        pattern: String,
        /// Root path of the codebase to search
        root_path: PathBuf,
        /// Maximum number of results
        limit: usize,
        /// Content search options
        options: ContentSearchOptions,
    },

    /// Check server health and get stats
    Status,

    /// Request server to reload index for a path
    Reload { root_path: PathBuf },

    /// Graceful shutdown request
    Shutdown,

    /// Ping for connection testing
    Ping,
}

/// Response from server to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// Search results
    Search(SearchResponse),

    /// Content search results (ripgrep-like)
    ContentSearch(ContentSearchResponse),

    /// Server status
    Status(StatusResponse),

    /// Reload completed
    Reloaded { success: bool, message: String },

    /// Shutdown acknowledged
    ShuttingDown,

    /// Pong response
    Pong,

    /// Error response
    Error { message: String },
}

/// Search results response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    /// The matches found
    pub matches: Vec<SearchMatchData>,
    /// Time taken in milliseconds
    pub duration_ms: f64,
    /// Whether results came from cache
    pub cached: bool,
}

/// Serializable search match (mirrors SearchMatch but with Serialize/Deserialize)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatchData {
    pub doc_id: u32,
    pub path: PathBuf,
    pub line_number: u32,
    pub score: f32,
}

/// Content search response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentSearchResponse {
    pub matches: Vec<ContentMatch>,
    pub duration_ms: f64,
    pub files_with_matches: usize,
}

/// Server status response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Server uptime in seconds
    pub uptime_secs: u64,
    /// Number of indexes currently loaded
    pub indexes_loaded: usize,
    /// Total documents across all indexes
    pub total_docs: u32,
    /// Total queries served
    pub queries_served: u64,
    /// Cache hit rate (0.0 - 1.0)
    pub cache_hit_rate: f32,
    /// Memory usage in bytes (approximate)
    pub memory_bytes: u64,
    /// Loaded codebase roots
    pub loaded_roots: Vec<PathBuf>,
}

/// Write a message to a stream with length prefix
pub fn write_message<W: Write>(writer: &mut W, msg: &impl Serialize) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;

    let len = json.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&json)?;
    writer.flush()?;

    Ok(())
}

/// Read a message from a stream with length prefix
pub fn read_message<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Sanity check: don't allocate more than 100MB
    if len > 100 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Message too large",
        ));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;

    serde_json::from_slice(&buf).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_roundtrip_request() {
        let req = Request::Search {
            query: "test query".to_string(),
            root_path: PathBuf::from("/home/user/project"),
            limit: 100,
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_message(&mut cursor).unwrap();

        match decoded {
            Request::Search { query, root_path, limit } => {
                assert_eq!(query, "test query");
                assert_eq!(root_path, PathBuf::from("/home/user/project"));
                assert_eq!(limit, 100);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_roundtrip_response() {
        let resp = Response::Search(SearchResponse {
            matches: vec![SearchMatchData {
                doc_id: 1,
                path: PathBuf::from("src/main.rs"),
                line_number: 42,
                score: 1.5,
            }],
            duration_ms: 12.5,
            cached: false,
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &resp).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Response = read_message(&mut cursor).unwrap();

        match decoded {
            Response::Search(sr) => {
                assert_eq!(sr.matches.len(), 1);
                assert_eq!(sr.matches[0].line_number, 42);
            }
            _ => panic!("Wrong variant"),
        }
    }
}
