# FXI: Index Search Optimization Techniques

> A comprehensive technical deep-dive into the optimization strategies powering fxi's 100-400x faster code search

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [System Architecture](#2-system-architecture)
3. [Daemon Server Architecture](#3-daemon-server-architecture)
4. [Index Structure](#4-index-structure)
5. [Query Processing Pipeline](#5-query-processing-pipeline)
6. [Trigram Optimization](#6-trigram-optimization)
7. [Memory Optimization](#7-memory-optimization)
8. [Parallelization Strategy](#8-parallelization-strategy)
9. [Caching Mechanisms](#9-caching-mechanisms)
10. [Scoring & Ranking](#10-scoring--ranking)
11. [Performance Characteristics](#11-performance-characteristics)

---

## 1. Executive Summary

**fxi** achieves dramatic search performance improvements through a multi-layered optimization strategy centered around a **persistent daemon architecture**:

| Technique | Impact | Description |
|-----------|--------|-------------|
| **Daemon Server** | **Sub-millisecond queries** | Persistent process keeps index warm in memory |
| Query Result Cache | 10-100x faster repeated queries | LRU cache (128 entries) per index |
| Hybrid Indexing | 100-400x faster | Trigram + Token indices for different query patterns |
| Tiered Data Structures | 30-40% faster indexing | File-size adaptive trigram extraction |
| Bloom Pre-filtering | Skip 60-80% of segments | Probabilistic early rejection |
| Stop-gram Filtering | 10-50x smaller candidate sets | Eliminate common trigrams from queries |
| Delta Encoding | 60-70% smaller indices | Variable-length integer compression |
| Memory Mapping | Zero-copy file access | OS-managed paging for large files |
| Adaptive Parallelism | Linear scaling | Rayon with threshold-based decisions |
| Early Termination | 2-10x faster for limited queries | Stop as soon as limit reached |

---

## 2. System Architecture

### 2.1 High-Level Architecture (Daemon Mode)

fxi is designed to run as a **persistent daemon** (`fxid`) that keeps indexes warm in memory, enabling sub-millisecond query response times.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         FXI DAEMON ARCHITECTURE                              │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  CLIENT PROCESSES                           DAEMON PROCESS (fxid)           │
│  ─────────────────                          ─────────────────────           │
│                                                                              │
│  ┌─────────────┐                           ┌─────────────────────────────┐  │
│  │  Terminal 1 │──┐                        │      INDEX SERVER           │  │
│  │  fxi "query"│  │                        │                             │  │
│  └─────────────┘  │    Unix Socket         │  ┌─────────────────────┐    │  │
│                   │    (or Named Pipe)     │  │   Warm Index Cache  │    │  │
│  ┌─────────────┐  │   ┌──────────────┐     │  │                     │    │  │
│  │  Terminal 2 │──┼──▶│ fxi.sock     │────▶│  │  ┌─────────────┐    │    │  │
│  │  fxi "foo"  │  │   └──────────────┘     │  │  │ Index A     │    │    │  │
│  └─────────────┘  │                        │  │  │ (150K docs) │    │    │  │
│                   │                        │  │  └─────────────┘    │    │  │
│  ┌─────────────┐  │                        │  │  ┌─────────────┐    │    │  │
│  │  Terminal 3 │──┘                        │  │  │ Index B     │    │    │  │
│  │  fxi -l     │                           │  │  │ (80K docs)  │    │    │  │
│  └─────────────┘                           │  │  └─────────────┘    │    │  │
│                                            │  └─────────────────────┘    │  │
│  ┌─────────────┐                           │                             │  │
│  │    TUI      │◀─────────────────────────▶│  ┌─────────────────────┐    │  │
│  │  (ratatui)  │   Persistent Connection   │  │   Query Cache (LRU) │    │  │
│  └─────────────┘                           │  │   128 entries/index │    │  │
│                                            │  └─────────────────────┘    │  │
│                                            │                             │  │
│                                            │  ┌─────────────────────┐    │  │
│                                            │  │      Statistics     │    │  │
│                                            │  │  • queries served   │    │  │
│                                            │  │  • cache hit rate   │    │  │
│                                            │  │  • uptime           │    │  │
│                                            │  └─────────────────────┘    │  │
│                                            └─────────────────────────────┘  │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 Component Interaction Flow

```
                    USER QUERY
                        │
                        ▼
┌───────────────────────────────────────────────────────────────┐
│                      QUERY PARSER                             │
│  • Tokenize query string                                      │
│  • Build AST (QueryNode tree)                                 │
│  • Extract filters (ext:, lang:, path:, etc.)                 │
└───────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────────────────────┐
│                     QUERY PLANNER                             │
│  • Extract trigrams from literals                             │
│  • Build execution plan (narrowing + verification steps)      │
│  • Order by selectivity (rarest trigrams first)               │
│  • Identify stop-grams to skip                                │
└───────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌───────────────────────────────────────────────────────────────┐
│                    QUERY EXECUTOR                             │
│                                                               │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │              PHASE 1: NARROWING                         │  │
│  │                                                         │  │
│  │   For each segment (parallel):                          │  │
│  │     1. Check Bloom filter → skip if no match possible   │  │
│  │     2. Load trigram postings (delta-decode)             │  │
│  │     3. Intersect posting lists (RoaringBitmap)          │  │
│  │     4. Apply token lookups if applicable                │  │
│  │     5. Apply field filters (ext, lang, size, mtime)     │  │
│  │     6. Produce candidate document set                   │  │
│  └─────────────────────────────────────────────────────────┘  │
│                         │                                     │
│                         ▼                                     │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │             PHASE 2: VERIFICATION                       │  │
│  │                                                         │  │
│  │   For each candidate (parallel if large):               │  │
│  │     1. Load file content (mmap or read)                 │  │
│  │     2. Search for actual pattern (literal/regex)        │  │
│  │     3. Extract match locations & context                │  │
│  │     4. Early terminate if limit reached                 │  │
│  └─────────────────────────────────────────────────────────┘  │
│                         │                                     │
│                         ▼                                     │
│  ┌─────────────────────────────────────────────────────────┐  │
│  │                    SCORING                              │  │
│  │                                                         │  │
│  │   For each match:                                       │  │
│  │     • Match count (logarithmic weighting)               │  │
│  │     • Filename match bonus (2x)                         │  │
│  │     • Path depth penalty (-0.05 per level)              │  │
│  │     • Recency bonus (7-day half-life)                   │  │
│  │     • User boost factor (^N syntax)                     │  │
│  └─────────────────────────────────────────────────────────┘  │
└───────────────────────────────────────────────────────────────┘
                        │
                        ▼
                    RESULTS
```

### 2.3 On-Disk Index Layout

```
~/.local/share/fxi/indexes/{codebase_hash}/
│
├── meta.json                 # Index metadata
│   {
│     "doc_count": 150000,
│     "segment_count": 12,
│     "stop_grams": [0x202020, 0x746865, ...],  // Top 512 common trigrams
│     "created_at": "2024-01-15T10:30:00Z",
│     "root_path": "/path/to/codebase"
│   }
│
├── docs.bin                  # Fixed-size document table (30 bytes each)
│   ┌────────────────────────────────────────────────────────────────┐
│   │ doc_id │ path_id │ size   │ mtime  │ lang │ flags │ segment_id │
│   │ u32    │ u32     │ u64    │ u64    │ u16  │ u16   │ u16        │
│   │ 4B     │ 4B      │ 8B     │ 8B     │ 2B   │ 2B    │ 2B    = 30B│
│   └────────────────────────────────────────────────────────────────┘
│
├── paths.bin                 # Variable-length path strings
│   ┌───────────────────────────────────────────────────┐
│   │ len(2B) │ path_bytes... │ len(2B) │ path_bytes... │
│   └───────────────────────────────────────────────────┘
│
└── segments/
    └── seg_0001/
        │
        ├── grams.dict        # Trigram dictionary (sorted)
        │   ┌───────────────────────────────────────────────┐
        │   │ trigram │ offset │ length │ doc_freq │        │
        │   │ u32     │ u64    │ u32    │ u32      │ = 20B  │
        │   └───────────────────────────────────────────────┘
        │
        ├── grams.postings    # Delta-encoded doc IDs
        │   ┌───────────────────────────────────────────────┐
        │   │ varint(delta1) │ varint(delta2) │ ...         │
        │   └───────────────────────────────────────────────┘
        │
        ├── tokens.dict       # Token dictionary (sorted)
        │   ┌───────────────────────────────────────────────┐
        │   │ len(2B) │ token │ offset │ length │ doc_freq  │
        │   └───────────────────────────────────────────────┘
        │
        ├── tokens.postings   # Delta-encoded doc IDs
        │
        ├── linemap.bin       # Line offset positions
        │   ┌───────────────────────────────────────────────┐
        │   │ doc_id │ line_count │ offset1 │ offset2 │ ... │
        │   └───────────────────────────────────────────────┘
        │
        └── bloom.bin         # Bloom filter for trigrams
            ┌───────────────────────────────────────────────┐
            │ bit_array[m bits] where m = -n*ln(p)/ln(2)²   │
            └───────────────────────────────────────────────┘
```

---

## 3. Daemon Server Architecture

The daemon (`fxid`) is the **primary mode of operation** for fxi. It eliminates cold-start latency by keeping indexes permanently loaded in memory.

### 3.1 Why a Daemon?

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    COLD START vs WARM DAEMON                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   WITHOUT DAEMON (cold start every query):                                  │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  User runs: fxi "pattern"                                          │    │
│   │                                                                     │    │
│   │  Timeline:                                                          │    │
│   │  0ms ─────── 150ms ─────── 300ms ─────── 350ms ─────── 400ms       │    │
│   │  │           │             │             │             │            │    │
│   │  │ Load      │ Load        │ Parse       │ Execute     │ Done       │    │
│   │  │ docs.bin  │ segments    │ query       │ search      │            │    │
│   │  │           │             │             │             │            │    │
│   │  └───────────┴─────────────┴─────────────┴─────────────┘            │    │
│   │     INDEX LOADING: 300ms        ACTUAL SEARCH: 50ms                 │    │
│   │                                                                     │    │
│   │  Total: ~350ms (85% spent loading!)                                │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   WITH DAEMON (index always warm):                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  User runs: fxi "pattern"                                          │    │
│   │                                                                     │    │
│   │  Timeline:                                                          │    │
│   │  0ms ── 2ms ── 5ms ── 15ms ── 20ms                                 │    │
│   │  │      │      │       │       │                                    │    │
│   │  │ IPC  │ Parse│ Exec  │ IPC   │ Done                               │    │
│   │  │ send │ query│ search│ recv  │                                    │    │
│   │  │      │      │       │       │                                    │    │
│   │  └──────┴──────┴───────┴───────┘                                    │    │
│   │                                                                     │    │
│   │  Total: ~20ms (17x faster!)                                        │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   CACHE HIT (repeated query):                                               │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  0ms ── 1ms ── 2ms                                                 │    │
│   │  │      │      │                                                    │    │
│   │  │ IPC  │ Cache│ Done                                               │    │
│   │  │      │ hit! │                                                    │    │
│   │                                                                     │    │
│   │  Total: ~2ms (sub-millisecond search!)                             │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.2 Server Components

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         INDEX SERVER INTERNALS                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   struct IndexServer {                                                       │
│       indexes: RwLock<HashMap<PathBuf, CachedIndex>>,  // Multi-codebase    │
│       stats: ServerStats,                               // Metrics           │
│       shutdown: AtomicBool,                             // Graceful stop     │
│   }                                                                          │
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │                        CachedIndex                                   │   │
│   ├─────────────────────────────────────────────────────────────────────┤   │
│   │                                                                      │   │
│   │   reader: Arc<IndexReader>          ← Shared across threads          │   │
│   │                                                                      │   │
│   │   query_cache: Mutex<LruCache<      ← Per-index result cache         │   │
│   │       String,                          Query string                  │   │
│   │       Vec<SearchMatchData>             Cached results                │   │
│   │   >>                                                                 │   │
│   │   Capacity: 128 entries                                              │   │
│   │                                                                      │   │
│   │   last_used: Mutex<Instant>         ← For future LRU eviction        │   │
│   │                                                                      │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │                        ServerStats                                   │   │
│   ├─────────────────────────────────────────────────────────────────────┤   │
│   │                                                                      │   │
│   │   start_time: Instant               ← Uptime tracking                │   │
│   │   queries_served: AtomicU64         ← Total query count              │   │
│   │   cache_hits: AtomicU64             ← For hit rate calculation       │   │
│   │   cache_misses: AtomicU64                                            │   │
│   │                                                                      │   │
│   │   cache_hit_rate() = hits / (hits + misses)                          │   │
│   │                                                                      │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.3 IPC Protocol

Communication uses a **length-prefixed JSON protocol** over Unix sockets (or named pipes on Windows):

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          MESSAGE FORMAT                                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   ┌──────────────┬──────────────────────────────────────────────────────┐   │
│   │  Length (4B) │  JSON Payload (N bytes)                              │   │
│   │  Little-end  │                                                      │   │
│   └──────────────┴──────────────────────────────────────────────────────┘   │
│                                                                              │
│   REQUEST TYPES:                                                            │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Search        { query, root_path, limit }                         │    │
│   │  ContentSearch { pattern, root_path, limit, options }              │    │
│   │  Status        (no params)                                         │    │
│   │  Reload        { root_path }                                       │    │
│   │  Shutdown      (no params)                                         │    │
│   │  Ping          (no params)                                         │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   RESPONSE TYPES:                                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Search        { matches, duration_ms, cached }                    │    │
│   │  ContentSearch { matches, duration_ms, files_with_matches }        │    │
│   │  Status        { uptime, indexes_loaded, queries_served, ... }     │    │
│   │  Reloaded      { success, message }                                │    │
│   │  ShuttingDown                                                      │    │
│   │  Pong                                                              │    │
│   │  Error         { message }                                         │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   EXAMPLE EXCHANGE:                                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Client → Server:                                                  │    │
│   │  [45 00 00 00] {"type":"Search","query":"fn main","root_path":...} │    │
│   │   └─ 69 bytes                                                      │    │
│   │                                                                     │    │
│   │  Server → Client:                                                  │    │
│   │  [A3 01 00 00] {"type":"Search","matches":[...],"duration_ms":8.5} │    │
│   │   └─ 419 bytes                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.4 Connection Handling

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    THREAD-PER-CONNECTION MODEL                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   Main Thread                          Worker Threads                       │
│   ───────────                          ──────────────                       │
│                                                                              │
│   ┌─────────────┐                                                           │
│   │   accept()  │ ←── UnixListener::incoming()                              │
│   └──────┬──────┘                                                           │
│          │                                                                   │
│          │ New connection                                                   │
│          ▼                                                                   │
│   ┌─────────────┐        ┌─────────────────────────────────────┐            │
│   │   spawn()   │───────▶│  Worker Thread                      │            │
│   └──────┬──────┘        │                                     │            │
│          │               │  loop {                             │            │
│          │               │      request = read_message()       │            │
│          │               │      response = handle_request()    │            │
│          │               │      write_message(response)        │            │
│          │               │      if shutdown { break }          │            │
│          │               │  }                                  │            │
│          │               └─────────────────────────────────────┘            │
│          │                                                                   │
│          │ Another connection                                               │
│          ▼                                                                   │
│   ┌─────────────┐        ┌─────────────────────────────────────┐            │
│   │   spawn()   │───────▶│  Worker Thread                      │            │
│   └─────────────┘        │  (handles concurrent client)        │            │
│                          └─────────────────────────────────────┘            │
│                                                                              │
│   CONCURRENCY:                                                              │
│   • Multiple clients can query simultaneously                               │
│   • IndexReader is Arc<> shared across threads                              │
│   • Query cache uses Mutex (brief lock)                                     │
│   • Read-heavy workload: RwLock on indexes HashMap                          │
│                                                                              │
│   TIMEOUTS:                                                                 │
│   • Connection timeout: 30 seconds                                          │
│   • Prevents hung clients from holding resources                            │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.5 Daemon Lifecycle

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        DAEMON LIFECYCLE                                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   STARTUP (fxi daemon start):                                                   │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │   1. Double-fork (Unix daemonization)                              │    │
│   │      ┌────────┐     ┌────────┐     ┌────────┐                      │    │
│   │      │ Parent │────▶│ Child  │────▶│Grandchild                     │    │
│   │      │ exits  │     │ exits  │     │ = daemon │                     │    │
│   │      └────────┘     └────────┘     └────────┘                      │    │
│   │                                                                     │    │
│   │   2. Create new session (setsid)                                   │    │
│   │   3. Close stdin/stdout/stderr → /dev/null                         │    │
│   │   4. Write PID to ~/.local/run/fxi.pid                             │    │
│   │   5. Create Unix socket at ~/.local/run/fxi.sock                   │    │
│   │   6. Set socket permissions to 0600 (user only)                    │    │
│   │   7. Enter accept() loop                                           │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   LAZY INDEX LOADING:                                                       │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │   First query for /path/to/codebase:                               │    │
│   │   1. Check indexes HashMap (read lock) → not found                 │    │
│   │   2. Acquire write lock                                            │    │
│   │   3. Double-check (another thread may have loaded)                 │    │
│   │   4. IndexReader::open(/path/to/codebase)                          │    │
│   │   5. Insert into HashMap                                           │    │
│   │   6. Release write lock                                            │    │
│   │                                                                     │    │
│   │   Subsequent queries: read lock only (fast path)                   │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   SHUTDOWN (fxi daemon stop or SIGTERM):                                         │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │   1. Set shutdown flag (AtomicBool)                                │    │
│   │   2. Accept loop breaks                                            │    │
│   │   3. Existing connections finish current request                   │    │
│   │   4. Remove socket file                                            │    │
│   │   5. Remove PID file                                               │    │
│   │   6. Exit cleanly                                                  │    │
│   │                                                                     │    │
│   │   If graceful shutdown times out (1.5s):                           │    │
│   │   → SIGKILL sent to force termination                              │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   SOCKET LOCATIONS:                                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Platform   │ Primary Location                                     │    │
│   │  ─────────  │ ────────────────                                     │    │
│   │  Linux      │ $XDG_RUNTIME_DIR/fxi.sock (tmpfs, secure)           │    │
│   │  macOS      │ ~/.local/run/fxi.sock                               │    │
│   │  Windows    │ \\.\pipe\fxi-{username} (named pipe)                │    │
│   │  Fallback   │ /tmp/fxi-{uid}.sock                                 │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.6 Query Result Caching

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      QUERY RESULT CACHE                                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   Each loaded index has its own LRU cache:                                  │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Cache Key (String)          │  Cache Value (Vec<SearchMatchData>) │    │
│   │  ────────────────────        │  ──────────────────────────────────  │    │
│   │  "fn main"                   │  [match1, match2, match3, ...]      │    │
│   │  "ext:rs error"              │  [match1, match2, ...]              │    │
│   │  "TODO"                      │  [match1, match2, match3, ...]      │    │
│   │  ...                         │  ...                                 │    │
│   │                              │                                      │    │
│   │  Capacity: 128 entries       │                                      │    │
│   │  Eviction: LRU               │                                      │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   CACHE FLOW:                                                               │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │   Query arrives: "fn main"                                         │    │
│   │         │                                                          │    │
│   │         ▼                                                          │    │
│   │   ┌─────────────┐                                                  │    │
│   │   │ Cache check │                                                  │    │
│   │   └──────┬──────┘                                                  │    │
│   │          │                                                          │    │
│   │    ┌─────┴─────┐                                                   │    │
│   │    │           │                                                   │    │
│   │   HIT         MISS                                                 │    │
│   │    │           │                                                   │    │
│   │    ▼           ▼                                                   │    │
│   │  Return     Execute query                                          │    │
│   │  cached     │                                                      │    │
│   │  results    ▼                                                      │    │
│   │  (< 1ms)  Store in cache                                           │    │
│   │           │                                                         │    │
│   │           ▼                                                         │    │
│   │         Return results                                             │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   CACHE INVALIDATION:                                                       │
│   • Manual: `fxi daemon reload` clears cache for a codebase                     │
│   • No automatic invalidation (user must reload after file changes)        │
│   • Future: File watcher for automatic invalidation                        │
│                                                                              │
│   STATISTICS:                                                               │
│   • cache_hit_rate tracked per server                                      │
│   • Exposed via `fxi daemon status` command                                     │
│   • Typical hit rates: 40-60% for interactive use                          │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 3.7 Multi-Codebase Support

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    MULTIPLE CODEBASE SUPPORT                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   Single daemon serves multiple codebases simultaneously:                   │
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │                         IndexServer                                  │   │
│   │                                                                      │   │
│   │   indexes: HashMap<PathBuf, CachedIndex>                            │   │
│   │   ┌──────────────────────────────────────────────────────────────┐  │   │
│   │   │                                                               │  │   │
│   │   │  "/home/user/project-a"  →  CachedIndex { 150K docs }        │  │   │
│   │   │  "/home/user/project-b"  →  CachedIndex { 80K docs }         │  │   │
│   │   │  "/home/user/monorepo"   →  CachedIndex { 500K docs }        │  │   │
│   │   │                                                               │  │   │
│   │   └──────────────────────────────────────────────────────────────┘  │   │
│   │                                                                      │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│   USAGE:                                                                     │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  # From different directories, same daemon serves both:            │    │
│   │                                                                     │    │
│   │  cd ~/project-a && fxi "pattern"   # Uses project-a index          │    │
│   │  cd ~/project-b && fxi "pattern"   # Uses project-b index          │    │
│   │                                                                     │    │
│   │  # First query for each codebase loads its index (lazy loading)    │    │
│   │  # Subsequent queries are instant                                  │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   MEMORY MANAGEMENT:                                                        │
│   • Each index stays loaded until daemon restart                           │
│   • Future: LRU eviction for indexes not used in X hours                   │
│   • Status command shows all loaded indexes and memory usage               │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 4. Index Structure

### 4.1 Dual-Index Strategy

fxi maintains **two complementary indices** that work together:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           HYBRID INDEX STRATEGY                             │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌─────────────────────────────┐    ┌─────────────────────────────┐         │
│  │       TRIGRAM INDEX         │    │        TOKEN INDEX          │         │
│  ├─────────────────────────────┤    ├─────────────────────────────┤         │
│  │                             │    │                             │         │
│  │  "assert" → trigrams:       │    │  "assert" → exact token     │         │
│  │    "ass", "sse", "ser",     │    │                             │         │
│  │    "ert"                    │    │  Matches: "assert"          │         │
│  │                             │    │                             │         │
│  │  Matches:                   │    │  Does NOT match:            │         │
│  │    • "assert"               │    │    • "assertion"            │         │
│  │    • "assertion"            │    │    • "assert_eq"            │         │
│  │    • "assert_eq"            │    │                             │         │
│  │    • "reassert"             │    │                             │         │
│  │                             │    │                             │         │
│  │  USE CASE:                  │    │  USE CASE:                  │         │
│  │  Substring/partial matches  │    │  Exact word matches         │         │
│  │                             │    │                             │         │
│  └─────────────────────────────┘    └─────────────────────────────┘         │
│                                                                             │
│  QUERY STRATEGY:                                                            │
│  ┌────────────────────────────────────────────────────────────────────────┐ │
│  │  1. Single word (≥2 chars): UNION of token lookup AND trigram search   │ │
│  │  2. Multi-word query: Trigram intersection only                        │ │
│  │  3. Short word (<2 chars): Token index only (no useful trigrams)       │ │
│  │  4. Regex with prefix: Extract literal prefix, use trigrams            │ │
│  └────────────────────────────────────────────────────────────────────────┘ │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 4.2 Trigram Index Structure

A **trigram** is any 3-byte sequence from file content:

```
File content: "fn main() {"
             │││││││││││
Trigrams:    "fn " → 0x666E20
              "n m" → 0x6E206D
               " ma" → 0x206D61
                "mai" → 0x6D6169
                 "ain" → 0x61696E
                  "in(" → 0x696E28
                   "n()" → 0x6E2829
                    "() " → 0x282920
                     ") {" → 0x29207B
```

**Dictionary Format (Binary Search Lookup)**:

```
┌────────────────────────────────────────────────────────────────┐
│              TRIGRAM DICTIONARY (sorted by trigram)            │
├────────────────────────────────────────────────────────────────┤
│                                                                │
│  Index    Trigram    Posting Offset    Length    Doc Frequency │
│  ─────    ───────    ──────────────    ──────    ───────────── │
│  0        0x202020   0                 1,234     45,000        │
│  1        0x202061   1,234             567       12,000        │
│  2        0x202062   1,801             234       8,000         │
│  ...      ...        ...               ...       ...           │
│  N        0xFFFFFF   ...               ...       ...           │
│                                                                │
│  Lookup: Binary search O(log N) where N ≈ 16 million possible  │
│          (but typically only 500K-2M trigrams actually occur)  │
│                                                                │
└────────────────────────────────────────────────────────────────┘
```

**Posting List Format (Delta-Encoded)**:

```
Original doc IDs:    [1, 5, 10, 15, 100, 105, 200]
                      │  │   │   │   │    │    │
Deltas:              [1, 4,  5,  5, 85,   5,  95]
                      │  │   │   │   │    │    │
Varint encoded:      [01][04][05][05][55 01][05][5F 01]
                     1B  1B  1B  1B   2B    1B   2B   = 10 bytes

vs. raw u32:         28 bytes (7 × 4B)

Compression ratio:   ~65% smaller
```

### 4.3 Token Index Structure

Tokens are code-aware extractions:

```
Source code:           Token extraction:
─────────────          ─────────────────
getUserById     →      ["get", "user", "by", "id"]     (camelCase split)
get_user_by_id  →      ["get", "user", "by", "id"]     (snake_case split)
HTTP_STATUS     →      ["http", "status"]              (SCREAMING_CASE)
XMLParser       →      ["xml", "parser"]               (acronym handling)
```

**Tokenization Algorithm**:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        TOKEN EXTRACTION FLOW                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│   Input: "getUserById_v2"                                                   │
│                                                                             │
│   Step 1: Identify boundaries                                               │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  g e t U s e r B y I d _ v 2                                       │    │
│   │  ─ ─ ─ ↑ ─ ─ ─ ↑ ─ ↑ ─ ↑ ─ ↑                                       │    │
│   │        │       │   │   │   │                                       │    │
│   │        └─ upper└───┴───┴───┴─ boundaries                           │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                             │
│   Step 2: Split at boundaries                                               │
│   ["get", "User", "By", "Id", "v", "2"]                                     │
│                                                                             │
│   Step 3: Lowercase and filter                                              │
│   ["get", "user", "by", "id", "v", "2"]                                     │
│         │                     │   │                                         │
│         │                     │   └─ filtered (len < 2)                     │
│         │                     └─ filtered (len < 2)                         │
│         ▼                                                                   │
│   Final: ["get", "user", "by", "id"]                                        │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 4.4 Bloom Filter Pre-filtering

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        BLOOM FILTER MECHANISM                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│   Purpose: Fast rejection of segments that CANNOT contain query trigrams    │
│                                                                             │
│   ┌──────────────────────────────────────────────────────────────────────┐  │
│   │                    BLOOM FILTER BIT ARRAY                            │  │
│   │                                                                      │  │
│   │   Position:  0  1  2  3  4  5  6  7  8  9  10 11 12 13 14 15 ...     │  │
│   │              │  │  │  │  │  │  │  │  │  │  │  │  │  │  │  │          │  │
│   │   Bits:      0  1  0  0  1  1  0  1  0  0  1  0  1  0  0  1  ...     │  │
│   │                 ↑        ↑  ↑     ↑        ↑     ↑           ↑       │  │
│   │                 │        │  │     │        │     │           │       │  │
│   │                 └────────┴──┴─────┴────────┴─────┴───────────┘       │  │
│   │                   Trigram "abc" hashes to positions [1,4,5,7,10,12]  │  │
│   │                                                                      │  │
│   └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│   CHECK ALGORITHM:                                                          │
│   ┌──────────────────────────────────────────────────────────────────────┐  │
│   │  fn might_contain(trigram) -> bool {                                 │  │
│   │      for hash_fn in hash_functions {           // k=8 hash functions │  │
│   │          let pos = hash_fn(trigram) % m;       // m = filter size    │  │
│   │          if !bit_array[pos] {                                        │  │
│   │              return false;  // DEFINITELY not in set                 │  │
│   │          }                                                           │  │
│   │      }                                                               │  │
│   │      return true;  // MIGHT be in set (could be false positive)      │  │
│   │  }                                                                   │  │
│   └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│   PERFORMANCE IMPACT:                                                       │
│   ┌──────────────────────────────────────────────────────────────────────┐  │
│   │  Query: "rareTerm"                                                   │  │
│   │                                                                      │  │
│   │  Segment 1: Bloom check → FALSE → Skip (no postings lookup!)         │  │
│   │  Segment 2: Bloom check → FALSE → Skip                               │  │
│   │  Segment 3: Bloom check → TRUE  → Load postings, verify              │  │
│   │  Segment 4: Bloom check → FALSE → Skip                               │  │
│   │  ...                                                                 │  │
│   │                                                                      │  │
│   │  Result: Only 1 segment accessed instead of 12                       │  │
│   │  Speedup: ~12x for this query                                        │  │
│   └──────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 5. Query Processing Pipeline

### 5.1 Query AST Structure

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         QUERY AST EXAMPLES                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│   Query: "foo bar"                    Query: "foo | bar"                    │
│                                                                             │
│         And                                  Or                             │
│        /   \                                /  \                            │
│   Literal  Literal                    Literal  Literal                      │
│    "foo"    "bar"                      "foo"    "bar"                       │
│                                                                             │
│   ─────────────────────────────────────────────────────────────────────     │
│                                                                             │
│   Query: "ext:rs -test error"         Query: "near:foo,bar,5"               │
│                                                                             │
│              And                                Near                        │
│           /   |   \                        (distance=5)                     │
│     Filter  Not  Literal                      /    \                        │
│   (ext=rs)   |    "error"                 "foo"   "bar"                     │
│           Literal                                                           │
│           "test"                                                            │
│                                                                             │
│   ─────────────────────────────────────────────────────────────────────     │
│                                                                             │
│   QueryNode enum:                                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Literal(String)           // Exact string match                   │    │
│   │  Phrase(String)            // Quoted phrase "foo bar"              │    │
│   │  Regex(String)             // /pattern/                            │    │
│   │  Near { terms, distance }  // Proximity search                     │    │
│   │  And(Vec<QueryNode>)       // All must match                       │    │
│   │  Or(Vec<QueryNode>)        // Any can match                        │    │
│   │  Not(Box<QueryNode>)       // Exclude matches                      │    │
│   │  Empty                     // No query (list all)                  │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 5.2 Query Execution Plan

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      EXECUTION PLAN GENERATION                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   Input Query: "fn main ext:rs"                                             │
│                                                                              │
│   Step 1: Parse → AST                                                       │
│   ┌───────────────────────────────────────┐                                 │
│   │            And                        │                                 │
│   │           /   \                       │                                 │
│   │     Literal   Filter                  │                                 │
│   │    "fn main"  (ext=rs)                │                                 │
│   └───────────────────────────────────────┘                                 │
│                                                                              │
│   Step 2: Extract trigrams                                                  │
│   "fn main" → ["fn ", "n m", " ma", "mai", "ain"]                           │
│                                                                              │
│   Step 3: Check stop-grams                                                  │
│   ┌───────────────────────────────────────┐                                 │
│   │  "fn " → NOT in stop-grams → KEEP     │                                 │
│   │  "n m" → NOT in stop-grams → KEEP     │                                 │
│   │  " ma" → NOT in stop-grams → KEEP     │                                 │
│   │  "mai" → NOT in stop-grams → KEEP     │                                 │
│   │  "ain" → NOT in stop-grams → KEEP     │                                 │
│   └───────────────────────────────────────┘                                 │
│                                                                              │
│   Step 4: Order by selectivity (doc frequency ascending)                    │
│   ┌───────────────────────────────────────┐                                 │
│   │  "mai" → 5,000 docs   ← Process first │                                 │
│   │  "ain" → 8,000 docs                   │                                 │
│   │  "n m" → 12,000 docs                  │                                 │
│   │  "fn " → 45,000 docs                  │                                 │
│   │  " ma" → 50,000 docs  ← Process last  │                                 │
│   └───────────────────────────────────────┘                                 │
│                                                                              │
│   Step 5: Generate execution plan                                           │
│   ┌───────────────────────────────────────────────────────────────────────┐ │
│   │  ExecutionPlan {                                                      │ │
│   │    narrowing_steps: [                                                 │ │
│   │      TrigramIntersect(["mai", "ain", "n m", "fn ", " ma"]),           │ │
│   │      ExtensionFilter("rs"),                                           │ │
│   │    ],                                                                 │ │
│   │    verification_steps: [                                              │ │
│   │      LiteralSearch("fn main"),                                        │ │
│   │    ],                                                                 │ │
│   │    scoring: ScoreConfig { ... },                                      │ │
│   │  }                                                                    │ │
│   └───────────────────────────────────────────────────────────────────────┘ │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 5.3 Selectivity-Based Intersection

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    SELECTIVITY OPTIMIZATION                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   PROBLEM: Naive intersection processes huge intermediate sets              │
│                                                                              │
│   Query trigrams for "configuration":                                        │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Trigram    Doc Count    Processing Order                          │    │
│   │  ───────    ─────────    ────────────────                          │    │
│   │  "con"      150,000      ← LAST (most common)                      │    │
│   │  "onf"       45,000                                                │    │
│   │  "nfi"       12,000                                                │    │
│   │  "fig"        8,000                                                │    │
│   │  "igu"        3,000                                                │    │
│   │  "gur"        2,500                                                │    │
│   │  "ura"        2,000                                                │    │
│   │  "rat"       35,000                                                │    │
│   │  "ati"       40,000                                                │    │
│   │  "tio"       80,000                                                │    │
│   │  "ion"      120,000                                                │    │
│   │  "ura"        2,000      ← FIRST (rarest)                          │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   NAIVE ORDER (alphabetical):                                               │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  "ati" ∩ "con" ∩ "fig" ∩ "gur" ∩ "igu" ∩ "ion" ∩ ...              │    │
│   │                                                                     │    │
│   │  Step 1: Load 40,000 docs  ───┐                                    │    │
│   │  Step 2: Load 150,000 docs    │                                    │    │
│   │          Intersect → 35,000 ──┤ Large intermediate sets!           │    │
│   │  Step 3: Load 8,000 docs      │                                    │    │
│   │          Intersect → 6,000 ───┘                                    │    │
│   │  ...                                                               │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   SELECTIVITY ORDER (rarest first):                                         │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  "ura" ∩ "gur" ∩ "igu" ∩ "nfi" ∩ "fig" ∩ ...                      │    │
│   │                                                                     │    │
│   │  Step 1: Load 2,000 docs  ────┐                                    │    │
│   │  Step 2: Load 2,500 docs      │                                    │    │
│   │          Intersect → 800 ─────┤ Small intermediate sets!           │    │
│   │  Step 3: Load 3,000 docs      │                                    │    │
│   │          Intersect → 400 ─────┘                                    │    │
│   │  ...                                                               │    │
│   │  Final: 50 docs (10x faster!)                                      │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 5.4 Stop-Gram Filtering

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      STOP-GRAM OPTIMIZATION                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   PROBLEM: Some trigrams appear in 90%+ of files, providing no filtering    │
│                                                                              │
│   Top stop-grams (computed during indexing):                                │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Rank   Trigram     Appears In    Description                      │    │
│   │  ────   ───────     ──────────    ───────────                      │    │
│   │  1      "   "       98%           Three spaces                     │    │
│   │  2      "  \n"      97%           Two spaces + newline             │    │
│   │  3      "the"       89%           Common word                      │    │
│   │  4      "   "       88%           Tab + spaces                     │    │
│   │  5      " = "       85%           Assignment                       │    │
│   │  ...                                                               │    │
│   │  512    "for"       45%           Loop keyword                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   SOLUTION: Skip stop-grams during query execution                          │
│                                                                              │
│   Query: "the error"                                                        │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Trigrams: ["the", "he ", "e e", " er", "err", "rro", "ror"]       │    │
│   │                ↑                                                   │    │
│   │                └── SKIP (stop-gram)                                │    │
│   │                                                                     │    │
│   │  Used trigrams: ["he ", "e e", " er", "err", "rro", "ror"]         │    │
│   │                                                                     │    │
│   │  Candidate reduction: 150,000 → 2,500 (60x smaller!)               │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 6. Trigram Optimization

### 6.1 Tiered Extraction Strategy

```
┌─────────────────────────────────────────────────────────────────────────────┐
│              TIERED TRIGRAM EXTRACTION (Commit #39 Optimization)             │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   PROBLEM: One-size-fits-all approach is suboptimal                         │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  File Size    Best Strategy    Why                                 │    │
│   │  ─────────    ─────────────    ───                                 │    │
│   │  < 4KB        Sort + Dedup     Cache-friendly, no allocation       │    │
│   │  4KB-100KB    HashSet          O(1) insert, moderate memory        │    │
│   │  100KB-1MB    Sparse Bitset    Reduced memory, fast operations     │    │
│   │  > 1MB        Full Bitset      O(1) everything, worth 2MB alloc    │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   TIER 1: TINY FILES (< 4KB) - Sort + Dedup                                 │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  fn extract_tiny(content: &[u8]) -> Vec<Trigram> {                 │    │
│   │      let mut trigrams = Vec::new();                                │    │
│   │      for window in content.windows(3) {                            │    │
│   │          trigrams.push(to_trigram(window));                        │    │
│   │      }                                                             │    │
│   │      trigrams.sort_unstable();  // In-place, cache-optimal         │    │
│   │      trigrams.dedup();          // Remove duplicates               │    │
│   │      trigrams                                                      │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  Characteristics:                                                  │    │
│   │  • Memory: ~4KB (same as input)                                    │    │
│   │  • Time: O(n log n) but very fast due to cache locality            │    │
│   │  • No hash table overhead                                          │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   TIER 2: SMALL FILES (4KB-100KB) - HashSet                                 │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  fn extract_small(content: &[u8]) -> Vec<Trigram> {                │    │
│   │      let capacity = content.len() / 8;  // Tuned capacity          │    │
│   │      let mut seen = AHashSet::with_capacity(capacity);             │    │
│   │      for window in content.windows(3) {                            │    │
│   │          seen.insert(to_trigram(window));                          │    │
│   │      }                                                             │    │
│   │      seen.into_iter().collect()                                    │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  Characteristics:                                                  │    │
│   │  • Memory: ~capacity × 8 bytes                                     │    │
│   │  • Time: O(n) average                                              │    │
│   │  • Good for moderate unique trigram counts                         │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   TIER 3: MEDIUM FILES (100KB-1MB) - Sparse Bitset                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  struct SparseTrigramBitset {                                      │    │
│   │      blocks: AHashMap<u32, u64>,  // block_index → 64 bits         │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  impl SparseTrigramBitset {                                        │    │
│   │      fn insert(&mut self, trigram: u32) {                          │    │
│   │          let block_idx = trigram >> 6;       // Divide by 64       │    │
│   │          let bit_pos = trigram & 63;         // Mod 64             │    │
│   │          *self.blocks.entry(block_idx).or_insert(0) |= 1 << bit_pos│    │
│   │      }                                                             │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  Characteristics:                                                  │    │
│   │  • Memory: ~8KB-64KB (only non-zero blocks)                        │    │
│   │  • Time: O(n) with fast hash lookups                               │    │
│   │  • Much smaller than 2MB full bitset                               │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   TIER 4: LARGE FILES (> 1MB) - Full Bitset                                 │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  struct FullTrigramBitset {                                        │    │
│   │      bits: Vec<u64>,  // 2^24 / 64 = 262,144 words = 2MB           │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  impl FullTrigramBitset {                                          │    │
│   │      fn insert(&mut self, trigram: u32) {                          │    │
│   │          let word_idx = (trigram >> 6) as usize;                   │    │
│   │          let bit_pos = trigram & 63;                               │    │
│   │          self.bits[word_idx] |= 1 << bit_pos;                      │    │
│   │      }                                                             │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  Characteristics:                                                  │    │
│   │  • Memory: 2MB fixed                                               │    │
│   │  • Time: O(1) per trigram (no hashing!)                            │    │
│   │  • Best for files with many unique trigrams                        │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   PERFORMANCE COMPARISON:                                                   │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  File Size   Before (one strategy)   After (tiered)   Improvement  │    │
│   │  ─────────   ────────────────────    ──────────────   ───────────  │    │
│   │  1KB         150µs (HashSet)         80µs (sort)      47% faster   │    │
│   │  50KB        1.2ms (full bitset)     0.9ms (HashSet)  25% faster   │    │
│   │  500KB       8ms (full bitset)       3ms (sparse)     63% faster   │    │
│   │  5MB         45ms (full bitset)      40ms (full)      11% faster   │    │
│   │                                                                     │    │
│   │  Overall indexing: 30-40% faster                                   │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 7. Memory Optimization

### 7.1 Delta + Varint Encoding

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    POSTING LIST COMPRESSION                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   RAW ENCODING (naive):                                                      │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Doc IDs: [1, 5, 10, 15, 100, 105, 200, 250, 1000]                  │    │
│   │                                                                     │    │
│   │  As u32: [00000001, 00000005, 0000000A, ...]                        │    │
│   │          │         │         │                                      │    │
│   │          4 bytes   4 bytes   4 bytes                               │    │
│   │                                                                     │    │
│   │  Total: 9 × 4 = 36 bytes                                           │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   DELTA ENCODING:                                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Original: [1,   5,  10,  15, 100, 105, 200, 250, 1000]            │    │
│   │  Deltas:   [1,   4,   5,   5,  85,   5,  95,  50,  750]            │    │
│   │             ↑    ↑    ↑    ↑    ↑    ↑    ↑    ↑    ↑              │    │
│   │             │    │    │    │    │    │    │    │    │              │    │
│   │            first 5-1  10-5      105-100                            │    │
│   │                                                                     │    │
│   │  Observation: Most deltas are small numbers!                       │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   VARINT ENCODING:                                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Encoding scheme (like protobuf):                                  │    │
│   │    • If value < 128: 1 byte  (0xxxxxxx)                            │    │
│   │    • If value < 16384: 2 bytes (1xxxxxxx 0xxxxxxx)                 │    │
│   │    • If value < 2M: 3 bytes (1xxxxxxx 1xxxxxxx 0xxxxxxx)           │    │
│   │    • etc.                                                          │    │
│   │                                                                     │    │
│   │  Example:                                                          │    │
│   │    Delta 5   → 0x05          (1 byte)                              │    │
│   │    Delta 85  → 0x55          (1 byte)                              │    │
│   │    Delta 750 → 0xEE 0x05     (2 bytes: 750 = 0x2EE)                │    │
│   │                                                                     │    │
│   │  Result for our example:                                           │    │
│   │  [01, 04, 05, 05, 55, 05, 5F, 32, EE 05]                           │    │
│   │   1B  1B  1B  1B  1B  1B  1B  1B   2B   = 11 bytes                 │    │
│   │                                                                     │    │
│   │  Compression ratio: 36 → 11 bytes (69% reduction!)                 │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   DECOMPRESSION (fast path):                                                │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  fn delta_decode(bytes: &[u8]) -> Vec<u32> {                       │    │
│   │      let mut result = Vec::with_capacity(bytes.len());  // Pre-alloc│    │
│   │      let mut pos = 0;                                              │    │
│   │      let mut prev = 0u32;                                          │    │
│   │                                                                     │    │
│   │      while pos < bytes.len() {                                     │    │
│   │          let (delta, consumed) = decode_varint(&bytes[pos..]);     │    │
│   │          prev += delta;                                            │    │
│   │          result.push(prev);                                        │    │
│   │          pos += consumed;                                          │    │
│   │      }                                                             │    │
│   │      result                                                        │    │
│   │  }                                                                 │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 7.2 Memory-Mapped I/O Strategy

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    HYBRID FILE READING STRATEGY                              │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   Decision flow:                                                            │
│                                                                              │
│                     ┌─────────────────┐                                     │
│                     │  Read file for  │                                     │
│                     │  verification   │                                     │
│                     └────────┬────────┘                                     │
│                              │                                              │
│                              ▼                                              │
│                     ┌─────────────────┐                                     │
│                     │ file_size < 4KB │                                     │
│                     └────────┬────────┘                                     │
│                              │                                              │
│               ┌──────────────┴──────────────┐                               │
│               │ YES                         │ NO                            │
│               ▼                             ▼                               │
│     ┌─────────────────┐           ┌─────────────────┐                       │
│     │ fs::read_to_    │           │ Mmap::map()     │                       │
│     │ string()        │           │                 │                       │
│     └─────────────────┘           └─────────────────┘                       │
│              │                             │                                │
│              ▼                             ▼                                │
│     ┌─────────────────┐           ┌─────────────────┐                       │
│     │ Data copied to  │           │ Virtual mapping │                       │
│     │ heap (owned)    │           │ (zero-copy)     │                       │
│     └─────────────────┘           └─────────────────┘                       │
│                                                                              │
│   WHY THIS MATTERS:                                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │  Small file (< 4KB):                                               │    │
│   │  ├─ read() syscall: ~2µs                                           │    │
│   │  ├─ Memory allocation: ~0.5µs                                      │    │
│   │  └─ Total: ~2.5µs                                                  │    │
│   │                                                                     │    │
│   │  vs. mmap for small file:                                          │    │
│   │  ├─ mmap() syscall: ~5µs                                           │    │
│   │  ├─ Page table setup: ~3µs                                         │    │
│   │  ├─ Page fault on access: ~2µs                                     │    │
│   │  └─ Total: ~10µs (4x slower!)                                      │    │
│   │                                                                     │    │
│   │  ────────────────────────────────────────────────────────────────  │    │
│   │                                                                     │    │
│   │  Large file (1MB):                                                 │    │
│   │  ├─ read() syscall: ~500µs                                         │    │
│   │  ├─ Memory allocation: ~50µs (1MB heap!)                           │    │
│   │  ├─ Copy from kernel: ~200µs                                       │    │
│   │  └─ Total: ~750µs                                                  │    │
│   │                                                                     │    │
│   │  vs. mmap for large file:                                          │    │
│   │  ├─ mmap() syscall: ~5µs                                           │    │
│   │  ├─ Page table setup: ~10µs                                        │    │
│   │  ├─ Demand paging: OS handles                                      │    │
│   │  └─ Total: ~15µs + incremental faults (50x faster startup!)        │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 7.3 Fixed-Size Document Entries

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                  DOCUMENT TABLE LAYOUT (30 bytes per entry)                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │ Offset │ Field      │ Type │ Size │ Description                     │   │
│   │ ────── │ ─────      │ ──── │ ──── │ ───────────                     │   │
│   │ 0      │ doc_id     │ u32  │ 4B   │ Unique document ID              │   │
│   │ 4      │ path_id    │ u32  │ 4B   │ Index into paths.bin            │   │
│   │ 8      │ size       │ u64  │ 8B   │ File size in bytes              │   │
│   │ 16     │ mtime      │ u64  │ 8B   │ Last modification (unix time)   │   │
│   │ 24     │ language   │ u16  │ 2B   │ Language enum (Rust=0, etc.)    │   │
│   │ 26     │ flags      │ u16  │ 2B   │ Bitflags (MINIFIED, STALE, etc.)│   │
│   │ 28     │ segment_id │ u16  │ 2B   │ Which segment contains doc      │   │
│   │ ────── │ ─────      │ ──── │ ──── │                                 │   │
│   │ Total  │            │      │ 30B  │                                 │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│   BENEFITS:                                                                 │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  1. Direct memory mapping: No deserialization needed               │    │
│   │     - Cast byte slice to Document slice: O(1)                      │    │
│   │     - get_document(doc_id) = docs[doc_id_to_index[doc_id]]         │    │
│   │                                                                     │    │
│   │  2. Cache-friendly: Predictable memory layout                      │    │
│   │     - CPU prefetcher works efficiently                             │    │
│   │     - 30 bytes fits in cache line                                  │    │
│   │                                                                     │    │
│   │  3. Compact: 150,000 docs × 30B = 4.5MB                            │    │
│   │     - Fits in L3 cache on most CPUs                                │    │
│   │                                                                     │    │
│   │  4. No pointers: Safe for mmap                                     │    │
│   │     - Can be loaded directly from disk                             │    │
│   │     - No pointer fixup required                                    │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   FLAGS BITFIELD:                                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Bit │ Name      │ Description                                     │    │
│   │  ─── │ ────      │ ───────────                                     │    │
│   │  0   │ MINIFIED  │ File is minified JS/CSS (skip detailed parsing) │    │
│   │  1   │ STALE     │ File changed since last index                   │    │
│   │  2   │ TOMBSTONE │ File deleted (marked for removal)               │    │
│   │  3   │ BINARY    │ Binary file detected                            │    │
│   │  4   │ GENERATED │ Auto-generated code (lower priority)            │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 8. Parallelization Strategy

### 8.1 Rayon-Based Data Parallelism

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    PARALLEL EXECUTION MODEL                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   INDEXING PARALLELISM:                                                     │
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │                                                                      │   │
│   │   Main Thread              Worker Pool (rayon)                      │   │
│   │   ───────────              ───────────────────                      │   │
│   │                                                                      │   │
│   │   ┌─────────┐              ┌─────────┐  ┌─────────┐                 │   │
│   │   │ Walk    │─────────────▶│ Worker 1│  │ Worker 2│                 │   │
│   │   │ Builder │  file paths  │ Process │  │ Process │                 │   │
│   │   │(parallel│─────────────▶│ File A  │  │ File C  │                 │   │
│   │   │ walker) │              └────┬────┘  └────┬────┘                 │   │
│   │   └─────────┘                   │            │                      │   │
│   │        │                        ▼            ▼                      │   │
│   │        │                   ┌─────────┐  ┌─────────┐                 │   │
│   │        │                   │ Worker 3│  │ Worker 4│                 │   │
│   │        └──────────────────▶│ Process │  │ Process │                 │   │
│   │                            │ File B  │  │ File D  │                 │   │
│   │                            └────┬────┘  └────┬────┘                 │   │
│   │                                 │            │                      │   │
│   │                                 └──────┬─────┘                      │   │
│   │                                        ▼                            │   │
│   │                              ┌──────────────────┐                   │   │
│   │                              │   Chunk Buffer   │                   │   │
│   │                              │  (ProcessedFiles)│                   │   │
│   │                              └────────┬─────────┘                   │   │
│   │                                       │                             │   │
│   │                                       ▼                             │   │
│   │                              ┌──────────────────┐                   │   │
│   │                              │ Background Write │                   │   │
│   │                              │     Thread       │                   │   │
│   │                              └──────────────────┘                   │   │
│   │                                                                      │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│   QUERY PARALLELISM:                                                        │
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │                                                                      │   │
│   │   Query: "fn main"                                                  │   │
│   │                                                                      │   │
│   │   Phase 1: Parallel Segment Scanning                                │   │
│   │   ┌─────────────────────────────────────────────────────────────┐   │   │
│   │   │                                                              │   │   │
│   │   │  Segment 1    Segment 2    Segment 3    Segment 4           │   │   │
│   │   │  ─────────    ─────────    ─────────    ─────────           │   │   │
│   │   │  │ Bloom ✗│   │ Bloom ✓│   │ Bloom ✓│   │ Bloom ✗│           │   │   │
│   │   │  │ SKIP   │   │ Lookup │   │ Lookup │   │ SKIP   │           │   │   │
│   │   │  └────────┘   │ 500docs│   │ 200docs│   └────────┘           │   │   │
│   │   │               └────┬───┘   └────┬───┘                        │   │   │
│   │   │                    └─────┬──────┘                            │   │   │
│   │   │                          ▼                                   │   │   │
│   │   │                   RoaringBitmap                              │   │   │
│   │   │                   (700 candidates)                           │   │   │
│   │   │                                                              │   │   │
│   │   └─────────────────────────────────────────────────────────────┘   │   │
│   │                                                                      │   │
│   │   Phase 2: Adaptive Parallel Verification                           │   │
│   │   ┌─────────────────────────────────────────────────────────────┐   │   │
│   │   │                                                              │   │   │
│   │   │  Decision: candidates (700) > threshold (num_cpus × 4)?      │   │   │
│   │   │                                                              │   │   │
│   │   │  YES (700 > 32) → Use parallel iterator                     │   │   │
│   │   │                                                              │   │   │
│   │   │   candidates.par_iter()                                     │   │   │
│   │   │      .with_min_len(4)    // Work-stealing granularity        │   │   │
│   │   │      .filter_map(|doc| {                                    │   │   │
│   │   │          let content = read_file(doc.path);                 │   │   │
│   │   │          search_literal(content, "fn main")                 │   │   │
│   │   │      })                                                      │   │   │
│   │   │      .take_any_while(|_| !limit_reached())                  │   │   │
│   │   │      .collect()                                             │   │   │
│   │   │                                                              │   │   │
│   │   └─────────────────────────────────────────────────────────────┘   │   │
│   │                                                                      │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 8.2 Index Reader Parallel Loading

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                   PARALLEL INDEX STARTUP                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   SEQUENTIAL LOADING (naive):                                               │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Time: ───────────────────────────────────────────────────────▶    │    │
│   │                                                                     │    │
│   │  ┌─────────┐                                                       │    │
│   │  │ docs.bin│ 100ms                                                 │    │
│   │  └─────────┘                                                       │    │
│   │            ┌──────────┐                                            │    │
│   │            │paths.bin │ 80ms                                       │    │
│   │            └──────────┘                                            │    │
│   │                       ┌─────────────────────────────────────────┐  │    │
│   │                       │ segments (12 × 50ms = 600ms)            │  │    │
│   │                       └─────────────────────────────────────────┘  │    │
│   │                                                                     │    │
│   │  Total: 780ms                                                      │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   PARALLEL LOADING (optimized):                                             │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Time: ───────────────────────────────────────────────────────▶    │    │
│   │                                                                     │    │
│   │  ┌─────────┐                                                       │    │
│   │  │ docs.bin│ 100ms    ─┐                                           │    │
│   │  └─────────┘           │                                           │    │
│   │  ┌──────────┐          │                                           │    │
│   │  │paths.bin │ 80ms     ├─ rayon::join()                            │    │
│   │  └──────────┘          │                                           │    │
│   │  ┌────┐┌────┐┌────┐... │                                           │    │
│   │  │seg1││seg2││seg3│... ├─ par_iter (12 segments)                   │    │
│   │  │50ms││50ms││50ms│    │                                           │    │
│   │  └────┘└────┘└────┘... ─┘                                          │    │
│   │                                                                     │    │
│   │  Total: ~150ms (5x faster!)                                        │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   CODE:                                                                      │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  let (documents_result, (paths_result, segments)) = rayon::join(   │    │
│   │      || read_documents(index_path),     // Thread 1                │    │
│   │      || rayon::join(                                               │    │
│   │          || read_paths(index_path),     // Thread 2                │    │
│   │          || segments.par_iter()         // Threads 3-N             │    │
│   │              .map(|seg| SegmentReader::open(seg))                  │    │
│   │              .collect::<Vec<_>>()                                  │    │
│   │      )                                                             │    │
│   │  );                                                                │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 8.3 Background Segment Writing

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                  ASYNC SEGMENT WRITING                                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   PROBLEM: Segment writing blocks indexing progress                         │
│                                                                              │
│   SOLUTION: Dedicated background thread for I/O                             │
│                                                                              │
│   ┌─────────────────────────────────────────────────────────────────────┐   │
│   │                                                                      │   │
│   │   Main Thread                      Background Thread                │   │
│   │   ───────────                      ─────────────────                │   │
│   │                                                                      │   │
│   │   ┌───────────────┐                                                 │   │
│   │   │ Process chunk │                                                 │   │
│   │   │ 1 (1000 files)│                                                 │   │
│   │   └───────┬───────┘                                                 │   │
│   │           │                                                          │   │
│   │           │ send(SegmentWriteJob)                                   │   │
│   │           │ ────────────────────▶   ┌──────────────────┐            │   │
│   │           │ (non-blocking)          │ Receive job      │            │   │
│   │   ┌───────┴───────┐                 │ Build dictionary │            │   │
│   │   │ Process chunk │                 │ Encode postings  │            │   │
│   │   │ 2 (1000 files)│                 │ Write to disk    │            │   │
│   │   └───────┬───────┘                 └──────────────────┘            │   │
│   │           │                                                          │   │
│   │           │ send(SegmentWriteJob)                                   │   │
│   │           │ ────────────────────▶   ┌──────────────────┐            │   │
│   │           │                         │ Receive job      │            │   │
│   │   ┌───────┴───────┐                 │ Build dictionary │            │   │
│   │   │ Process chunk │                 │ Encode postings  │            │   │
│   │   │ 3 (1000 files)│                 │ Write to disk    │            │   │
│   │   └───────────────┘                 └──────────────────┘            │   │
│   │                                                                      │   │
│   │   OVERLAP: Main thread processes next chunk while background        │   │
│   │            thread writes previous chunk                             │   │
│   │                                                                      │   │
│   └─────────────────────────────────────────────────────────────────────┘   │
│                                                                              │
│   MEMORY MANAGEMENT:                                                        │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  • Chunk size: 1000-5000 files (configurable)                      │    │
│   │  • Only ONE chunk's worth of ProcessedFiles in memory              │    │
│   │  • Background thread consumes and frees memory as it writes        │    │
│   │  • Bounded memory usage even for multi-million file codebases      │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 9. Caching Mechanisms

### 9.1 LRU File Content Cache

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                       FILE CONTENT CACHE                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   Configuration:                                                            │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  MAX_CACHED_FILES = 256                                            │    │
│   │  MAX_FILE_SIZE = 512KB                                             │    │
│   │  Total max memory: 256 × 512KB = 128MB                             │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   Cache structure:                                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  file_cache: Mutex<LruCache<PathBuf, String>>                      │    │
│   │                                                                     │    │
│   │  LRU eviction: Least recently used files evicted first             │    │
│   │                                                                     │    │
│   │  ┌─────────────────────────────────────────────────────────────┐   │    │
│   │  │ MRU ←───────────────────────────────────────────────→ LRU   │   │    │
│   │  │                                                              │   │    │
│   │  │ [main.rs] [lib.rs] [utils.rs] [config.rs] ... [old_file.rs] │   │    │
│   │  │     ↑                                              ↑         │   │    │
│   │  │  accessed                                       evicted      │   │    │
│   │  │  recently                                       when full    │   │    │
│   │  └─────────────────────────────────────────────────────────────┘   │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   When cache is beneficial:                                                 │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  1. Interactive TUI: Same files re-queried rapidly                 │    │
│   │  2. Context extraction: File read multiple times for context       │    │
│   │  3. Proximity search: Multiple passes over same file              │    │
│   │  4. Boolean queries: File checked for multiple terms              │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   Sequential vs. Parallel access:                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Sequential (small result set):                                    │    │
│   │    • Uses cache (Mutex lock acceptable)                            │    │
│   │    • Cache hits avoid file I/O entirely                            │    │
│   │                                                                     │    │
│   │  Parallel (large result set):                                      │    │
│   │    • Bypasses cache (lock contention would hurt)                   │    │
│   │    • Uses memory-mapped I/O instead                                │    │
│   │    • Relies on OS page cache                                       │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 9.2 Regex Compilation Cache

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                       REGEX CACHE                                            │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   PROBLEM: Regex compilation is expensive (~100µs-1ms per pattern)          │
│                                                                              │
│   SOLUTION: Global thread-safe regex cache                                  │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  static REGEX_CACHE: OnceLock<RegexCache> = OnceLock::new();       │    │
│   │                                                                     │    │
│   │  struct RegexCache {                                               │    │
│   │      cache: RwLock<HashMap<String, Arc<Regex>>>,                   │    │
│   │      max_size: 64,                                                 │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  impl RegexCache {                                                 │    │
│   │      fn get_or_compile(&self, pattern: &str) -> Arc<Regex> {       │    │
│   │          // Fast path: read lock for cache hit                     │    │
│   │          if let Some(regex) = self.cache.read().get(pattern) {     │    │
│   │              return Arc::clone(regex);                             │    │
│   │          }                                                         │    │
│   │                                                                     │    │
│   │          // Slow path: write lock for compilation                  │    │
│   │          let regex = Arc::new(Regex::new(pattern)?);               │    │
│   │          self.cache.write().insert(pattern.to_string(), regex);    │    │
│   │          regex                                                     │    │
│   │      }                                                             │    │
│   │  }                                                                 │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   CONCURRENCY:                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │  Thread 1 ──▶ cache.read() ──▶ HIT ──▶ return Arc<Regex>          │    │
│   │  Thread 2 ──▶ cache.read() ──▶ HIT ──▶ return Arc<Regex>          │    │
│   │  Thread 3 ──▶ cache.read() ──▶ HIT ──▶ return Arc<Regex>          │    │
│   │                    ▲                                               │    │
│   │                    │ All readers concurrent (RwLock)               │    │
│   │                                                                     │    │
│   │  Thread 4 ──▶ cache.read() ──▶ MISS ──▶ compile ──▶ cache.write() │    │
│   │                                              │                      │    │
│   │                                              └─ Exclusive access    │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 9.3 Lazy Line Map Loading

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    LAZY LOADING OPTIMIZATION                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   OBSERVATION: Most queries don't need line numbers                         │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Query Type              Line Numbers Needed?                      │    │
│   │  ──────────              ─────────────────────                      │    │
│   │  fxi -l "pattern"        NO (file list only)                       │    │
│   │  fxi "pattern"           YES (show matches with line numbers)      │    │
│   │  fxi -c "pattern"        NO (count only)                           │    │
│   │  fxi -l                  NO (list all files)                       │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   IMPLEMENTATION:                                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  struct IndexReader {                                              │    │
│   │      documents: Vec<Document>,                                     │    │
│   │      paths: PathStore,                                             │    │
│   │      segments: Vec<SegmentReader>,                                 │    │
│   │                                                                     │    │
│   │      // Only loaded when first accessed                            │    │
│   │      line_maps: OnceLock<HashMap<DocId, Vec<u32>>>,                │    │
│   │  }                                                                 │    │
│   │                                                                     │    │
│   │  impl IndexReader {                                                │    │
│   │      fn get_line_map(&self, doc_id: DocId) -> &Vec<u32> {          │    │
│   │          self.line_maps                                            │    │
│   │              .get_or_init(|| self.load_all_line_maps())            │    │
│   │              .get(&doc_id)                                         │    │
│   │              .unwrap()                                             │    │
│   │      }                                                             │    │
│   │  }                                                                 │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   MEMORY SAVINGS:                                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Index with 150,000 files:                                         │    │
│   │    • Average 500 lines per file                                    │    │
│   │    • Line map: 500 × 4 bytes = 2KB per file                        │    │
│   │    • Total: 150,000 × 2KB = 300MB                                  │    │
│   │                                                                     │    │
│   │  With lazy loading:                                                │    │
│   │    • -l queries: 0 bytes                                           │    │
│   │    • Content queries: 300MB (only when needed)                     │    │
│   │                                                                     │    │
│   │  Most interactive usage never loads line maps!                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 10. Scoring & Ranking

### 10.1 Multi-Factor Scoring Algorithm

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      RELEVANCE SCORING                                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   FINAL SCORE = Σ (factor × weight) × boost                                 │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Factor           │ Formula                     │ Weight │ Max     │    │
│   │  ─────────────    │ ─────────                   │ ────── │ ───     │    │
│   │  Match Count      │ log2(count + 1)             │ 1.0    │ ~10     │    │
│   │  Filename Match   │ if term in filename: 2.0    │ 2.0    │ 2.0     │    │
│   │  Path Depth       │ -0.05 × depth (max -0.5)    │ 0.05   │ -0.5    │    │
│   │  Recency          │ e^(-age_days / 7)           │ 0.5    │ 0.5     │    │
│   │  User Boost       │ ^N syntax multiplier        │ N      │ 10      │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   EXAMPLE CALCULATION:                                                       │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Query: "error" in file src/utils/error_handler.rs                 │    │
│   │                                                                     │    │
│   │  Matches: 15 occurrences                                           │    │
│   │  Path depth: 2 (src/utils/)                                        │    │
│   │  Modified: 3 days ago                                              │    │
│   │  Term "error" appears in filename                                  │    │
│   │                                                                     │    │
│   │  Score breakdown:                                                  │    │
│   │  ┌──────────────────────────────────────────────────────────────┐  │    │
│   │  │  Match count:   log2(15 + 1) × 1.0           = 4.0           │  │    │
│   │  │  Filename:      2.0 × 2.0                     = 4.0           │  │    │
│   │  │  Depth:         -0.05 × 2                     = -0.1          │  │    │
│   │  │  Recency:       e^(-3/7) × 0.5               = 0.32          │  │    │
│   │  │  ────────────────────────────────────────────────────────    │  │    │
│   │  │  TOTAL:                                       = 8.22          │  │    │
│   │  └──────────────────────────────────────────────────────────────┘  │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   LOGARITHMIC MATCH WEIGHTING:                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                                                                     │    │
│   │  WHY: Prevents huge files from dominating results                  │    │
│   │                                                                     │    │
│   │  Matches    Linear Score    Log Score                              │    │
│   │  ───────    ────────────    ─────────                              │    │
│   │  1          1               1.0                                    │    │
│   │  10         10              3.5                                    │    │
│   │  100        100             6.7                                    │    │
│   │  1000       1000            10.0                                   │    │
│   │                                                                     │    │
│   │  Effect: File with 1000 matches scores only 3x higher than         │    │
│   │          file with 10 matches (instead of 100x)                    │    │
│   │                                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 10.2 Early Termination Strategy

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    EARLY TERMINATION                                         │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   OBSERVATION: Users rarely need ALL matches                                │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Default limit: 100 results                                        │    │
│   │  User-specified: fxi -m 20 "pattern" (20 results)                  │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   TERMINATION STRATEGIES:                                                   │
│                                                                              │
│   1. File-only mode (-l):                                                   │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  // Atomic counter tracks matching files                           │    │
│   │  let match_count = AtomicUsize::new(0);                            │    │
│   │                                                                     │    │
│   │  candidates.par_iter().for_each(|doc| {                            │    │
│   │      // Early exit check                                           │    │
│   │      if match_count.load(Ordering::Relaxed) >= limit {             │    │
│   │          return;  // Don't process more files                      │    │
│   │      }                                                             │    │
│   │                                                                     │    │
│   │      if file_matches(doc) {                                        │    │
│   │          match_count.fetch_add(1, Ordering::Relaxed);              │    │
│   │      }                                                             │    │
│   │  });                                                               │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   2. Content search with ranking:                                           │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  // Collect 1.5x limit for better ranking                          │    │
│   │  let target = limit + (limit / 2);                                 │    │
│   │                                                                     │    │
│   │  let results = candidates                                          │    │
│   │      .par_iter()                                                   │    │
│   │      .filter_map(|doc| search_file(doc))                           │    │
│   │      .take_any_while(|_| results.len() < target)  // Early exit    │    │
│   │      .collect::<Vec<_>>();                                         │    │
│   │                                                                     │    │
│   │  // Sort by score and take top `limit`                             │    │
│   │  results.sort_by(|a, b| b.score.partial_cmp(&a.score));            │    │
│   │  results.truncate(limit);                                          │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   PERFORMANCE IMPACT:                                                        │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Query: "import" (very common)                                     │    │
│   │  Codebase: 150,000 files                                           │    │
│   │  Matches in: 100,000 files                                         │    │
│   │                                                                     │    │
│   │  Without early termination:                                        │    │
│   │    • Process all 100,000 matching files                            │    │
│   │    • Time: ~10 seconds                                             │    │
│   │                                                                     │    │
│   │  With early termination (limit=100):                               │    │
│   │    • Process ~150 files (1.5x for ranking)                         │    │
│   │    • Time: ~15ms (670x faster!)                                    │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## 11. Performance Characteristics

### 11.1 Benchmark Results

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                      PERFORMANCE BENCHMARKS                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   TEST ENVIRONMENT:                                                         │
│   • CPU: Apple M1 Pro (8 cores)                                             │
│   • RAM: 16GB                                                               │
│   • SSD: NVMe                                                               │
│   • Codebase: Linux kernel (80,000 files, 30M LOC)                          │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                       SEARCH PERFORMANCE                            │    │
│   ├────────────────────────────────────────────────────────────────────┤    │
│   │  Query Type              fxi         ripgrep      Speedup          │    │
│   │  ──────────              ───         ───────      ───────          │    │
│   │  Common literal          8ms         3,200ms      400x             │    │
│   │  (e.g., "return")                                                  │    │
│   │                                                                     │    │
│   │  Rare literal            3ms         3,200ms      1,067x           │    │
│   │  (e.g., "xyzzy123")                                                │    │
│   │                                                                     │    │
│   │  Regex with prefix       45ms        4,500ms      100x             │    │
│   │  (e.g., "error.*hand")                                             │    │
│   │                                                                     │    │
│   │  File list (-l)          5ms         2,800ms      560x             │    │
│   │                                                                     │    │
│   │  Extension filter        12ms        800ms        67x              │    │
│   │  (ext:c error)                                                     │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                      INDEXING PERFORMANCE                           │    │
│   ├────────────────────────────────────────────────────────────────────┤    │
│   │  Phase                   Time         Rate                          │    │
│   │  ─────                   ────         ────                          │    │
│   │  File discovery          8s           10,000 files/s                │    │
│   │  Trigram extraction      25s          3,200 files/s                 │    │
│   │  Segment writing         15s          5,300 files/s                 │    │
│   │  ─────────────────────────────────────────────────────────────────  │    │
│   │  Total                   48s          1,667 files/s                 │    │
│   │                                                                     │    │
│   │  Index size: 2.1GB (7% of source size)                             │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │                       MEMORY USAGE                                  │    │
│   ├────────────────────────────────────────────────────────────────────┤    │
│   │  Component                          Size                           │    │
│   │  ─────────                          ────                           │    │
│   │  Index reader (cold start)          85MB                           │    │
│   │  Index reader (warm, all segments)  450MB                          │    │
│   │  Query execution (typical)          50-100MB                       │    │
│   │  File cache (when used)             up to 128MB                    │    │
│   │                                                                     │    │
│   │  Peak during indexing:              1.2GB                          │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 11.2 Complexity Analysis

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                     TIME COMPLEXITY ANALYSIS                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   INDEXING:                                                                 │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Operation                 Complexity      Notes                   │    │
│   │  ─────────                 ──────────      ─────                   │    │
│   │  File discovery            O(F)            F = file count          │    │
│   │  Trigram extraction        O(F × S)        S = avg file size       │    │
│   │  Dictionary building       O(T log T)      T = unique trigrams     │    │
│   │  Posting encoding          O(P)            P = total postings      │    │
│   │                                                                     │    │
│   │  Total: O(F × S + T log T)                                         │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   QUERY EXECUTION:                                                          │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Operation                 Complexity      Notes                   │    │
│   │  ─────────                 ──────────      ─────                   │    │
│   │  Trigram lookup            O(log D)        D = dict entries        │    │
│   │  Posting decode            O(P)            P = posting list size   │    │
│   │  Bitmap intersection       O(min(P1,P2))   Roaring optimization    │    │
│   │  Content verification      O(C × M)        C = candidates, M = match│   │
│   │                                                                     │    │
│   │  Total: O(log D + P + C × M)                                       │    │
│   │                                                                     │    │
│   │  With early termination: O(log D + L × M)  L = limit               │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
│   SPACE COMPLEXITY:                                                         │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Component                 Size Formula                            │    │
│   │  ─────────                 ────────────                            │    │
│   │  Document table            O(F × 30 bytes)                         │    │
│   │  Trigram dictionary        O(T × 20 bytes)                         │    │
│   │  Posting lists             O(P × ~2.5 bytes) (compressed)          │    │
│   │  Bloom filters             O(T × 1.2 bytes)                        │    │
│   │                                                                     │    │
│   │  Total index: ~5-10% of source code size                           │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 11.3 Optimization Trade-offs

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                       DESIGN TRADE-OFFS                                      │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   ┌────────────────────────────────────────────────────────────────────┐    │
│   │  Trade-off                  Benefit               Cost             │    │
│   │  ──────────                 ───────               ────             │    │
│   │                                                                     │    │
│   │  Persistent index           100-400x faster       Disk space       │    │
│   │                             searches              (5-10% of src)   │    │
│   │                                                                     │    │
│   │  Stop-gram filtering        Smaller candidate     May miss some    │    │
│   │                             sets                  very common terms│    │
│   │                                                                     │    │
│   │  Bloom filters              Fast segment skip     1% false positive│    │
│   │                                                   (still correct)  │    │
│   │                                                                     │    │
│   │  Early termination          2-10x faster for      May miss better  │    │
│   │                             limited queries       matches           │    │
│   │                                                                     │    │
│   │  Tiered trigram extraction  30-40% faster index   Code complexity  │    │
│   │                                                                     │    │
│   │  Token index                Exact word matches    Additional index │    │
│   │                                                   space            │    │
│   │                                                                     │    │
│   │  Delta encoding             60-70% smaller index  Decode overhead  │    │
│   │                                                   (minimal)        │    │
│   │                                                                     │    │
│   │  Memory-mapped I/O          Zero-copy for large   Page fault cost  │    │
│   │                             files                 for small files  │    │
│   └────────────────────────────────────────────────────────────────────┘    │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Appendix A: Query Syntax Reference

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                        QUERY SYNTAX                                          │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  LITERALS                                                                   │
│  ────────                                                                   │
│  foo              Substring match for "foo"                                 │
│  "foo bar"        Exact phrase match                                        │
│  /pattern/        Regular expression                                        │
│                                                                              │
│  BOOLEAN OPERATORS                                                          │
│  ─────────────────                                                          │
│  foo bar          AND: both must match                                      │
│  foo | bar        OR: either can match                                      │
│  -foo             NOT: exclude matches                                      │
│  (foo | bar) baz  Grouping                                                  │
│                                                                              │
│  FIELD FILTERS                                                              │
│  ─────────────                                                              │
│  ext:rs           Extension filter                                          │
│  lang:python      Language filter                                           │
│  file:main        Filename contains                                         │
│  path:src/*.rs    Path glob pattern                                         │
│  size:>1000       File size (bytes)                                         │
│  mtime:>2024-01   Modified after date                                       │
│  line:100-200     Line range                                                │
│                                                                              │
│  ADVANCED                                                                   │
│  ────────                                                                   │
│  near:foo,bar,5   Proximity: terms within 5 lines                           │
│  term^2.0         Boost: multiply term score by 2                           │
│  case:foo         Case-sensitive match                                      │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Appendix B: File Type Detection

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    SUPPORTED LANGUAGES (31 total)                            │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  Language      Extensions            Language       Extensions              │
│  ────────      ──────────            ────────       ──────────              │
│  Rust          .rs                   TypeScript     .ts, .tsx               │
│  Python        .py, .pyi             JavaScript     .js, .jsx, .mjs         │
│  Go            .go                   C              .c, .h                  │
│  Java          .java                 C++            .cpp, .cc, .cxx, .hpp   │
│  Ruby          .rb                   C#             .cs                     │
│  PHP           .php                  Swift          .swift                  │
│  Kotlin        .kt, .kts             Scala          .scala                  │
│  Haskell       .hs                   Elixir         .ex, .exs               │
│  Erlang        .erl                  Clojure        .clj, .cljs             │
│  Lua           .lua                  Perl           .pl, .pm                │
│  Shell         .sh, .bash            PowerShell     .ps1                    │
│  SQL           .sql                  HTML           .html, .htm             │
│  CSS           .css, .scss, .less    JSON           .json                   │
│  YAML          .yml, .yaml           TOML           .toml                   │
│  XML           .xml                  Markdown       .md, .markdown          │
│  Protobuf      .proto                                                       │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

*Document generated: 2026-02-04*
*fxi version: Latest (commit 9a9e3a9)*
