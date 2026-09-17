//! Unix socket transport for the fxi daemon.
//!
//! Accepts connections, frames protocol messages (with request-id
//! pipelining), and delegates every request to
//! [`IndexServer::handle_request`] in `daemon_core`.

use crate::server::daemon_core::IndexServer;
use crate::server::protocol::{Request, Response, read_message_with_id, write_message_with_id};
use crate::server::{get_pid_path, get_socket_path};
use anyhow::{Context, Result};
use std::fs;
use std::io::{BufReader, BufWriter};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// Connection timeout
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum concurrent connection handlers
const MAX_CONCURRENT_CONNECTIONS: u64 = 64;

/// Default maximum pipelined requests per connection
const DEFAULT_MAX_PIPELINED: usize = 32;

/// Read per-connection pipelining limit from env or use default
fn max_pipelined() -> usize {
    std::env::var("FXI_MAX_PIPELINED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_PIPELINED)
}

impl IndexServer {
    pub fn run(self: &Arc<Self>) -> Result<()> {
        let socket_path = get_socket_path();
        let pid_path = get_pid_path();

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Remove stale socket file
        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }

        // Write PID file
        fs::write(&pid_path, format!("{}", std::process::id()))?;

        // Bind to socket
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind to {}", socket_path.display()))?;

        // Set socket permissions (user only)
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        }

        eprintln!("fxid: listening on {}", socket_path.display());

        // Start watcher processor thread
        let server_for_watcher = Arc::clone(self);
        let watcher_processor = thread::spawn(move || {
            server_for_watcher.run_watcher_processor();
        });

        listener.set_nonblocking(true)?;

        // Accept connections with concurrency limit
        let active_connections = Arc::new(AtomicU64::new(0));
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    // macOS inherits O_NONBLOCK from the listener; framed
                    // connection readers require blocking I/O with timeouts.
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("fxid: cannot configure connection: {error}");
                        continue;
                    }
                    // Check connection limit
                    if active_connections.load(Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
                        eprintln!("fxid: too many connections, rejecting");
                        continue;
                    }

                    // Set timeout
                    let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

                    // Handle in new thread
                    let server = Arc::clone(self);
                    let conn_count = Arc::clone(&active_connections);
                    conn_count.fetch_add(1, Ordering::Relaxed);
                    thread::spawn(move || {
                        if let Err(e) = server.handle_connection(stream) {
                            eprintln!("fxid: connection error: {}", e);
                        }
                        conn_count.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    eprintln!("fxid: accept error: {}", e);
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }

        // Stop all watchers
        self.stop_all_watchers();

        // Wait for watcher processor to finish
        let _ = watcher_processor.join();

        let reply_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !self.shutdown_reply_sent.load(Ordering::Acquire)
            && std::time::Instant::now() < reply_deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        // Cleanup
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(&pid_path);

        Ok(())
    }

    /// Handle a single client connection with pipelining support.
    ///
    /// Uses `std::thread::scope` to spawn a writer thread and per-request
    /// handler threads. Requests are read with `read_message_with_id` and
    /// responses are written with `write_message_with_id`, preserving the
    /// optional `request_id` for client-side correlation.
    fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        self.handle_connection_with_timeout(stream, CONNECTION_TIMEOUT)
    }

    fn handle_connection_with_timeout(&self, stream: UnixStream, timeout: Duration) -> Result<()> {
        let reader_stream = stream.try_clone()?;
        let _ = reader_stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));

        let (tx, rx) = std::sync::mpsc::channel::<(Response, Option<String>)>();
        let max_handlers = max_pipelined();
        let active = std::sync::atomic::AtomicUsize::new(0);

        let mut saw_shutdown = false;
        std::thread::scope(|s| {
            // Writer thread: drains the channel and writes responses
            s.spawn(move || {
                let mut writer = BufWriter::new(stream);
                while let Ok((response, request_id)) = rx.recv() {
                    if write_response(&mut writer, &response, request_id.as_deref()).is_err() {
                        let _ = writer.get_ref().shutdown(std::net::Shutdown::Both);
                        break;
                    }
                }
            });

            // Reader loop: reads requests and spawns handler threads
            let mut reader = BufReader::new(reader_stream);
            loop {
                let (request, request_id): (Request, _) = match read_message_with_id(&mut reader) {
                    Ok(r) => r,
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => {
                        let _ = tx.send((
                            Response::Error {
                                message: format!("Invalid request: {}", e),
                            },
                            None,
                        ));
                        break; // Framing state is lost after partial reads or oversized frames.
                    }
                };

                // Concurrency limit
                if active.fetch_add(1, Ordering::Relaxed) >= max_handlers {
                    active.fetch_sub(1, Ordering::Relaxed);
                    let _ = tx.send((
                        Response::Error {
                            message: "Too many concurrent requests".into(),
                        },
                        request_id,
                    ));
                    continue;
                }

                let is_shutdown = matches!(request, Request::Shutdown);
                saw_shutdown |= is_shutdown;
                let tx = tx.clone();
                let active = &active;

                s.spawn(move || {
                    let response = self.handle_request(request);
                    let _ = tx.send((response, request_id));
                    active.fetch_sub(1, Ordering::Relaxed);
                });

                if is_shutdown {
                    break;
                }
            }

            drop(tx); // signal writer to finish after in-flight handlers complete
        });

        if saw_shutdown {
            self.shutdown_reply_sent.store(true, Ordering::Release);
        }
        Ok(())
    }
}

