//! macOS CLI façade. Keep daemon implementation and its native framework links
//! in the sibling `fxid` executable; search clients share the same wire protocol.

#[path = "client_unix.rs"]
mod client_unix;
#[path = "common.rs"]
mod common;
#[path = "control_unix.rs"]
mod control_unix;
#[path = "launcher_macos.rs"]
pub mod daemon;
#[path = "protocol.rs"]
pub mod protocol;
// Index building shares the daemon's compaction default. The remaining watcher
// types are used by the full library/server, not by this CLI module tree.
#[allow(dead_code)]
#[path = "watcher.rs"]
pub mod watcher;

pub use client_unix::IndexClient;
pub use common::*;
