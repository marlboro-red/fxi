# FXI Architecture

This document describes the high-level architecture of FXI, a fast code search engine.

## Overview

FXI achieves 100-400x faster search performance than ripgrep through persistent indexing. The core insight is that source code changes infrequently relative to how often it's searched, making pre-computed indexes highly beneficial.

```
┌─────────────────────────────────────────────────────────────────┐
│                         User Interface                          │
├──────────────┬──────────────┬──────────────┬───────────────────┤
│ CLI (main.rs)│  TUI (tui/)  │Server(server)│ VS Code Extension │
└──────┬───────┴───────┬──────┴──────┬───────┴────────┬──────────┘
       │               │             │                │
       │               │             │    (IPC: Unix socket /
       │               │             │     Windows named pipe)
       └───────────────┼─────────────┘                │
                       │                              │
┌──────────────────────▼──────────────────────────────▼──────────┐
│                  Daemon (server/)                               │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌───────────────┐  │
│  │ Protocol │  │  Client  │  │ Watcher  │  │   Debouncer   │  │
│  └──────────┘  └──────────┘  └──────────┘  └───────────────┘  │
└──────────────────────┬────────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────────┐
│                      Query Processing                          │
│  ┌─────────────┐  ┌─────────────┐  ┌───────────────────────┐  │
│  │   Parser    │→ │   Planner   │→ │      Executor         │  │
│  │ (parser.rs) │  │(planner.rs) │  │   (executor.rs)       │  │
│  └─────────────┘  └─────────────┘  └───────────────────────┘  │
└──────────────────────┬────────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────────┐
│                      Index Layer                               │
│  ┌─────────────┐  ┌─────────────┐  ┌───────────────────────┐  │
│  │   Reader    │  │   Writer    │  │       Types           │  │
│  │ (reader.rs) │  │ (writer.rs) │  │    (types.rs)         │  │
│  └─────────────┘  └─────────────┘  └───────────────────────┘  │
└──────────────────────┬────────────────────────────────────────┘
                       │
┌──────────────────────▼────────────────────────────────────────┐
│                     Storage (Filesystem)                       │
│  ~/.local/share/fxi/indexes/{hash}/                            │
│  ├── meta.json, docs.bin, paths.bin                            │
│  └── segments/seg_NNNN/{grams.*, tokens.*, bloom.bin}          │
└───────────────────────────────────────────────────────────────┘
```

## Module Structure

### `src/index/` - Index Management

The index module handles building, reading, and maintaining the search index.

| File | Purpose |
|------|---------|
| `build.rs` | Parallel index construction from filesystem |
| `reader.rs` | Memory-mapped index reading |
| `writer.rs` | Streaming index writing |
| `types.rs` | Data structures (Document, Trigram, Language) |
| `compact.rs` | Segment merging and compaction |
| `stats.rs` | Index statistics and diagnostics |

### `src/query/` - Query Processing

The query module implements the search pipeline: parse → plan → execute.

| File | Purpose |
|------|---------|
| `parser.rs` | Query tokenization and AST construction |
| `planner.rs` | Query optimization and execution planning |
| `executor.rs` | Parallel query execution with early termination |
| `scorer.rs` | Relevance scoring and ranking |

### `src/server/` - Persistent Daemon

The server module provides a daemon for warm searches.

| File | Purpose |
|------|---------|
| `daemon_unix.rs` | Unix socket server |
| `daemon_windows.rs` | Windows named pipe server |
| `client_unix.rs` | Unix client |
| `client_windows.rs` | Windows client |
| `protocol.rs` | JSON-based request/response protocol |
| `watcher.rs` | File system watching for live index updates |
| `debouncer.rs` | Event debouncing to batch rapid file changes |

### `src/tui/` - Terminal Interface

| File | Purpose |
|------|---------|
| `app.rs` | Application state and search logic |
| `ui.rs` | Ratatui-based rendering |

### `src/utils/` - Utilities

| File | Purpose |
|------|---------|
| `trigram.rs` | 3-byte sequence extraction |
| `tokenizer.rs` | Identifier extraction (camelCase, snake_case) |
| `bloom.rs` | Bloom filter for fast negative lookups |
| `encoding.rs` | Variable-length integer encoding |
| `app_data.rs` | XDG-compliant data directory management |

### `vscode-extension/` - VS Code Integration

A TypeScript extension that connects to the fxi daemon for IDE-integrated search.

| Directory/File | Purpose |
|----------------|---------|
| `src/extension.ts` | Extension entry point and lifecycle |
| `src/commands/` | Command handlers (search, daemon, index, reload) |
| `src/daemon/` | Daemon communication (client, protocol, socket) |
| `src/ui/` | Status bar and workspace utilities |
| `src/webview/` | Search panel webview (HTML generation, message protocol) |

The extension communicates with the daemon over the same Unix socket / Windows named pipe protocol used by the CLI.

## Key Design Decisions

### 1. Hybrid Two-Tier Indexing

FXI uses two complementary index structures:

**Trigram Index** (3-byte substrings):
- Enables substring matching (`"println"` matches `"eprintln"`)
- Fast candidate narrowing via posting list intersection
- Compact representation using varint encoding

**Token Index** (extracted identifiers):
- Exact word matching for known identifiers
- Handles camelCase/snake_case decomposition
- Faster than trigram for whole-word queries

