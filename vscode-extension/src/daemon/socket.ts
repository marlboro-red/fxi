import * as path from "path";
import * as os from "os";

/**
 * Resolve the fxi daemon socket/pipe path.
 * Mirrors the logic in src/server/mod.rs:
 *   - Windows: get_pipe_name()  → \\.\pipe\fxi-{USERNAME}
 *   - Unix:    get_socket_path() → XDG_RUNTIME_DIR/fxi.sock, etc.
 */
export function getSocketPath(): string {
  if (process.platform === "win32") {
    const username = process.env["USERNAME"];
    if (username) {
      return `\\\\.\\pipe\\fxi-${username}`;
    }
    return `\\\\.\\pipe\\fxi`;
  }

  // Unix: socket file
  const xdgRuntime = process.env["XDG_RUNTIME_DIR"];
  if (xdgRuntime) {
    return path.join(xdgRuntime, "fxi.sock");
  }

  const home = os.homedir();
  if (home) {
    return path.join(home, ".local", "run", "fxi.sock");
  }

  // Last resort: /tmp with uid
  const uid = process.getuid?.() ?? 0;
  return `/tmp/fxi-${uid}.sock`;
}
