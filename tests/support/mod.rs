//! Shared environment for subprocess fixtures. The library's test override is
//! process-local, so CLI children receive explicit config, index, and IPC paths.
use std::path::PathBuf;
use std::process::Command;

pub fn fxi_command(binary: impl AsRef<std::ffi::OsStr>) -> Command {
    let data = fxi::utils::app_data::isolate_test_storage().expect("isolate test storage");
    let indexes = std::env::var_os("FXI_INDEXES")
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("indexes"));
    let mut command = Command::new(binary);
    command
        .env("FXI_APP_DATA", &data)
        .env("FXI_INDEXES", indexes)
        .env("XDG_RUNTIME_DIR", &data);
    #[cfg(unix)]
    command.env("FXI_SOCKET", data.join("unused.sock"));
    #[cfg(windows)]
    command.env(
        "FXI_SOCKET",
        format!(r"\\.\pipe\fxi-tests-{}", std::process::id()),
    );
    command
}
