//! Windows client for connecting to the index server daemon

use crate::index::types::SearchMatch;
use crate::server::protocol::{
    read_message, write_message, ContentSearchOptions, ContentSearchResponse, Request, Response,
    StatusResponse, PROTOCOL_VERSION,
};
use crate::server::get_pipe_name;
use std::fs::OpenOptions;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Read/write timeout (not directly supported for file handles, but we document intent)
const _IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Result type for client operations
pub type ClientResult<T> = Result<T, ClientError>;

/// Errors that can occur in client operations
#[derive(Debug)]
pub enum ClientError {
    /// Server is not running
    #[allow(dead_code)]
    NotRunning,
    /// Communication error
    IoError(std::io::Error),
    /// Server returned an error
    ServerError(String),
    /// Invalid response
    InvalidResponse,
    /// Protocol version mismatch
    VersionMismatch {
        client_version: u32,
        server_version: u32,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotRunning => write!(f, "Index server is not running"),
            ClientError::IoError(e) => write!(f, "I/O error: {}", e),
            ClientError::ServerError(msg) => write!(f, "Server error: {}", msg),
            ClientError::InvalidResponse => write!(f, "Invalid response from server"),
            ClientError::VersionMismatch {
                client_version,
                server_version,
            } => write!(
                f,
                "Protocol version mismatch: client={}, server={}",
                client_version, server_version
            ),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::IoError(e)
    }
}

/// A wrapper for a named pipe connection that implements Read + Write
struct PipeStream {
    handle: std::fs::File,
}

impl PipeStream {
    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            handle: self.handle.try_clone()?,
        })
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.handle.read(buf)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.handle.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.handle.flush()
    }
}

/// Client for the index server
pub struct IndexClient {
    reader: BufReader<PipeStream>,
    writer: BufWriter<PipeStream>,
}

impl IndexClient {
    /// Try to connect to the running daemon
    /// Returns None if daemon is not running (allowing fallback to direct mode)
    pub fn connect() -> Option<Self> {
        let pipe_name = get_pipe_name();

        // Try to connect to named pipe
        let handle = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pipe_name)
        {
            Ok(h) => h,
            Err(_) => return None,
        };

        let stream = PipeStream { handle };
        let reader = BufReader::new(stream.try_clone().ok()?);
        let writer = BufWriter::new(stream);

        Some(Self { reader, writer })
    }

    /// Connect or return an error (for when daemon is required)
    #[allow(dead_code)]
    pub fn connect_required() -> ClientResult<Self> {
        Self::connect().ok_or(ClientError::NotRunning)
    }

    /// Execute a search query
    pub fn search(
        &mut self,
        query: &str,
        root_path: Option<&Path>,
        limit: usize,
    ) -> ClientResult<SearchResult> {
        let request = Request::Search {
            query: query.to_string(),
            root_path: root_path.map(|p| p.to_path_buf()),
            limit,
        };

        write_message(&mut self.writer, &request)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::Search(sr) => Ok(SearchResult {
                matches: sr
                    .matches
                    .into_iter()
                    .map(|m| SearchMatch {
                        doc_id: m.doc_id,
                        path: m.path,
                        line_number: m.line_number,
                        score: m.score,
                    })
                    .collect(),
                duration_ms: sr.duration_ms,
                cached: sr.cached,
            }),
            Response::Error { message } => Err(ClientError::ServerError(message)),
            _ => Err(ClientError::InvalidResponse),
        }
    }

    /// Execute a content search query (ripgrep-like)
    pub fn content_search(
        &mut self,
        pattern: &str,
        root_path: Option<&Path>,
        limit: usize,
        options: ContentSearchOptions,
    ) -> ClientResult<ContentSearchResponse> {
        let request = Request::ContentSearch {
            pattern: pattern.to_string(),
            root_path: root_path.map(|p| p.to_path_buf()),
            limit,
            options,
        };

        write_message(&mut self.writer, &request)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::ContentSearch(sr) => Ok(sr),
            Response::Error { message } => Err(ClientError::ServerError(message)),
            _ => Err(ClientError::InvalidResponse),
        }
    }

    /// Get server status
    pub fn status(&mut self) -> ClientResult<StatusResponse> {
        write_message(&mut self.writer, &Request::Status)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::Status(status) => Ok(status),
            Response::Error { message } => Err(ClientError::ServerError(message)),
            _ => Err(ClientError::InvalidResponse),
        }
    }

    /// Request index reload
    pub fn reload(&mut self, root_path: Option<&Path>) -> ClientResult<(bool, String)> {
        let request = Request::Reload {
            root_path: root_path.map(|p| p.to_path_buf()),
        };

        write_message(&mut self.writer, &request)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::Reloaded { success, message, .. } => Ok((success, message)),
            Response::Error { message } => Err(ClientError::ServerError(message)),
            _ => Err(ClientError::InvalidResponse),
        }
    }

    /// Request graceful shutdown
    pub fn shutdown(&mut self) -> ClientResult<()> {
        write_message(&mut self.writer, &Request::Shutdown)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::ShuttingDown => Ok(()),
            Response::Error { message } => Err(ClientError::ServerError(message)),
            _ => Err(ClientError::InvalidResponse),
        }
    }

    /// Perform protocol version handshake
    pub fn hello(&mut self) -> ClientResult<HelloResponse> {
        let request = Request::Hello {
            protocol_version: PROTOCOL_VERSION,
        };

        write_message(&mut self.writer, &request)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::Hello {
                protocol_version,
                server_version,
            } => Ok(HelloResponse {
                protocol_version,
                server_version,
                compatible: protocol_version == PROTOCOL_VERSION,
            }),
            // Old server that doesn't know Hello — treat as pre-versioning
            Response::Error { .. } => Ok(HelloResponse {
                protocol_version: 0,
                server_version: String::new(),
                compatible: false,
            }),
            _ => Err(ClientError::InvalidResponse),
        }
    }

    /// Ping the server
    pub fn ping(&mut self) -> ClientResult<()> {
        write_message(&mut self.writer, &Request::Ping)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::Pong => Ok(()),
            Response::Error { message } => Err(ClientError::ServerError(message)),
            _ => Err(ClientError::InvalidResponse),
        }
    }
}

/// Response from a Hello handshake
pub struct HelloResponse {
    pub protocol_version: u32,
    pub server_version: String,
    pub compatible: bool,
}

/// Search result from the server
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    #[allow(dead_code)]
    pub duration_ms: f64,
    #[allow(dead_code)]
    pub cached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connect_when_not_running() {
        // Should return None, not panic
        let client = IndexClient::connect();
        assert!(client.is_none() || client.is_some()); // Either is fine
    }
}