/// Serialization failures occur before any frame bytes are written, so a small
/// protocol error can still be delivered. Transport errors cannot be retried.
fn write_response(
    writer: &mut impl std::io::Write,
    response: &Response,
    request_id: Option<&str>,
) -> std::io::Result<()> {
    let result = write_message_with_id(writer, response, request_id);
    if let Err(error) = &result
        && error.kind() == std::io::ErrorKind::InvalidData
    {
        let _ = write_message_with_id(
            writer,
            &Response::Error {
                message: format!("Cannot encode response: {error}"),
            },
            request_id,
        );
    }
    result
}

/// Daemonize the current process
pub fn daemonize(watch: bool) -> Result<()> {
    // Fork using double-fork technique for proper daemonization
    match unsafe { libc::fork() } {
        -1 => anyhow::bail!("First fork failed"),
        0 => {
            // Child process
            // Create new session
            if unsafe { libc::setsid() } == -1 {
                anyhow::bail!("setsid failed");
            }

            // Second fork to prevent acquiring a controlling terminal
            match unsafe { libc::fork() } {
                -1 => anyhow::bail!("Second fork failed"),
                0 => {
                    // Grandchild - this becomes the daemon
                    // Close standard file descriptors
                    unsafe {
                        libc::close(0);
                        libc::close(1);
                        libc::close(2);

                        // Redirect to /dev/null
                        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                        if null != -1 {
                            libc::dup2(null, 0);
                            libc::dup2(null, 1);
                            libc::dup2(null, 2);
                            if null > 2 {
                                libc::close(null);
                            }
                        }
                    }

                    // Change to root directory to avoid holding mounts
                    let _ = std::env::set_current_dir("/");

                    // Now run the server
                    let server = IndexServer::new(watch);
                    if let Err(e) = server.run() {
                        // Can't really report this since stdout is closed
                        // Write to user-specific path to avoid symlink attacks on /tmp
                        if let Some(data_dir) = dirs::data_local_dir() {
                            let log_dir = data_dir.join("fxi");
                            let _ = fs::create_dir_all(&log_dir);
                            let _ = fs::write(log_dir.join("fxid-error.log"), format!("{}", e));
                        }
                    }
                    std::process::exit(0);
                }
                _ => {
                    // First child exits immediately
                    std::process::exit(0);
                }
            }
        }
        _ => {
            // Parent process - wait for first child then exit
            unsafe {
                let mut status: libc::c_int = 0;
                libc::wait(&mut status);
            }
            Ok(())
        }
    }
}

/// Start the daemon in foreground (for debugging)
pub fn run_foreground(watch: bool) -> Result<()> {
    let server = IndexServer::new(watch);
    server.run()
}

fn positive_pid(value: &str) -> Result<i32> {
    let pid: i32 = value.trim().parse()?;
    anyhow::ensure!(
        pid > 0,
        "Invalid daemon PID; refusing to signal a process group"
    );
    Ok(pid)
}

/// Stop the running daemon
pub fn stop_daemon() -> Result<bool> {
    let pid_path = get_pid_path();

    if !pid_path.exists() {
        return Ok(false);
    }

    let pid_str = fs::read_to_string(&pid_path)?;
    let pid = positive_pid(&pid_str)?;

    // Send SIGTERM
    unsafe {
        if libc::kill(pid, libc::SIGTERM) == 0 {
            // Wait a bit for graceful shutdown
            thread::sleep(Duration::from_millis(500));

            // Check if still running, send SIGKILL if needed
            if libc::kill(pid, 0) == 0 {
                thread::sleep(Duration::from_secs(1));
                if libc::kill(pid, 0) == 0 {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    // Clean up socket and pid files
    let socket_path = get_socket_path();
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&pid_path);

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn encoding_failure_returns_correlated_error_instead_of_empty_response() {
        use crate::server::protocol::{SearchMatchData, SearchResponse};
        use std::os::unix::ffi::OsStringExt;
        let response = Response::Search(SearchResponse {
            matches: vec![SearchMatchData {
                path: std::ffi::OsString::from_vec(vec![0xff]).into(),
                line_number: 1,
                score: 1.0,
            }],
            duration_ms: 0.0,
            cached: false,
            resolved_root: None,
        });
        let mut wire = Vec::new();
        assert_eq!(
            write_response(&mut wire, &response, Some("query-1"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        let (error, id): (Response, _) =
            read_message_with_id(&mut std::io::Cursor::new(wire)).unwrap();
        assert!(matches!(error, Response::Error { .. }));
        assert_eq!(id.as_deref(), Some("query-1"));
    }

    #[test]
    fn invalid_pids_never_reach_process_signalling() {
        for invalid in ["0", "-1", "not-a-pid", "99999999999999999999"] {
            assert!(positive_pid(invalid).is_err());
        }
        assert_eq!(positive_pid(" 42\n").unwrap(), 42);
    }

    #[test]
    fn idle_and_partial_frames_release_connection() {
        for partial in [false, true] {
            let server = IndexServer::new(false);
            let (stream, mut client) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            if partial {
                client.write_all(&[10, 0]).unwrap();
            }
            let handler = thread::spawn(move || {
                server
                    .handle_connection_with_timeout(stream, Duration::from_millis(20))
                    .unwrap()
            });
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            handler.join().unwrap();
            assert!(!response.is_empty());
        }
    }

    #[test]
    fn oversized_frame_closes_without_resynchronizing_body() {
        let server = IndexServer::new(false);
        let (stream, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .write_all(&(100 * 1024 * 1024 + 1u32).to_le_bytes())
            .unwrap();
        let handler = thread::spawn(move || server.handle_connection(stream).unwrap());
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        handler.join().unwrap();
        assert!(String::from_utf8_lossy(&response).contains("Message too large"));
    }
}
