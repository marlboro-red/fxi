//! Protocol messages for client-server communication
//!
//! Uses a simple length-prefixed JSON protocol:
//! - 4 bytes (little-endian u32): message length
//! - N bytes: JSON-encoded message

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

/// Protocol version number. Bumped only on breaking changes
/// (field removal/rename, semantic changes, wire format changes).
/// Adding new optional fields or new request types does NOT require a bump.
pub const PROTOCOL_VERSION: u32 = 2;

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
        /// Root path of the codebase to search (optional — server resolves subdirs or uses single loaded index)
        #[serde(default)]
        root_path: Option<PathBuf>,
        /// Maximum number of results
        limit: usize,
    },

    /// Execute a content search query (ripgrep-like)
    ContentSearch {
        /// The search pattern
        pattern: String,
        /// Root path of the codebase to search (optional — server resolves subdirs or uses single loaded index)
        #[serde(default)]
        root_path: Option<PathBuf>,
        /// Maximum number of results
        limit: usize,
        /// Content search options
        options: ContentSearchOptions,
    },

    /// Check server health and get stats
    Status,

    /// Request server to reload index for a path
    Reload {
        #[serde(default)]
        root_path: Option<PathBuf>,
    },

    /// Graceful shutdown request
    Shutdown,

    /// Ping for connection testing
    Ping,

    /// Protocol version handshake
    Hello {
        /// Client's protocol version
        protocol_version: u32,
    },

    /// Ask whether the daemon is watching (and keeping fresh) a root
    WatchStatus {
        #[serde(default)]
        root_path: Option<PathBuf>,
    },
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
    Reloaded {
        success: bool,
        message: String,
        /// The resolved codebase root the server used
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_root: Option<PathBuf>,
    },

    /// Shutdown acknowledged
    ShuttingDown,

    /// Pong response
    Pong,

    /// Error response
    Error { message: String },

    /// Protocol version handshake response
    Hello {
        /// Server's protocol version
        protocol_version: u32,
        /// Server software version (e.g. "0.1.0")
        server_version: String,
    },

    /// Watch status for a root
    WatchStatus {
        /// Whether a file watcher is active for this root
        watching: bool,
        /// Debounced file changes not yet flushed to a delta segment
        pending_changes: usize,
        /// The resolved codebase root the server used
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_root: Option<PathBuf>,
    },
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
    /// The resolved codebase root the server used
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_root: Option<PathBuf>,
}

/// Serializable search match (mirrors SearchMatch but with Serialize/Deserialize)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatchData {
    pub path: PathBuf,
    pub line_number: u32,
    pub score: f32,
}

/// Match with line content (for ripgrep-like output)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentMatch {
    pub path: PathBuf,
    pub line_number: u32,
    pub line_content: String,
    pub match_start: usize,
    pub match_end: usize,
    pub context_before: Vec<(u32, String)>,
    pub context_after: Vec<(u32, String)>,
}

/// Content search response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentSearchResponse {
    pub matches: Vec<ContentMatch>,
    pub duration_ms: f64,
    pub files_with_matches: usize,
    /// The resolved codebase root the server used
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_root: Option<PathBuf>,
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
    /// Protocol version (0 if server predates versioning)
    #[serde(default)]
    pub protocol_version: u32,
    /// Server software version (empty if server predates versioning)
    #[serde(default)]
    pub server_version: String,
}

/// Serialization envelope that appends an optional `request_id` field to a
/// message without an intermediate `serde_json::Value` round-trip. The
/// flattened message must serialize as a JSON object (all Request/Response
/// variants do).
#[derive(Serialize)]
struct TaggedMessage<'a, T: Serialize> {
    #[serde(flatten)]
    msg: &'a T,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

/// Deserialization helper that extracts only the `request_id` field, skipping
/// everything else without building any intermediate structure.
#[derive(Deserialize)]
struct RequestIdOnly {
    #[serde(default, deserialize_with = "string_or_none")]
    request_id: Option<String>,
}

/// Tolerate a non-string `request_id` by ignoring it (matches the previous
/// behavior of only extracting string-valued ids).
fn string_or_none<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    })
}

/// Write a message to a stream with length prefix and an optional request_id.
///
/// The message is serialized in a single pass; `request_id`, if provided, is
/// appended as an extra field of the serialized object.
pub fn write_message_with_id<W: Write>(
    writer: &mut W,
    msg: &impl Serialize,
    request_id: Option<&str>,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(&TaggedMessage { msg, request_id })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let len = json.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&json)?;
    writer.flush()?;

    Ok(())
}

