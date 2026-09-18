//! Process control shared by the CLI and Unix daemon.

use super::{get_pid_path, get_socket_path};
use anyhow::Result;
use std::{fs, thread, time::Duration};

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

    #[test]
    fn invalid_pids_never_reach_process_signalling() {
        for invalid in ["0", "-1", "not-a-pid", "99999999999999999999"] {
            assert!(positive_pid(invalid).is_err());
        }
        assert_eq!(positive_pid(" 42\n").unwrap(), 42);
    }
}
