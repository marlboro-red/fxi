import * as path from "path";
import * as os from "os";

/**
 * Resolve the fxi daemon Unix socket path.
 * Mirrors the logic in src/server/mod.rs get_socket_path().
 *
 * Priority:
 * 1. $XDG_RUNTIME_DIR/fxi.sock
 * 2. $HOME/.local/run/fxi.sock
 * 3. /tmp/fxi-{uid}.sock
 */
export function getSocketPath(): string {
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
