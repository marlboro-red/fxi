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

        // Accept connections with concurrency limit
        let active_connections = Arc::new(AtomicU64::new(0));
        for stream in listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            match stream {
                Ok(stream) => {
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
                Err(e) => {
                    eprintln!("fxid: accept error: {}", e);
                }
            }
        }

        // Stop all watchers
        self.stop_all_watchers();

        // Wait for watcher processor to finish
        let _ = watcher_processor.join();

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
        let reader_stream = stream.try_clone()?;
        let _ = reader_stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));

        let (tx, rx) = std::sync::mpsc::channel::<(Response, Option<String>)>();
        let max_handlers = max_pipelined();
        let active = std::sync::atomic::AtomicUsize::new(0);

        std::thread::scope(|s| {
            // Writer thread: drains the channel and writes responses
            s.spawn(move || {
                let mut writer = BufWriter::new(stream);
                while let Ok((response, request_id)) = rx.recv() {
                    if write_message_with_id(&mut writer, &response, request_id.as_deref()).is_err()
                    {
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
                        continue;
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

        Ok(())
    }
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

/// Stop the running daemon
pub fn stop_daemon() -> Result<bool> {
    let pid_path = get_pid_path();

    if !pid_path.exists() {
        return Ok(false);
    }

    let pid_str = fs::read_to_string(&pid_path)?;
    let pid: i32 = pid_str.trim().parse()?;

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
