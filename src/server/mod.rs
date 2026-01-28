//! Persistent index server for warm searches
//!
//! This module provides a daemon that keeps the search index loaded in memory,
//! allowing for instant searches without cold-start overhead.
//!
//! Architecture:
//! - `fxid` daemon: Loads index, listens on Unix socket, handles search requests
//! - Client: Connects to socket, sends queries, receives results
//! - Fallback: If daemon unavailable, falls back to direct index loading

mod client;
pub mod daemon;
mod protocol;

pub use client::IndexClient;

use std::path::PathBuf;

/// Get the socket path for the index server
/// Uses a per-user runtime directory for security
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

/// Get the PID file path for the daemon
pub fn get_pid_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("fxi.pid");
    }

    if let Some(home) = dirs::home_dir() {
        return home.join(".local").join("run").join("fxi.pid");
    }

    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/fxi-{}.pid", uid))
}

/// Check if the daemon is running
pub fn is_daemon_running() -> bool {
    let pid_path = get_pid_path();
    if !pid_path.exists() {
        return false;
    }

    // Read PID and check if process exists
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            // Check if process exists using kill(pid, 0)
            unsafe {
                return libc::kill(pid, 0) == 0;
            }
        }
    }

    false
}
