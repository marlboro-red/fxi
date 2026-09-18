//! Windows named-pipe transport for the fxi daemon.
//!
//! Accepts pipe connections, frames protocol messages, and delegates every
//! request to [`IndexServer::handle_request`] in `daemon_core`.
//!
//! Windows synchronous named pipes do not support concurrent ReadFile and
//! WriteFile on one handle, so requests on a connection are processed
//! sequentially (no pipelining); request IDs are still echoed back.

use crate::server::admission;
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const PIPE_NOWAIT: u32 = 0x00000001;
const ERROR_PIPE_LISTENING: u32 = 536;
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

    fn SetNamedPipeHandleState(
        hNamedPipe: *mut std::ffi::c_void,
        lpMode: *const u32,
        lpMaxCollectionCount: *const u32,
        lpCollectDataTimeout: *const u32,
    ) -> i32;

    fn DisconnectNamedPipe(hNamedPipe: *mut std::ffi::c_void) -> i32;

    fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;

    fn GetLastError() -> u32;
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(
        desired_access: u32,
        inherit_handle: i32,
        thread_id: u32,
    ) -> *mut std::ffi::c_void;
    fn CancelSynchronousIo(thread: *mut std::ffi::c_void) -> i32;

    fn OpenProcess(
        dwDesiredAccess: u32,
        bInheritHandle: i32,
        dwProcessId: u32,
    ) -> *mut std::ffi::c_void;

    fn TerminateProcess(hProcess: *mut std::ffi::c_void, uExitCode: u32) -> i32;

    fn PeekNamedPipe(
        hNamedPipe: *mut std::ffi::c_void,
        lpBuffer: *mut std::ffi::c_void,
        nBufferSize: u32,
        lpBytesRead: *mut u32,
        lpTotalBytesAvail: *mut u32,
        lpBytesLeftThisMessage: *mut u32,
    ) -> i32;

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

impl Read for PipeHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Only this handler reads the pipe. Peek before ReadFile so idle or
        // partial frames cannot retain a connection slot indefinitely.
        let deadline =
            std::time::Instant::now() + Duration::from_millis(CONNECTION_TIMEOUT_MS as u64);
        let available = loop {
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    self.handle.as_raw(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut available,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if available > 0 {
                break available;
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Pipe read timed out",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut bytes_read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle.as_raw(),
                buf.as_mut_ptr(),
                buf.len().min(available as usize) as u32,
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
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
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

            // Poll connection readiness so shutdown never needs an extra client.
            let connected = wait_for_connection(pipe_handle, &self.shutdown);
            let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
            if !connected
                || unsafe { SetNamedPipeHandleState(pipe_handle, &mode, ptr::null(), ptr::null()) }
                    == 0
            {
                unsafe {
                    CloseHandle(pipe_handle);
                }
                continue;
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

        let reply_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !self.shutdown_reply_sent.load(Ordering::Acquire)
            && std::time::Instant::now() < reply_deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
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
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) => {
                    let resp = Response::Error {
                        message: format!("Invalid request: {}", e),
                    };
                    let _ = with_write_timeout(
                        Duration::from_millis(CONNECTION_TIMEOUT_MS as u64),
                        || write_message_with_id(&mut writer, &resp, None),
                    );
                    break;
                }
            };

            let is_shutdown = matches!(request, Request::Shutdown);
            let searching = admission::is_search(&request);
            let _search_permit = if searching {
                admission::searches().try_acquire()
            } else {
                None
            };
            let response = if searching && _search_permit.is_none() {
                admission::overloaded()
            } else {
                self.handle_request(request)
            };

            let written =
                with_write_timeout(Duration::from_millis(CONNECTION_TIMEOUT_MS as u64), || {
                    match write_message_with_id(&mut writer, &response, request_id.as_deref()) {
                        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                            let _ = write_message_with_id(
                                &mut writer,
                                &Response::Error {
                                    message: format!("Cannot encode response: {error}"),
                                },
                                request_id.as_deref(),
                            );
                            Err(error)
                        }
                        result => result,
                    }
                });
            if is_shutdown {
                self.shutdown_reply_sent.store(true, Ordering::Release);
            }
            if written.is_err() || is_shutdown {
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

fn positive_pid(value: &str) -> Result<u32> {
    let pid: u32 = value.trim().parse()?;
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

/// Bound a complete synchronous response, including FlushFileBuffers waiting
/// for the client to drain its pipe. Repeated cancellation closes the race
/// where the deadline expires immediately before WriteFile starts.
fn with_write_timeout(
    timeout: Duration,
    operation: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    const THREAD_TERMINATE: u32 = 0x0001;
    let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, GetCurrentThreadId()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    struct ThreadHandle(SendableHandle);
    impl Drop for ThreadHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0.as_raw());
            }
        }
    }
    struct Complete(std::sync::mpsc::Sender<()>);
    impl Drop for Complete {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let thread_handle = ThreadHandle(SendableHandle::from_raw(handle));
    let (tx, rx) = std::sync::mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(move || {
            let thread_handle = thread_handle;
            if rx.recv_timeout(timeout) != Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                return;
            }
            loop {
                unsafe {
                    CancelSynchronousIo(thread_handle.0.as_raw());
                }
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        let _complete = Complete(tx);
        operation()
    })
}

