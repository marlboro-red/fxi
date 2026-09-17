# FXI daemon API

The daemon exposes indexed search and index lifecycle operations over local IPC.
The schema lives in [`src/server/protocol.rs`](../src/server/protocol.rs); query
behavior is defined in [search semantics](SEMANTICS.md). This document describes
the current implementation, including limits that differ between output modes.

## Transport and discovery

Each message is a four-byte **little-endian unsigned length**, followed by that
many bytes of UTF-8 JSON. Read both the header and payload fully: a socket read
can return fewer bytes than requested. The payload limit is **100 MiB
(104,857,600 bytes)** in either direction. It is a byte limit, independent of
result counts; a query below its record limit can still produce an oversized
response. There is no pagination or streaming response protocol.

Use `fxi daemon socket-path` to discover the endpoint instead of reproducing
platform-dependent path logic. `FXI_SOCKET` overrides the endpoint for both the
server and clients. Without that override:

| Platform | Resolution order |
|---|---|
| Unix/macOS | `$XDG_RUNTIME_DIR/fxi.sock`, otherwise `~/.local/run/fxi.sock`, otherwise `/tmp/fxi-{uid}.sock` if no home directory is available |
| Windows | `\\.\pipe\fxi-{USERNAME}`, otherwise `\\.\pipe\fxi` |

These are environment/home-resolution fallbacks, not a search through existing
socket files. Unix socket permissions are `0600`. There is no protocol-level
authentication; named-pipe naming alone is not an authentication guarantee.

Connections can carry multiple requests. Both platforms limit active connections
to 64 and use approximately 30-second I/O timeouts. An idle or incomplete frame
can cause the connection to close. These are transport timeouts, **not search
execution deadlines**; disconnecting does not provide query cancellation.

On Unix, requests on one connection can execute concurrently and replies may
arrive out of order. The default per-connection handler limit is 32, configurable
with `FXI_MAX_PIPELINED`; overload returns `Error`. Windows handles requests
sequentially on each connection. Do not depend on concurrent requests completing
in submission order, or pipeline dependent mutations and searches.

### Correlation, versions and failures

Any request may include a string `request_id`; a successfully decoded request's
response echoes it. Non-string IDs are ignored. Use distinct IDs for concurrent
requests. Without correlation support, keep only one request outstanding rather
than assuming FIFO on a concurrently executing server.

The current `protocol_version` is **2**. `Hello` reports the server's protocol and
package versions; the server does not reject a different client version itself.
Clients must compare versions. Optional fields and request variants have been
added without a version increment, so version 2 alone is not a capability list
for every older build. Older servers may ignore new option fields or reject a
new request variant. Pin compatible builds when relying on newer semantics.

Any decoded request can return `Error` instead of its usual response. Invalid
JSON, unknown variants, oversized frames and partial-frame failures close the
connection; an error frame may be sent first and may lack `request_id`. An
oversized response can likewise produce an encoding error or disconnect; do not
assume every failure produces a decodable error response. Reduce result/context
limits rather than retrying the same oversized query indefinitely.

## Root selection and search scope

`Search`, `ContentSearch`, `Reload` and `WatchStatus` accept an optional
`root_path`. Supply an absolute existing path:

- An indexed root selects that codebase.
- A file or subdirectory resolves its containing codebase. For **search**
  requests, results are also restricted to that file/subtree before limits are
  applied. Lifecycle and watch-status operations apply to the whole root.
- Omitting the field, or sending `null`, uses the sole loaded index. Zero loaded
  indexes or more than one loaded index produces an error. This does not select
  the sole index stored on disk if none is loaded.

Search can load an existing index on demand. It does not build a missing index.
When watching is enabled, loading a root starts its watcher/reconciliation.
Successful root-specific responses include `resolved_root`. All result paths are
relative to **that resolved root**, including searches scoped to a subdirectory.
A client can cache it for root identity, but must retain the original scoped path
if it wants subsequent queries to keep the same restriction.

## Query language

Both `Search.query` and `ContentSearch.pattern` use the **FXI query language**.
`ContentSearch.pattern` is not implicitly a raw regex. For example, use
`re:/TODO.*@\w+/` to request regex matching.