Query planner automatically chooses the optimal strategy:
- Single words → Union of token lookup + trigram search
- Phrases → Trigram intersection + verification
- Filters → Direct metadata lookup

### 2. Memory-Mapped I/O

The index reader uses `mmap` for:
- Zero-copy reads from disk
- OS-managed caching (no manual cache invalidation)
- Instant "opening" - actual reads happen on demand
- Shared memory between daemon and clients

### 3. Parallel Processing

FXI leverages Rayon for data parallelism:
- **Index building**: Files processed in parallel
- **Query execution**: Posting lists intersected in parallel
- **Result verification**: Content verification parallelized

### 4. Segment-Based Architecture

Indexes are built in segments that can be:
- Written incrementally without rewriting the entire index
- Compacted in the background
- Memory-mapped individually for efficient memory use

### 5. Cross-Platform Daemon

The daemon uses platform-specific IPC:
- **Unix/macOS**: Unix domain sockets (`$XDG_RUNTIME_DIR/fxi.sock` or `~/.local/run/fxi.sock`)
- **Windows**: Named pipes (`\\.\pipe\fxi-{username}`)

Both share the same JSON-based length-prefixed protocol (`protocol.rs`), with platform-specific transport in `daemon_unix.rs`/`client_unix.rs` and `daemon_windows.rs`/`client_windows.rs`.

### 6. File Watching and Incremental Updates

The daemon supports live index updates via file system watching:
- **Watcher** (`watcher.rs`): Monitors directories using the `notify` crate, respects `.gitignore` rules and skips non-source directories
- **Debouncer** (`debouncer.rs`): Batches rapid file changes (IDE auto-save, git operations) into single update operations
- **Delta segments**: Changes are written as new segments rather than rebuilding the full index, with periodic compaction available via `fxi compact`

### 7. Bloom Filter Pre-filtering

Each segment includes a Bloom filter for:
- Fast rejection of queries that have no matches
- Avoids expensive posting list reads
- ~1% false positive rate with 10 bits/element

## Data Flow

### Index Building

```
Filesystem                 Index Writer
    │                          │
    ├─ Walk directory ─────────┤
    │                          │
    ├─ Filter .gitignore ──────┤
    │                          │
    ├─ Read file content ──────┤
    │                          │
    │                    ┌─────▼─────┐
    │                    │ Extract   │
    │                    │ trigrams  │
    │                    │ & tokens  │
    │                    └─────┬─────┘
    │                          │
    │                    ┌─────▼─────┐
    │                    │ Write     │
    │                    │ postings  │
    │                    │ & bloom   │
    │                    └───────────┘
```

### Query Execution

```
Query String        Parser            Planner           Executor
     │                │                  │                  │
     └───────────────►│                  │                  │
                      │ AST              │                  │
                      └─────────────────►│                  │
                                         │ Plan             │
                                         └─────────────────►│
                                                            │
                                               ┌────────────▼────────────┐
                                               │ 1. Bloom filter check   │
                                               │ 2. Posting list lookup  │
                                               │ 3. List intersection    │
                                               │ 4. Content verification │
                                               │ 5. Score & rank         │
                                               └────────────┬────────────┘
                                                            │
                                                       Results
```

## On-Disk Format

### Index Directory Structure

```
~/.local/share/fxi/indexes/{path_hash}/
├── meta.json           # Index metadata (version, doc_count, etc.)
├── docs.bin            # Document table (fixed-size records)
├── paths.bin           # Path string store (length-prefixed)
└── segments/
    └── seg_0001/
        ├── grams.dict      # Trigram → offset mapping
        ├── grams.postings  # Varint-encoded doc ID lists
        ├── tokens.dict     # Token → offset mapping
        ├── tokens.postings # Token posting lists
        └── bloom.bin       # Bloom filter bitmap
```

### Document Record (30 bytes)

```
┌──────────┬──────────┬──────────┬──────────┬──────────┬───────┬────────────┐
│ doc_id   │ path_id  │ size     │ mtime    │ language │ flags │ segment_id │
│ (4 bytes)│ (4 bytes)│ (8 bytes)│ (8 bytes)│ (2 bytes)│(2 b)  │ (2 bytes)  │
└──────────┴──────────┴──────────┴──────────┴──────────┴───────┴────────────┘
```

## Performance Characteristics

| Operation | Complexity | Notes |
|-----------|------------|-------|
| Index open | O(1) | Just mmap, no actual I/O |
| Trigram lookup | O(k) | k = posting list length |
| Intersection | O(m+n) | Merge-style intersection |
| Content verification | O(n) | n = candidate count |

Typical search latency: 5-50ms on million-file codebases.

## Security Considerations

1. **Path Traversal Protection**: `get_full_path()` canonicalizes paths and validates they don't escape the index root
2. **Input Validation**: Query parser rejects malformed input
3. **Bounded Memory**: Result limits prevent OOM on pathological queries
4. **Lock Poisoning**: Poisoned mutexes are recovered gracefully

## Testing Strategy

- **Unit tests**: Core algorithms (trigram extraction, varint encoding)
- **Integration tests**: End-to-end query scenarios
- **Fuzzing**: Query parser and trigram extraction (cargo-fuzz)
- **Benchmarks**: Performance regression detection (criterion)

## Future Directions

- Distributed indexing for very large codebases
- Language-aware symbol extraction
- Integration with LSP servers
