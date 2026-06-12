//! Windows named-pipe transport for the fxi daemon.
//!
//! Accepts pipe connections, frames protocol messages, and delegates every
//! request to [`IndexServer::handle_request`] in `daemon_core`.
//!
//! Windows synchronous named pipes do not support concurrent ReadFile and
//! WriteFile on one handle, so requests on a connection are processed
//! sequentially (no pipelining); request IDs are still echoed back.

use crate::server::daemon_core::IndexServer;
use crate::server::protocol::{Request, Response, read_message_with_id, write_message_with_id};
use crate::server::{get_pid_path, get_pipe_name};
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

/// Connection timeout in milliseconds
const CONNECTION_TIMEOUT_MS: u32 = 30000;

/// Buffer size for named pipe
const PIPE_BUFFER_SIZE: u32 = 65536;

/// Maximum concurrent connection handlers
const MAX_CONCURRENT_CONNECTIONS: u64 = 64;

// Windows API constants
const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
const PIPE_TYPE_BYTE: u32 = 0x00000000;
const PIPE_READMODE_BYTE: u32 = 0x00000000;
const PIPE_WAIT: u32 = 0x00000000;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;
const ERROR_PIPE_CONNECTED: u32 = 535;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateNamedPipeW(
        lpName: *const u16,
        dwOpenMode: u32,
        dwPipeMode: u32,
        nMaxInstances: u32,
        nOutBufferSize: u32,
        nInBufferSize: u32,
        nDefaultTimeOut: u32,
        lpSecurityAttributes: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;

    fn ConnectNamedPipe(
        hNamedPipe: *mut std::ffi::c_void,
        lpOverlapped: *mut std::ffi::c_void,
    ) -> i32;

    fn DisconnectNamedPipe(hNamedPipe: *mut std::ffi::c_void) -> i32;

    fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;

    fn GetLastError() -> u32;

    fn OpenProcess(
        dwDesiredAccess: u32,
        bInheritHandle: i32,
        dwProcessId: u32,
    ) -> *mut std::ffi::c_void;

    fn TerminateProcess(hProcess: *mut std::ffi::c_void, uExitCode: u32) -> i32;

    fn ReadFile(
        hFile: *mut std::ffi::c_void,
        lpBuffer: *mut u8,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: *mut u32,
        lpOverlapped: *mut std::ffi::c_void,
    ) -> i32;

    fn WriteFile(
        hFile: *mut std::ffi::c_void,
        lpBuffer: *const u8,
        nNumberOfBytesToWrite: u32,
        lpNumberOfBytesWritten: *mut u32,
        lpOverlapped: *mut std::ffi::c_void,
    ) -> i32;

    fn FlushFileBuffers(hFile: *mut std::ffi::c_void) -> i32;
}

const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

/// A Send-safe wrapper for a Windows HANDLE
#[derive(Clone, Copy)]
struct SendableHandle(isize);

// Safety: Windows HANDLEs can be used from any thread
unsafe impl Send for SendableHandle {}

impl SendableHandle {
    fn from_raw(ptr: *mut std::ffi::c_void) -> Self {
        Self(ptr as isize)
    }

    fn as_raw(&self) -> *mut std::ffi::c_void {
        self.0 as *mut std::ffi::c_void
    }
}

/// Wrapper for Windows handle that implements Read + Write
struct PipeHandle {
    handle: SendableHandle,
}

impl PipeHandle {
    #[allow(dead_code)]
    fn try_clone(&self) -> std::io::Result<Self> {
        // For simplicity, we don't actually clone the handle
        // The server uses separate reader/writer on the same handle
        Ok(Self {
            handle: self.handle,
        })
    }
}

impl Read for PipeHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut bytes_read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle.as_raw(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut bytes_read,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(bytes_read as usize)
        }
    }
}

impl Write for PipeHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut bytes_written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.handle.as_raw(),
                buf.as_ptr(),
                buf.len() as u32,
                &mut bytes_written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(bytes_written as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let ok = unsafe { FlushFileBuffers(self.handle.as_raw()) };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Non-owning pipe writer — does not close the handle on drop.
/// Used when the reader owns the handle lifetime and the writer
/// must not double-close.
struct PipeWriter(SendableHandle);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut bytes_written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.0.as_raw(),
                buf.as_ptr(),
                buf.len() as u32,
                &mut bytes_written,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(bytes_written as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let ok = unsafe { FlushFileBuffers(self.0.as_raw()) };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        let raw = self.handle.as_raw();
        if raw != INVALID_HANDLE_VALUE && !raw.is_null() {
            unsafe {
                CloseHandle(raw);
            }
        }
    }
}

