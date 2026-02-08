# fxi Daemon Server API

This document describes the wire protocol and API for the fxi daemon server, enabling other applications to integrate with fxi for fast code search.

## Transport

### Protocol

The daemon uses a **length-prefixed JSON** protocol over local IPC:

```
┌──────────────────┬──────────────────────────┐
│ 4 bytes (u32 LE) │ N bytes (UTF-8 JSON)     │
│ message length   │ message payload           │
└──────────────────┴──────────────────────────┘
```

- **Length prefix**: 4 bytes, unsigned 32-bit integer, **little-endian**
- **Payload**: UTF-8 encoded JSON
- **Max message size**: 100 MB
- **I/O timeout**: 30 seconds per read/write

### Connection

See [Socket Path Discovery](#socket-path-discovery) above for the full resolution order.

### Socket Path Discovery

Third-party clients need to know where the daemon is listening. Two mechanisms simplify this:

**1. `FXI_SOCKET` environment variable** (highest priority)

If set, all path-resolution logic is skipped and this value is used directly:

```bash
export FXI_SOCKET=/tmp/custom.sock
# All fxi commands and clients will use /tmp/custom.sock
```

**2. `fxi daemon socket-path` CLI command**

Prints the resolved socket/pipe path to stdout. Clients can shell out once at startup:

```bash
# Shell out to get the path
FXI_SOCKET=$(fxi daemon socket-path)

# Python
import subprocess
socket_path = subprocess.check_output(["fxi", "daemon", "socket-path"]).decode().strip()

# Node.js
const { execSync } = require("child_process");
const socketPath = execSync("fxi daemon socket-path").toString().trim();
```

**3. Built-in fallback chains** (when `FXI_SOCKET` is not set)

Unix / macOS — Unix domain socket, checked in order:

| Priority | Path |
|----------|------|
| 1 | `$FXI_SOCKET` (env var override) |
| 2 | `$XDG_RUNTIME_DIR/fxi.sock` |
| 3 | `~/.local/run/fxi.sock` |
| 4 | `/tmp/fxi-{uid}.sock` |

Windows — Named pipe:

| Priority | Name |
|----------|------|
| 1 | `$FXI_SOCKET` (env var override) |
| 2 | `\\.\pipe\fxi-{USERNAME}` |
| 3 | `\\.\pipe\fxi` |

### Path Resolution

The `root_path` field in `Search`, `ContentSearch`, and `Reload` requests is **optional**. The daemon resolves which index to use via three strategies, tried in order:

1. **Exact root path** — If `root_path` matches a loaded (or loadable) index root exactly, it is used directly. This is the original behavior.

2. **Subdirectory path** — If `root_path` points to a subdirectory of an indexed codebase, the daemon walks up to find the codebase root (`.git` directory or indexed parent). For example, sending `root_path: "/home/user/project/src/utils"` resolves to `/home/user/project`.

3. **Omitted** — If `root_path` is omitted (or `null`), the daemon checks how many indexes are loaded:
   - **Exactly one** — uses that index automatically.
   - **Zero** — returns an error: `"No indexes loaded; root_path is required"`.
   - **Two or more** — returns an error listing the loaded roots: `"Ambiguous: 2 indexes loaded; specify root_path. Loaded: /home/user/project1, /home/user/project2"`.

All three search/reload responses include a `resolved_root` field containing the absolute path the daemon resolved to. Clients can cache this value to avoid repeated resolution.

### Authentication

None. Access is controlled by filesystem permissions (socket is `0o600` on Unix, per-user pipe on Windows).

### Connection Lifecycle

The connection is persistent — multiple request/response pairs can be sent over the same connection sequentially. Each request gets exactly one response.

```
Client                              Server
  │                                   │
  ├─── connect ──────────────────────►│
  │                                   │
  │  ┌─ Hello (optional) ───────────►│
  │  │◄── Hello ─────────────────────┤
  │  │                                │
  │  │─ request 1 ───────────────────►│
  │  │◄── response 1 ─────────────────┤
  │  │                                │
  │  │─ request 2 ───────────────────►│
  │  │◄── response 2 ─────────────────┤
  │  └                                │
  │                                   │
  └─── disconnect ───────────────────►│
```

### Protocol Versioning

The protocol uses a single integer version number (`PROTOCOL_VERSION`, currently `2`). The version is bumped only on **breaking** changes (field removal/rename, semantic changes, wire format changes). Adding new optional fields or new request types does **not** require a bump.

Two mechanisms expose the version:

1. **Hello handshake** — optional first message after connect. Client sends its protocol version, server responds with its version + software version. Lets clients fail fast on mismatch.
2. **StatusResponse fields** — `protocol_version` and `server_version` are included in every Status response.

#### Backwards Compatibility

| Scenario | Behavior |
|----------|----------|
| Old client + New server | No Hello sent, works as before. New StatusResponse fields ignored by client. |
| New client + Old server | Hello returns Error (unknown variant), client knows it's pre-versioning. StatusResponse version fields default to `0`/`""`. |
| New client + New server | Hello succeeds, versions compared. |

---

## Message Format

All requests and responses are JSON objects with a `"type"` discriminator field (serde tagged enum).

### Requests

```typescript
type Request =
  | { type: "Search";        query: string; root_path?: string; limit: number }
  | { type: "ContentSearch"; pattern: string; root_path?: string; limit: number; options: ContentSearchOptions }
  | { type: "Status" }
  | { type: "Reload";        root_path?: string }
  | { type: "Shutdown" }
  | { type: "Ping" }
  | { type: "Hello";         protocol_version: number }
```

### Responses

```typescript
type Response =
  | { type: "Search";        matches: SearchMatchData[]; duration_ms: number; cached: boolean; resolved_root?: string }
  | { type: "ContentSearch"; matches: ContentMatch[]; duration_ms: number; files_with_matches: number; resolved_root?: string }
  | { type: "Status";        uptime_secs: number; indexes_loaded: number; total_docs: number; queries_served: number; cache_hit_rate: number; memory_bytes: number; loaded_roots: string[]; protocol_version?: number; server_version?: string }
  | { type: "Reloaded";      success: boolean; message: string; resolved_root?: string }
  | { type: "ShuttingDown" }
  | { type: "Pong" }
  | { type: "Error";         message: string }
  | { type: "Hello";         protocol_version: number; server_version: string }
```

Any request can return an `Error` response.

---

## API Reference

### Search

Index-based full-text search using the fxi query syntax (supports AND, OR, NOT, phrase matching, proximity, regex, file/path/extension filters, etc.).

**Request**

```json
{
  "type": "Search",
  "query": "fn main",
  "root_path": "/home/user/project",
  "limit": 100
}
```

| Field | Type | Description |
|-------|------|-------------|
| `query` | string | fxi query string (see [Query Syntax](#query-syntax)) |
| `root_path` | string? | Absolute path to the indexed codebase root (optional — see [Path Resolution](#path-resolution)) |
| `limit` | number | Max results to return. `0` = use the query's `top:N` limit or server default |

**Response**

```json
{
  "type": "Search",
  "matches": [
    {
      "path": "src/main.rs",
      "line_number": 10,
      "score": 2.5
    }
  ],
  "duration_ms": 12.3,
  "cached": false,
  "resolved_root": "/home/user/project"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `matches` | SearchMatchData[] | Array of matches |
| `matches[].path` | string | File path relative to `root_path` |
| `matches[].line_number` | number (u32) | 1-based line number |
| `matches[].score` | number (f32) | Relevance score (higher = better) |
| `duration_ms` | number (f64) | Server-side search time in milliseconds |
| `cached` | boolean | `true` if result was served from cache |
| `resolved_root` | string? | Absolute path of the codebase root the server resolved to |

---

### ContentSearch

Regex/literal pattern search with context lines (ripgrep-like). Searches file contents directly using the index for acceleration.

**Request**

```json
{
  "type": "ContentSearch",
  "pattern": "TODO.*@\\w+",
  "root_path": "/home/user/project",
  "limit": 50,
  "options": {
    "context_before": 2,
    "context_after": 2,
    "case_insensitive": false,
    "files_only": false
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `pattern` | string | Search pattern (regex or literal) |
| `root_path` | string? | Absolute path to the indexed codebase root (optional — see [Path Resolution](#path-resolution)) |
| `limit` | number | Max results. `0` = up to 10,000,000 (server cap) |
| `options.context_before` | number (u32) | Lines of context before each match |
| `options.context_after` | number (u32) | Lines of context after each match |
| `options.case_insensitive` | boolean | Case-insensitive matching |
| `options.files_only` | boolean | Only return first match per file (optimized path, for `-l` mode) |

**Response**

```json
{
  "type": "ContentSearch",
  "matches": [
    {
      "path": "src/main.rs",
      "line_number": 42,
      "line_content": "  // TODO @alice fix this",
      "match_start": 5,
      "match_end": 19,
      "context_before": [[40, "fn process() {"], [41, "  let x = 1;"]],
      "context_after": [[43, "  println!(\"done\");"], [44, "}"]]
    }
  ],
  "duration_ms": 25.5,
  "files_with_matches": 3
}
```

| Field | Type | Description |
|-------|------|-------------|
| `matches` | ContentMatch[] | Array of content matches |
| `matches[].path` | string | File path relative to `root_path` |
| `matches[].line_number` | number (u32) | 1-based line number of the match |
| `matches[].line_content` | string | Full text of the matching line |
| `matches[].match_start` | number | Byte offset of match start within the line |
| `matches[].match_end` | number | Byte offset of match end within the line |
| `matches[].context_before` | [number, string][] | Context lines before: `[line_number, content]` tuples |
| `matches[].context_after` | [number, string][] | Context lines after: `[line_number, content]` tuples |
| `duration_ms` | number (f64) | Server-side search time in milliseconds |
| `files_with_matches` | number | Count of unique files containing matches |
| `resolved_root` | string? | Absolute path of the codebase root the server resolved to |

---

### Status

Health check and server statistics.

**Request**

```json
{ "type": "Status" }
```

**Response**

```json
{
  "type": "Status",
  "uptime_secs": 3600,
  "indexes_loaded": 2,
  "total_docs": 150000,
  "queries_served": 1250,
  "cache_hit_rate": 0.756,
  "memory_bytes": 10485760,
  "loaded_roots": ["/home/user/project1", "/home/user/project2"],
  "protocol_version": 2,
  "server_version": "0.1.0"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `uptime_secs` | number (u64) | Seconds since daemon started |
| `indexes_loaded` | number | Number of indexes currently in memory |
| `total_docs` | number (u32) | Total documents across all loaded indexes |
| `queries_served` | number (u64) | Total queries handled since start |
| `cache_hit_rate` | number (f32) | Cache hit rate, `0.0` to `1.0` |
| `memory_bytes` | number (u64) | Approximate memory usage in bytes |
| `loaded_roots` | string[] | Absolute paths of all loaded codebase roots |
| `protocol_version` | number (u32) | Protocol version (`0` if server predates versioning) |
| `server_version` | string | Server software version (empty if server predates versioning) |

---

### Reload

Force the daemon to reload the index for a codebase from disk. Clears the query cache for that index.

**Request**

```json
{
  "type": "Reload",
  "root_path": "/home/user/project"
}
```

`root_path` is optional — see [Path Resolution](#path-resolution).

**Response**

```json
{
  "type": "Reloaded",
  "success": true,
  "message": "Reloaded 150000 files",
  "resolved_root": "/home/user/project"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `success` | boolean | Whether the reload succeeded |
| `message` | string | Human-readable status message |
| `resolved_root` | string? | Absolute path of the codebase root the server resolved to |

---

### Shutdown

Graceful shutdown. The server flushes pending changes, closes file watchers, cleans up the socket/pipe and PID file, then exits.

**Request**

```json
{ "type": "Shutdown" }
```

**Response**

```json
{ "type": "ShuttingDown" }
```

---

### Ping

Lightweight connection test with no payload.

**Request**

```json
{ "type": "Ping" }
```

**Response**

```json
{ "type": "Pong" }
```

---

### Hello

Optional protocol version handshake. Should be the first message after connecting. If the server does not support Hello (pre-versioning), it returns an Error response with "unknown variant" — the client should treat this as protocol version 0.

**Request**

```json
{
  "type": "Hello",
  "protocol_version": 2
}
```

| Field | Type | Description |
|-------|------|-------------|
| `protocol_version` | number (u32) | Client's protocol version |

**Response**

```json
{
  "type": "Hello",
  "protocol_version": 2,
  "server_version": "0.1.0"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `protocol_version` | number (u32) | Server's protocol version |
| `server_version` | string | Server software version (e.g. `"0.1.0"`) |

---

### Error

Any request can produce an error response instead of the expected response type.

```json
{
  "type": "Error",
  "message": "Invalid path: No such file or directory"
}
```

Common error causes:
- Path does not exist or is not canonicalizable
- Index not found for the given `root_path`
- Query parse error (malformed query syntax)
- Search execution failure
- Message exceeds 100 MB size limit
- Malformed JSON payload

---

## Query Syntax

The `query` field in `Search` requests supports fxi's full query syntax:

| Syntax | Description | Example |
|--------|-------------|---------|
| `foo bar` | AND — both terms must match | `"error handler"` |
| `"exact phrase"` | Phrase match | `"\"fn main\""` |
| `foo \| bar` | OR — either term | `"TODO \| FIXME"` |
| `-foo` | NOT — exclude term | `"error -debug"` |
| `(a \| b) c` | Grouping | `"(read \| write) file"` |
| `near:a,b,N` | Proximity — terms within N lines | `"near:async,await,5"` |
| `re:/pattern/` | Regex | `"re:/fn\\s+\\w+/"` |
| `file:name` | File name contains | `"file:config"` |
| `file:*.ext` | File name glob | `"file:*.json"` |
| `ext:rs` | File extension | `"ext:rs"` |
| `path:glob` | Path glob | `"path:src/utils/*"` |
| `lang:name` | Language filter | `"lang:rust"` |
| `size:>N` | File size filter (bytes) | `"size:>1000"` |
| `line:A-B` | Line range filter | `"line:100-200"` |
| `mtime:>date` | Modified time filter | `"mtime:>2024-01-01"` |
| `sort:recency` | Sort by modification time | `"sort:recency"` |
| `top:N` | Limit results | `"top:100"` |

---

## Example: Python Client

```python
import socket
import struct
import json
import os

def get_socket_path():
    # Highest priority: FXI_SOCKET env var override
    override = os.environ.get("FXI_SOCKET")
    if override:
        return override
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg:
        return os.path.join(xdg, "fxi.sock")
    home = os.path.expanduser("~")
    return os.path.join(home, ".local", "run", "fxi.sock")

def send_request(sock, request):
    payload = json.dumps(request).encode("utf-8")
    sock.sendall(struct.pack("<I", len(payload)))
    sock.sendall(payload)

def read_response(sock):
    length_bytes = sock.recv(4)
    if len(length_bytes) < 4:
        raise ConnectionError("Connection closed")
    length = struct.unpack("<I", length_bytes)[0]
    data = b""
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            raise ConnectionError("Connection closed")
        data += chunk
    return json.loads(data.decode("utf-8"))

# Connect
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(30)
sock.connect(get_socket_path())

# Hello (optional version handshake)
send_request(sock, {"type": "Hello", "protocol_version": 2})
hello = read_response(sock)
if hello["type"] == "Hello":
    print(f"Server protocol: v{hello['protocol_version']}, version: {hello['server_version']}")
else:
    print("Server predates protocol versioning")

# Ping
send_request(sock, {"type": "Ping"})
print(read_response(sock))  # {"type": "Pong"}

# Search (root_path is optional — omit if only one index is loaded)
send_request(sock, {
    "type": "Search",
    "query": "fn main",
    "root_path": "/home/user/project",
    "limit": 10
})
result = read_response(sock)
print(f"Resolved root: {result.get('resolved_root', 'N/A')}")
for match in result["matches"]:
    print(f"{match['path']}:{match['line_number']} (score: {match['score']})")

# Search without root_path (uses single loaded index)
send_request(sock, {
    "type": "Search",
    "query": "fn main",
    "limit": 10
})
result = read_response(sock)
print(f"Resolved root: {result['resolved_root']}")

# Content search with context
send_request(sock, {
    "type": "ContentSearch",
    "pattern": "TODO",
    "root_path": "/home/user/project",
    "limit": 50,
    "options": {
        "context_before": 2,
        "context_after": 2,
        "case_insensitive": True,
        "files_only": False
    }
})
result = read_response(sock)
for match in result["matches"]:
    print(f"{match['path']}:{match['line_number']}: {match['line_content']}")

# Status
send_request(sock, {"type": "Status"})
status = read_response(sock)
print(f"Uptime: {status['uptime_secs']}s, Indexes: {status['indexes_loaded']}")

sock.close()
```

## Example: Node.js Client

```javascript
const net = require("net");
const path = require("path");
const os = require("os");

function getSocketPath() {
  // Highest priority: FXI_SOCKET env var override
  const override = process.env.FXI_SOCKET;
  if (override) return override;
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) return path.join(xdg, "fxi.sock");
  return path.join(os.homedir(), ".local", "run", "fxi.sock");
}

function createClient() {
  const sock = net.createConnection(getSocketPath());
  let buffer = Buffer.alloc(0);
  let pending = null;

  sock.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    while (buffer.length >= 4) {
      const length = buffer.readUInt32LE(0);
      if (buffer.length < 4 + length) break;
      const payload = buffer.subarray(4, 4 + length);
      buffer = buffer.subarray(4 + length);
      const response = JSON.parse(payload.toString("utf-8"));
      if (pending) {
        const resolve = pending;
        pending = null;
        resolve(response);
      }
    }
  });

  function send(request) {
    return new Promise((resolve, reject) => {
      pending = resolve;
      const payload = Buffer.from(JSON.stringify(request), "utf-8");
      const header = Buffer.alloc(4);
      header.writeUInt32LE(payload.length);
      sock.write(Buffer.concat([header, payload]));
    });
  }

  return { send, close: () => sock.end() };
}

// Usage
(async () => {
  const client = createClient();

  // Hello (optional version handshake)
  const hello = await client.send({ type: "Hello", protocol_version: 2 });
  if (hello.type === "Hello") {
    console.log(`Server protocol: v${hello.protocol_version}, version: ${hello.server_version}`);
  } else {
    console.log("Server predates protocol versioning");
  }

  const pong = await client.send({ type: "Ping" });
  console.log(pong); // { type: "Pong" }

  // Search with explicit root_path
  const result = await client.send({
    type: "ContentSearch",
    pattern: "TODO",
    root_path: "/home/user/project",
    limit: 10,
    options: { context_before: 0, context_after: 0, case_insensitive: false, files_only: false }
  });
  console.log(`Found ${result.files_with_matches} files with matches`);
  console.log(`Resolved root: ${result.resolved_root}`);

  // Search without root_path (uses single loaded index)
  const result2 = await client.send({
    type: "Search",
    query: "main",
    limit: 10
  });
  console.log(`Resolved root: ${result2.resolved_root}`);

  client.close();
})();
```