/// Read a message from a stream with length prefix, extracting an optional request_id.
///
/// The message is deserialized directly into `T` (serde ignores the unknown
/// `request_id` field); the id itself is recovered with a second, allocation-
/// free skim of the buffer rather than a `serde_json::Value` round-trip.
pub fn read_message_with_id<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> std::io::Result<(T, Option<String>)> {
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

    let msg: T = serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let request_id = serde_json::from_slice::<RequestIdOnly>(&buf)
        .map(|e| e.request_id)
        .unwrap_or(None);

    Ok((msg, request_id))
}

/// Write a message to a stream with length prefix
#[cfg(test)]
pub fn write_message<W: Write>(writer: &mut W, msg: &impl Serialize) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let len = json.len() as u32;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&json)?;
    writer.flush()?;

    Ok(())
}

/// Read a message from a stream with length prefix
#[cfg(test)]
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

    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_roundtrip_watch_status() {
        let req = Request::WatchStatus {
            root_path: Some(PathBuf::from("/home/user/project")),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();
        let decoded: Request = read_message(&mut Cursor::new(buf)).unwrap();
        match decoded {
            Request::WatchStatus { root_path } => {
                assert_eq!(root_path, Some(PathBuf::from("/home/user/project")))
            }
            _ => panic!("Wrong variant"),
        }

        let resp = Response::WatchStatus {
            watching: true,
            pending_changes: 3,
            resolved_root: Some(PathBuf::from("/home/user/project")),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &resp).unwrap();
        let decoded: Response = read_message(&mut Cursor::new(buf)).unwrap();
        match decoded {
            Response::WatchStatus {
                watching,
                pending_changes,
                resolved_root,
            } => {
                assert!(watching);
                assert_eq!(pending_changes, 3);
                assert_eq!(resolved_root, Some(PathBuf::from("/home/user/project")));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_roundtrip_request() {
        let req = Request::Search {
            query: "test query".to_string(),
            root_path: Some(PathBuf::from("/home/user/project")),
            limit: 100,
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_message(&mut cursor).unwrap();

        match decoded {
            Request::Search {
                query,
                root_path,
                limit,
            } => {
                assert_eq!(query, "test query");
                assert_eq!(root_path, Some(PathBuf::from("/home/user/project")));
                assert_eq!(limit, 100);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_roundtrip_hello_request() {
        let req = Request::Hello {
            protocol_version: 1,
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_message(&mut cursor).unwrap();

        match decoded {
            Request::Hello { protocol_version } => {
                assert_eq!(protocol_version, 1);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_roundtrip_hello_response() {
        let resp = Response::Hello {
            protocol_version: 1,
            server_version: "0.1.0".to_string(),
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &resp).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Response = read_message(&mut cursor).unwrap();

        match decoded {
            Response::Hello {
                protocol_version,
                server_version,
            } => {
                assert_eq!(protocol_version, 1);
                assert_eq!(server_version, "0.1.0");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_status_response_version_defaults() {
        // Simulate a StatusResponse from an old server (no version fields)
        let json = r#"{
            "type": "Status",
            "uptime_secs": 100,
            "indexes_loaded": 1,
            "total_docs": 500,
            "queries_served": 10,
            "cache_hit_rate": 0.5,
            "memory_bytes": 1024,
            "loaded_roots": []
        }"#;

        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::Status(status) => {
                assert_eq!(status.protocol_version, 0);
                assert_eq!(status.server_version, "");
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_roundtrip_response() {
        let resp = Response::Search(SearchResponse {
            matches: vec![SearchMatchData {
                path: PathBuf::from("src/main.rs"),
                line_number: 42,
                score: 1.5,
            }],
            duration_ms: 12.5,
            cached: false,
            resolved_root: Some(PathBuf::from("/home/user/project")),
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &resp).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Response = read_message(&mut cursor).unwrap();

        match decoded {
            Response::Search(sr) => {
                assert_eq!(sr.matches.len(), 1);
                assert_eq!(sr.matches[0].line_number, 42);
                assert_eq!(sr.resolved_root, Some(PathBuf::from("/home/user/project")));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_search_response_with_doc_id_backward_compat() {
        // Old server sends doc_id in SearchMatchData; new client should ignore it
        let json = r#"{
            "type": "Search",
            "matches": [
                {"doc_id": 99, "path": "src/main.rs", "line_number": 10, "score": 2.5}
            ],
            "duration_ms": 1.0,
            "cached": false
        }"#;

        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::Search(sr) => {
                assert_eq!(sr.matches.len(), 1);
                assert_eq!(sr.matches[0].path, PathBuf::from("src/main.rs"));
                assert_eq!(sr.matches[0].line_number, 10);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_search_request_with_root_path_backward_compat() {
        // Old client sends root_path as a string, should deserialize to Some(...)
        let json =
            r#"{"type":"Search","query":"main","root_path":"/home/user/project","limit":10}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Search { root_path, .. } => {
                assert_eq!(root_path, Some(PathBuf::from("/home/user/project")));
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_search_request_without_root_path() {
        // New client omits root_path entirely
        let json = r#"{"type":"Search","query":"main","limit":10}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Search { root_path, .. } => {
                assert_eq!(root_path, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_resolved_root_omitted_when_none() {
        // resolved_root: None should not appear in serialized JSON
        let resp = SearchResponse {
            matches: vec![],
            duration_ms: 1.0,
            cached: false,
            resolved_root: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("resolved_root"));

        // resolved_root: Some should appear
        let resp_with_root = SearchResponse {
            matches: vec![],
            duration_ms: 1.0,
            cached: false,
            resolved_root: Some(PathBuf::from("/tmp/test")),
        };
        let json = serde_json::to_string(&resp_with_root).unwrap();
        assert!(json.contains("resolved_root"));
    }

    #[test]
    fn test_roundtrip_with_request_id() {
        let req = Request::Ping;
        let mut buf = Vec::new();
        write_message_with_id(&mut buf, &req, Some("c-42")).unwrap();

        let mut cursor = Cursor::new(buf);
        let (decoded, id): (Request, _) = read_message_with_id(&mut cursor).unwrap();

        assert!(matches!(decoded, Request::Ping));
        assert_eq!(id, Some("c-42".to_string()));
    }

    #[test]
    fn test_read_message_without_id_backward_compat() {
        // Write with old write_message (no request_id), read with read_message_with_id
        let req = Request::Ping;
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let (decoded, id): (Request, _) = read_message_with_id(&mut cursor).unwrap();

        assert!(matches!(decoded, Request::Ping));
        assert_eq!(id, None);
    }

    #[test]
    fn test_write_message_with_id_read_by_old_reader() {
        // Write with request_id, read with old read_message — unknown field ignored
        let req = Request::Ping;
        let mut buf = Vec::new();
        write_message_with_id(&mut buf, &req, Some("test-id")).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded: Request = read_message(&mut cursor).unwrap();
        assert!(matches!(decoded, Request::Ping));
    }

    #[test]
    fn test_non_string_request_id_ignored() {
        // Manually craft JSON with numeric request_id — should be ignored
        let json = r#"{"type":"Ping","request_id":42}"#;
        let json_bytes = json.as_bytes();
        let len = json_bytes.len() as u32;

        let mut buf = Vec::new();
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(json_bytes);

        let mut cursor = Cursor::new(buf);
        let (decoded, id): (Request, _) = read_message_with_id(&mut cursor).unwrap();
        assert!(matches!(decoded, Request::Ping));
        assert_eq!(id, None);
    }

    #[test]
    fn test_search_response_with_request_id() {
        let resp = Response::Search(SearchResponse {
            matches: vec![SearchMatchData {
                path: PathBuf::from("src/main.rs"),
                line_number: 42,
                score: 1.5,
            }],
            duration_ms: 12.5,
            cached: false,
            resolved_root: Some(PathBuf::from("/project")),
        });

        let mut buf = Vec::new();
        write_message_with_id(&mut buf, &resp, Some("req-99")).unwrap();

        let mut cursor = Cursor::new(buf);
        let (decoded, id): (Response, _) = read_message_with_id(&mut cursor).unwrap();

        assert_eq!(id, Some("req-99".to_string()));
        match decoded {
            Response::Search(sr) => {
                assert_eq!(sr.matches.len(), 1);
                assert_eq!(sr.matches[0].line_number, 42);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_content_search_response_resolved_root_default() {
        // Old server response without resolved_root should default to None
        let json =
            r#"{"type":"ContentSearch","matches":[],"duration_ms":1.0,"files_with_matches":0}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::ContentSearch(cs) => {
                assert_eq!(cs.resolved_root, None);
            }
            _ => panic!("Wrong variant"),
        }
    }

    #[test]
    fn test_reloaded_response_resolved_root() {
        // Reloaded without resolved_root (backward compat)
        let json = r#"{"type":"Reloaded","success":true,"message":"ok"}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::Reloaded { resolved_root, .. } => {
                assert_eq!(resolved_root, None);
            }
            _ => panic!("Wrong variant"),
        }

        // Reloaded with resolved_root
        let json =
            r#"{"type":"Reloaded","success":true,"message":"ok","resolved_root":"/tmp/test"}"#;
        let resp: Response = serde_json::from_str(json).unwrap();
        match resp {
            Response::Reloaded { resolved_root, .. } => {
                assert_eq!(resolved_root, Some(PathBuf::from("/tmp/test")));
            }
            _ => panic!("Wrong variant"),
        }
    }
}