| Query text | Meaning |
|---|---|
| `foo bar` | Both case-insensitive substrings occur somewhere in the same file |
| `"foo bar"` | Exact, case-sensitive phrase on one line |
| `foo-bar` | One substring containing a literal hyphen |
| `(foo \| bar) -debug` | Boolean grouping and file-level exclusion |
| `re:/foo\/bar/` | Regex matching `foo/bar`; the slash delimiter is escaped |
| `near:foo,bar,5` | Terms fit in a shared window whose largest line difference is at most 5 |
| `ext:rs (foo \| bar)` | Global extension filter plus a grouped content expression |
| `file:main.rs` / `file:*.rs` | Case-insensitive exact basename / basename glob |
| `path:src/*.rs` | Root-relative path glob; `*` does not cross separators |
| `line:10-20 foo` | Return positive matching lines in the inclusive range |
| `size:>1000` / `size:<1000` | Strict byte-size comparisons |
| `mtime:2026-09-18` | UTC calendar day, ending before the next midnight |
| `^3:needle top:20` | Boost and explicit limit for ranked `Search` |

Malformed input returns an error. Source queries are limited to 64 KiB, 1,024
parsed terms and 32 nested groups; public AST execution has additional traversal
checks. Boosts must be finite and non-negative. Dates and numeric ranges are
validated. Filters cannot be negated, placed inside groups, or mixed into an
ungrouped OR. Duplicate fields are rejected, except compatible opposite numeric
bounds. Write `ext:rs (foo | bar)`, not `ext:rs foo | bar`.

Positive Boolean matches are deduplicated by line. NOT tests file-level truth
without inventing a positive source line. A filter-only or pure-negative result
can be represented as a file-level record with line number 1, empty content and
zero-length span; that is not a claim that source line 1 matched.