/// Poll only connection establishment; established pipe I/O remains synchronous.
fn wait_for_connection(pipe_handle: *mut std::ffi::c_void, shutdown: &AtomicBool) -> bool {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }
        if unsafe { ConnectNamedPipe(pipe_handle, ptr::null_mut()) } != 0 {
            // NOWAIT success means the instance became available;
            // only ERROR_PIPE_CONNECTED confirms an actual client.
            continue;
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_PIPE_CONNECTED {
            return true;
        }
        if error != ERROR_PIPE_LISTENING {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_pids_never_reach_process_signalling() {
        for invalid in ["0", "-1", "not-a-pid", "99999999999999999999"] {
            assert!(positive_pid(invalid).is_err());
        }
        assert_eq!(positive_pid(" 42\n").unwrap(), 42);
    }

    #[test]
    fn stalled_client_does_not_hold_a_response_writer_forever() {
        let name = format!(
            r"\\.\pipe\fxi-write-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let wide = to_wide_string(&name);
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                1,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                CONNECTION_TIMEOUT_MS,
                ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let (release, released) = std::sync::mpsc::channel::<()>();
        let client = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let _file = loop {
                match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&name)
                {
                    Ok(file) => break file,
                    Err(error) if std::time::Instant::now() >= deadline => {
                        panic!("cannot connect test pipe: {error}")
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            };
            let _ = released.recv_timeout(Duration::from_secs(3));
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let (cancel, cancellation) = std::sync::mpsc::channel::<()>();
        let accept_timeout = thread::spawn(move || {
            if cancellation.recv_timeout(Duration::from_secs(3)).is_err() {
                stop.store(true, Ordering::Release);
            }
        });
        let connected = wait_for_connection(handle, &shutdown);
        let _ = cancel.send(());
        accept_timeout.join().unwrap();
        let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
        assert!(connected);
        assert_ne!(
            unsafe { SetNamedPipeHandleState(handle, &mode, ptr::null(), ptr::null()) },
            0
        );
        let pipe = PipeHandle {
            handle: SendableHandle::from_raw(handle),
        };
        let mut writer = PipeWriter(pipe.handle);
        let result = with_write_timeout(Duration::from_millis(30), || {
            writer.write_all(&vec![b'x'; PIPE_BUFFER_SIZE as usize * 4])?;
            writer.flush()
        });
        let _ = release.send(());
        client.join().unwrap();
        assert_eq!(result.unwrap_err().raw_os_error(), Some(995)); // ERROR_OPERATION_ABORTED
    }

    #[test]
    fn unconnected_accept_observes_shutdown_without_a_wakeup_client() {
        let name = to_wide_string(&format!(
            r"\\.\pipe\fxi-accept-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                1,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                CONNECTION_TIMEOUT_MS,
                ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            stop.store(true, Ordering::Release);
        });
        let connected = wait_for_connection(handle, &shutdown);
        unsafe {
            CloseHandle(handle);
        }
        worker.join().unwrap();
        assert!(
            !connected,
            "making a NOWAIT pipe available is not a connected client"
        );
    }
}