/// Convert a Rust string to a null-terminated wide string
fn to_wide_string(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

impl IndexServer {
    pub fn run(self: &Arc<Self>) -> Result<()> {
        let pipe_name = get_pipe_name();
        let pid_path = get_pid_path();

        // Ensure parent directory exists for PID file
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write PID file
        fs::write(&pid_path, format!("{}", std::process::id()))?;

        eprintln!("fxid: listening on {}", pipe_name);

        // Start watcher processor thread
        let server_for_watcher = Arc::clone(self);
        let watcher_processor = thread::spawn(move || {
            server_for_watcher.run_watcher_processor();
        });

        // Main server loop - create pipe instances and accept connections
        let active_connections = Arc::new(AtomicU64::new(0));
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Create a new pipe instance
            let wide_name = to_wide_string(&pipe_name);
            let pipe_handle = unsafe {
                CreateNamedPipeW(
                    wide_name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    PIPE_BUFFER_SIZE,
                    PIPE_BUFFER_SIZE,
                    CONNECTION_TIMEOUT_MS,
                    ptr::null_mut(),
                )
            };

            if pipe_handle == INVALID_HANDLE_VALUE {
                let err = unsafe { GetLastError() };
                eprintln!("fxid: failed to create pipe: error {}", err);
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            // Wait for client connection
            let connected = unsafe { ConnectNamedPipe(pipe_handle, ptr::null_mut()) };

            if connected == 0 {
                let err = unsafe { GetLastError() };
                if err != ERROR_PIPE_CONNECTED {
                    unsafe {
                        CloseHandle(pipe_handle);
                    }
                    continue;
                }
            }

            if self.shutdown.load(Ordering::Relaxed) {
                unsafe {
                    CloseHandle(pipe_handle);
                }
                break;
            }

            // Check connection limit
            if active_connections.load(Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
                eprintln!("fxid: too many connections, rejecting");
                unsafe {
                    DisconnectNamedPipe(pipe_handle);
                    CloseHandle(pipe_handle);
                }
                continue;
            }

            // Handle connection in new thread
            let server = Arc::clone(self);
            let sendable_handle = SendableHandle::from_raw(pipe_handle);
            let conn_count = Arc::clone(&active_connections);
            conn_count.fetch_add(1, Ordering::Relaxed);
            thread::spawn(move || {
                let handle = PipeHandle {
                    handle: sendable_handle,
                };
                if let Err(e) = server.handle_connection(handle) {
                    eprintln!("fxid: connection error: {}", e);
                }
                conn_count.fetch_sub(1, Ordering::Relaxed);
            });
        }

        // Stop all watchers
        self.stop_all_watchers();

        // Wait for watcher processor to finish
        let _ = watcher_processor.join();

        // Cleanup
        let _ = fs::remove_file(&pid_path);

        Ok(())
    }

    /// Handle a single client connection.
    ///
    /// Windows synchronous named pipes do not support concurrent ReadFile and
    /// WriteFile on the same handle — a pending read blocks all writes. So we
    /// process requests sequentially: read → handle → write → loop. Request
    /// IDs are still echoed back for client-side correlation.
    fn handle_connection(&self, pipe: PipeHandle) -> Result<()> {
        let handle = pipe.handle;
        let mut reader = BufReader::new(pipe);
        let mut writer = BufWriter::new(PipeWriter(handle));

        loop {
            let (request, request_id): (Request, _) = match read_message_with_id(&mut reader) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
                Err(e) => {
                    let resp = Response::Error {
                        message: format!("Invalid request: {}", e),
                    };
                    let _ = write_message_with_id(&mut writer, &resp, None);
                    continue;
                }
            };

            let is_shutdown = matches!(request, Request::Shutdown);
            let response = self.handle_request(request);

            if write_message_with_id(&mut writer, &response, request_id.as_deref()).is_err() {
                break;
            }

            if is_shutdown {
                break;
            }
        }

        unsafe {
            DisconnectNamedPipe(reader.get_ref().handle.as_raw());
        }

        Ok(())
    }
}

pub fn daemonize(watch: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    // Windows process creation flags
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;

    // On Windows, we spawn a detached child process
    let exe = std::env::current_exe()?;

    // Start the server in foreground mode as a detached process
    let mut args = vec!["daemon", "foreground"];
    if watch {
        args.push("--watch");
    }
    Command::new(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| "Failed to spawn daemon process")?;

    // Give it a moment to start
    thread::sleep(Duration::from_millis(100));

    Ok(())
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
    let pid: u32 = pid_str.trim().parse()?;

    // Open the process and terminate it
    unsafe {
        let handle = OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            // Process doesn't exist
            let _ = fs::remove_file(&pid_path);
            return Ok(false);
        }

        let result = TerminateProcess(handle, 0);
        CloseHandle(handle);

        if result == 0 {
            return Ok(false);
        }
    }

    // Wait a bit for process to exit
    thread::sleep(Duration::from_millis(500));

    // Clean up pid file
    let _ = fs::remove_file(&pid_path);

    Ok(true)
}