Candidates are verified against source content or validated source snapshots.
A stale index can still miss newly matching files until it is updated. Watched
roots can expose small updates before durable publication. Neither `Ping` nor
`WatchStatus.pending_changes == 0` is a filesystem freshness barrier. See
[Freshness](SEMANTICS.md#freshness) for the visibility/persistence contract.

## Requests and responses

Every message has a top-level `type`. All examples below show JSON payloads;
add the length prefix when transmitting them. Optional `request_id` is omitted
from some examples for readability. Integer fields must be non-negative and fit
their Rust protocol types; context and line numbers are `u32`.

### Hello and Ping

```json
{"type":"Hello","protocol_version":2,"request_id":"hello-1"}
```

```json
{"type":"Hello","protocol_version":2,"server_version":"0.1.0","request_id":"hello-1"}
```

```json
{"type":"Ping","request_id":"ping-1"}
```

```json
{"type":"Pong","request_id":"ping-1"}
```

`Pong` tests protocol responsiveness, not index freshness or persistence.

### Search: ranked results

```json
{"type":"Search","query":"fn main","root_path":"/work/project/src","limit":10,"request_id":"search-1"}
```

```json
{"type":"Search","matches":[{"path":"src/main.rs","line_number":10,"score":2.5}],"duration_ms":4.2,"cached":false,"resolved_root":"/work/project","request_id":"search-1"}
```

`query` and `limit` are required. Results contain a relative `path`, a 1-based
`line_number` and finite numeric `score`. Ranked mode can include filename
fallback matches; files-only/content/count modes do not. Ranking evaluates the
verified result set before applying its limit.

| Wire `limit` | Query limit | Effective ranked limit |
|---|---|---|
| `N > 0` | No `top:`, or `top:0` | `N` |
| `0` | No `top:`, or `top:0` | Unlimited by record count |
| `0` | `top:M`, `M > 0` | `M` |
| `N > 0` | `top:M`, `M > 0` | `min(N, M)` |

There is no hidden 100-result default on this wire operation. The 100 MiB frame
limit still applies. `sort:path` and `sort:recency` select alternate ranked-output
orders. `duration_ms` is server-side elapsed handling time, not client latency.
`cached` is currently always `false`: full query responses are not cached.
Source and index caches may still be used internally.

### ContentSearch: lines, files or counts

```json
{"type":"ContentSearch","pattern":"re:/TODO.*@\\w+/","root_path":"/work/project","limit":50,"options":{"context_before":1,"context_after":1,"case_insensitive":false,"files_only":false,"compact_files":false,"counts_only":false,"word_regexp":false},"request_id":"content-1"}
```

`pattern`, `limit` and `options` are required. Within `options`, the first three
fields below are required; the remaining booleans default to `false` when absent.

| Option | Meaning |
|---|---|
| `context_before`, `context_after` | Context lines per content match (`u32`) |
| `case_insensitive` | Ignore case for phrases/regexes; bare literals already ignore case |
| `files_only` | Return each matching file once; takes precedence over `counts_only` |
| `compact_files` | With `files_only`, return paths in `file_paths` instead of placeholder content records |
| `counts_only` | Return `[path, count]` pairs in `file_counts` |
| `word_regexp` | Apply Unicode regex word boundaries structurally, preserving leaf case behavior; boosts and `near:` are rejected in this mode |

Content output is path/line ordered; files and counts are path ordered.
`sort:` and `top:` do not control this operation: use its wire `limit`.

| Mode | Limit and payload |
|---|---|
| Full content | `limit` caps returned records globally; `0` means up to the 10,000,000-record cap. Payload uses `matches`. |
| Files only | `limit` caps files, with the same 10,000,000 cap. With `compact_files:true`, `matches` is empty and `file_paths` contains the paths; otherwise each file gets an empty-content placeholder in `matches`. |
| Counts only | `limit` caps the total counted matches across path-ordered files, **not per-file counts**; `0` is unlimited by count. `matches` is empty and `file_counts` contains pairs. This path does not apply the 10,000,000-record cap. |

All modes remain subject to the frame byte limit. Compact/count fields are
optional: clients should tolerate their absence from older servers. Empty
queries currently return empty `matches` without either optional field.

A full-content response has this shape:

```json
{"type":"ContentSearch","matches":[{"path":"src/main.rs","line_number":2,"line_content":"TODO fix","match_start":0,"match_end":4,"context_before":[[1,"fn main() {"]],"context_after":[[3,"}"]]}],"duration_ms":3.1,"files_with_matches":1,"resolved_root":"/work/project"}
```

`match_start` is inclusive and `match_end` exclusive, both **UTF-8 byte offsets**
within `line_content`. JavaScript/UTF-16 clients must convert them before slicing.
The current schema retains one positive span per matching line, not every
occurrence. Context arrays contain `[line_number, text]` pairs and can overlap
between records; presentation clients should merge overlapping context intervals.

`files_with_matches` counts all verified matching files **before** truncation in
full-content mode. In files/count modes it counts the files represented in the
returned payload. Do not infer the same completeness meaning across these modes.

### Status and WatchStatus

```json
{"type":"Status"}
```

The response has `type:"Status"` and these fields:

| Field | Meaning and limits |
|---|---|
| `uptime_secs` | Seconds since daemon construction |
| `indexes_loaded`, `loaded_roots` | Number and absolute paths of resident roots; root order is unspecified |
| `total_docs` | Live indexed documents across loaded readers (`u32`) |
| `queries_served` | Recorded successful nonempty-query searches; not every request or error |
| `protocol_version`, `server_version` | Wire protocol and package versions |
| `watch_enabled` | Whether this daemon was started with watching enabled |
| `watched_roots` | Roots currently reported as having running watcher handles |
| `cache_hit_rate` | Legacy query-response-cache metric; currently zero, not the source-cache hit rate |
| `memory_bytes` | Legacy heuristic: roughly stored document count × 100 bytes plus 1 MiB per root. **Not RSS, allocated bytes, or a memory-budget guarantee.** |

Use operating-system measurements for memory comparisons. `watch_enabled` does
not mean every stored index is loaded/watched, and a running watcher handle is
not an end-to-end freshness check.

```json
{"type":"WatchStatus","root_path":"/work/project"}
```

```json
{"type":"WatchStatus","watching":true,"pending_changes":2,"resolved_root":"/work/project"}
```

`WatchStatus` does not load an unloaded root. `pending_changes` is the count in
the daemon's accumulated pending batch, not every event in the OS, debouncer or
channel. Pending paths may already be searchable through a memory snapshot while
awaiting persistence; zero does not prove that the root is fully up to date.

### Reload and Remove

```json
{"type":"Reload","root_path":"/work/project"}
```

```json
{"type":"Reloaded","success":true,"message":"Reloaded current generation","resolved_root":"/work/project"}
```

`Reload` opens the current durable generation and replaces the resident reader,
or loads the root if needed. It does not scan/rebuild source files itself and is
not a “flush visible changes” request. Check both the response type and
`success`; reload failure can return `Reloaded` with `success:false`, while root
resolution can return `Error`. New searches use the replaced reader; already
running searches may retain their original immutable snapshot.

```json
{"type":"Remove","root_path":"/work/project"}
```

```json
{"type":"Reloaded","success":true,"message":"Removed index and unloaded daemon reader","resolved_root":"/work/project"}
```

`Remove.root_path` is required. Removal stops the root's watcher, unloads its
resident reader, discards its pending batch and removes its stored index under
the index writer lock. Source files are not deleted. The response deliberately
reuses `Reloaded`; there is no `Removed` variant. Failures return `Error`.
Already-running searches may still finish using a retained snapshot; a subsequent
new search cannot silently use an unloaded reader.

### Shutdown

```json
{"type":"Shutdown","request_id":"stop-1"}
```

```json
{"type":"ShuttingDown","request_id":"stop-1"}
```

Despite its name, `ShuttingDown` is sent **after the pending-update persistence
phase reports success**. The daemon stops and joins watcher producers, drains
final batches, reconciles watched roots and persists pending work. The update
loop retries pending work for up to approximately 20 seconds; the request waits
up to 25 seconds for its result. These are retry/wait bounds, not guaranteed
upper bounds on a filesystem operation already running.

If persistence fails or the wait expires, the response is `Error`, not a success
acknowledgment. The daemon is still shutting down; an error or lost connection
must not be treated as proof of durability. Resolve read/writer-lock problems
and restart with `--watch` or run an index update to reconcile. Successful
acknowledgment also does not mean socket/PID cleanup or process exit has already
completed. Stop editing during shutdown if a final on-disk snapshot matters;
writes made after watchers stop are outside that snapshot.

After shutdown begins, new search/reload/remove/watch/hello requests are rejected.
Ping, Status and repeated Shutdown requests are allowed while the endpoint is
still available. Do not use a shutdown request to wait for every pipelined search
on other connections to finish.

## Minimal sequential Python client (Unix)

This example discovers the actual endpoint, validates framing, correlates replies
and fails explicitly on protocol errors. Replace the example root with an
existing indexed root. It neither builds an index nor starts/stops the daemon.

```python
import json
import socket
import struct
import subprocess

MAX_FRAME = 100 * 1024 * 1024


def read_exact(stream, length):
    data = bytearray()
    while len(data) < length:
        chunk = stream.recv(length - len(data))
        if not chunk:
            raise ConnectionError("Daemon closed the connection")
        data.extend(chunk)
    return bytes(data)


def request(stream, payload, request_id):
    payload = dict(payload, request_id=request_id)
    encoded = json.dumps(payload).encode("utf-8")
    if len(encoded) > MAX_FRAME:
        raise ValueError("Request exceeds frame limit")
    stream.sendall(struct.pack("<I", len(encoded)) + encoded)
    length = struct.unpack("<I", read_exact(stream, 4))[0]
    if length > MAX_FRAME:
        raise ValueError("Response exceeds frame limit")
    response = json.loads(read_exact(stream, length))
    if response.get("request_id") != request_id:
        raise RuntimeError("Missing or unexpected request_id; close this connection")
    if response["type"] == "Error":
        raise RuntimeError(response["message"])
    return response


endpoint = subprocess.check_output(
    ["fxi", "daemon", "socket-path"], text=True
).strip()
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
    stream.settimeout(30)
    stream.connect(endpoint)
    hello = request(stream, {"type": "Hello", "protocol_version": 2}, "hello")
    if hello.get("type") != "Hello" or hello.get("protocol_version") != 2:
        raise RuntimeError("Unsupported daemon protocol")
    result = request(stream, {
        "type": "ContentSearch",
        "pattern": '"fn main"',
        "root_path": "/path/to/repository",
        "limit": 20,
        "options": {
            "context_before": 0,
            "context_after": 0,
            "case_insensitive": False,
            "files_only": True,
            "compact_files": True,
        },
    }, "search")
    if result["type"] != "ContentSearch":
        raise RuntimeError("Unexpected response type")
    paths = result.get("file_paths")
    if paths is None:  # Legacy noncompact reply, or an empty query.
        paths = [match["path"] for match in result["matches"]]
    for path in paths:
        print(path)
```

For a production client, also handle reconnection and cancellation in its UI,
reject pending requests on disconnect, and bound its own queued requests. The
server's per-connection concurrency setting is not a client-side memory bound.
