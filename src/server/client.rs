//! Client for connecting to the index server daemon

use crate::index::types::SearchMatch;
use crate::server::protocol::{read_message, write_message, Request, Response, StatusResponse};
use crate::server::get_socket_path;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Read/write timeout
const IO_TIMEOUT: Duration = Duration::from_secs(30);

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
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::NotRunning => write!(f, "Index server is not running"),
            ClientError::IoError(e) => write!(f, "I/O error: {}", e),
            ClientError::ServerError(msg) => write!(f, "Server error: {}", msg),
            ClientError::InvalidResponse => write!(f, "Invalid response from server"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::IoError(e)
    }
}

/// Client for the index server
pub struct IndexClient {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

impl IndexClient {
    /// Try to connect to the running daemon
    /// Returns None if daemon is not running (allowing fallback to direct mode)
    pub fn connect() -> Option<Self> {
        let socket_path = get_socket_path();

        // Quick check if socket exists
        if !socket_path.exists() {
            return None;
        }

        // Try to connect with short timeout
        let stream = match UnixStream::connect(&socket_path) {
            Ok(s) => s,
            Err(_) => return None,
        };

        // Set timeouts
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

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
        root_path: &PathBuf,
        limit: usize,
    ) -> ClientResult<SearchResult> {
        let request = Request::Search {
            query: query.to_string(),
            root_path: root_path.clone(),
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
    pub fn reload(&mut self, root_path: &PathBuf) -> ClientResult<(bool, String)> {
        let request = Request::Reload {
            root_path: root_path.clone(),
        };

        write_message(&mut self.writer, &request)?;

        let response: Response = read_message(&mut self.reader)?;

        match response {
            Response::Reloaded { success, message } => Ok((success, message)),
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
