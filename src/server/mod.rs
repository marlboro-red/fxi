//! Persistent index server for warm searches
//!
//! This module provides a daemon that keeps the search index loaded in memory,
//! allowing for instant searches without cold-start overhead.
//!
//! Architecture:
//! - `fxid` daemon: Loads index, listens on Unix socket (or named pipe on Windows), handles search requests
//! - Client: Connects to socket/pipe, sends queries, receives results
//! - Fallback: If daemon unavailable, falls back to direct index loading

#[cfg(unix)]
mod client_unix;
#[cfg(unix)]
pub mod daemon_unix;

#[cfg(windows)]
mod client_windows;
#[cfg(windows)]
pub mod daemon_windows;

pub mod protocol;

#[cfg(unix)]
pub use client_unix::IndexClient;
#[cfg(unix)]
pub use daemon_unix as daemon;

#[cfg(windows)]
pub use client_windows::IndexClient;
#[cfg(windows)]
pub use daemon_windows as daemon;

use std::path::PathBuf;

/// Get the socket path for the index server (Unix)
#[cfg(unix)]
pub fn get_socket_path() -> PathBuf {
    // Try XDG_RUNTIME_DIR first (most secure, tmpfs-backed)
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("fxi.sock");
    }

    // Fall back to user's home directory
    if let Some(home) = dirs::home_dir() {
        return home.join(".local").join("run").join("fxi.sock");
    }

    // Last resort: /tmp with user ID
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/fxi-{}.sock", uid))
}

/// Get the named pipe name for the index server (Windows)
#[cfg(windows)]
pub fn get_pipe_name() -> String {
    // Use a per-user pipe name based on username
    if let Ok(username) = std::env::var("USERNAME") {
        format!(r"\\.\pipe\fxi-{}", username)
    } else {
        r"\\.\pipe\fxi".to_string()
    }
}

/// Get the PID file path for the daemon
pub fn get_pid_path() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("fxi.pid");
        }

        if let Some(home) = dirs::home_dir() {
            return home.join(".local").join("run").join("fxi.pid");
        }

        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/fxi-{}.pid", uid))
    }

    #[cfg(windows)]
    {
        // Use LOCALAPPDATA or temp directory
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let path = PathBuf::from(local_app_data).join("fxi");
            let _ = std::fs::create_dir_all(&path);
            return path.join("fxi.pid");
        }

        if let Ok(temp) = std::env::var("TEMP") {
            return PathBuf::from(temp).join("fxi.pid");
        }

        PathBuf::from(r"C:\Windows\Temp\fxi.pid")
    }
}

/// Check if the daemon is running
#[cfg(unix)]
pub fn is_daemon_running() -> bool {
    let pid_path = get_pid_path();
    if !pid_path.exists() {
        return false;
    }

    // Read PID and check if process exists
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = pid_str.trim().parse::<i32>()
    {
        // Check if process exists using kill(pid, 0)
        unsafe {
            return libc::kill(pid, 0) == 0;
        }
    }

    false
}

/// Check if the daemon is running (Windows version)
#[cfg(windows)]
pub fn is_daemon_running() -> bool {
    let pid_path = get_pid_path();
    if !pid_path.exists() {
        return false;
    }

    // Read PID and check if process exists
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            // Try to open the process to check if it exists
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::ffi::c_void;
                fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
            }

            const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

            unsafe {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !handle.is_null() {
                    CloseHandle(handle);
                    return true;
                }
            }
        }
    }

    false
}
