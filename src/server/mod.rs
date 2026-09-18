//! Persistent index server for warm searches
//!
//! This module provides a daemon that keeps the search index loaded in memory,
//! allowing for instant searches without cold-start overhead.
//!
//! Architecture:
//! - `fxid` daemon: Loads index, listens on Unix socket (or named pipe on Windows), handles search requests
//! - Client: Connects to socket/pipe, sends queries, receives results
//! - Fallback: If daemon unavailable, falls back to direct index loading

mod admission;
pub mod daemon_core;

#[cfg(unix)]
mod client_unix;
#[cfg(unix)]
pub mod daemon_unix;

#[cfg(windows)]
mod client_windows;
#[cfg(windows)]
pub mod daemon_windows;

pub mod debouncer;
pub mod protocol;
pub mod watcher;

#[cfg(unix)]
pub use client_unix::IndexClient;
#[cfg(unix)]
pub use daemon_unix as daemon;

#[cfg(windows)]
pub use client_windows::IndexClient;
#[cfg(windows)]
pub use daemon_windows as daemon;

mod common;
pub use common::*;

#[cfg(unix)]
mod control_unix;
