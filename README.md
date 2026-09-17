# fxi

A terminal-first, ultra-fast code search engine built in Rust.

## Features

- **Indexed code search** with conservative regex planning and verified candidate matches
- **Ripgrep-like CLI**: Familiar flags (`-i`, `-A`, `-B`, `-C`, `-l`, `-c`)
- **Persistent daemon**: Keeps indexes warm for instant searches
- **Regex-aware indexing**: Conservative trigram plans, with token and position data available for structured queries
- **Rich query syntax**: Boolean operators, proximity search, field filters, regex
- **Interactive TUI**: Real-time search with vim-style keybindings
- **Instant preview**: File preview with matched line highlighting
- **File watching**: Daemon auto-updates indexes when files change (`--watch`)
- **Incremental updates**: Delta segments for efficient index maintenance
- **Cross-platform**: Unix sockets (Linux/macOS) and Windows named pipes
- **Respects .gitignore**: Automatic filtering of ignored files
- **Skips symlinks**: Like ripgrep, only real files are indexed (no duplicate results from links)
- **Centralized indexes**: Stored in app data, not in project directories
- **Auto-detection**: Finds codebase root from any subdirectory

## Installation

```bash
cargo build --release
```

## VS Code Extension

A VS Code extension is available in the `vscode-extension/` directory.

### Building and Installing

```bash
cd vscode-extension
npm install
npm run build
npx @vscode/vsce package
code --install-extension fxi-0.1.0.vsix
```

### Features

- Sidebar search panel with real-time results
- Click to open files at matching lines
- Context lines and files-only mode
- Daemon status indicator in the status bar
- Keyboard shortcut: `Ctrl+Shift+I` (macOS: `Cmd+Shift+I`)

### Commands

All accessible via the Command Palette (`Ctrl+Shift+P`):

| Command | Description |
|---------|-------------|
| `FXI: Search` | Focus the search panel |
| `FXI: Build Index` | Build index for the workspace |
| `FXI: Reload Index` | Reload index from disk |
| `FXI: Start Daemon` | Start the fxi daemon |
| `FXI: Stop Daemon` | Stop the fxi daemon |
| `FXI: Daemon Status` | Show daemon status |

### Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `fxi.binaryPath` | `"fxi"` | Path to the fxi executable |
| `fxi.defaultLimit` | `200` | Maximum search results (0 = unlimited) |
| `fxi.defaultContextLines` | `2` | Context lines shown with results |

## Usage

### Build Index

```bash
fxi index                  # Index current directory (auto-detects git root)
fxi index [path]           # Index a specific directory
fxi index --force [path]   # Force full rebuild
```

### Search (ripgrep-like)

Direct content search with ripgrep-compatible output. Automatically uses the daemon for instant results when available, otherwise falls back to loading the index from disk.

```bash
fxi 'pattern'              # One term: case-insensitive literal search
fxi 'fn main'              # AND: files containing both "fn" and "main"
fxi 'class Foo'            # AND: files containing both "class" and "Foo"
fxi '"fn main"'            # Phrase: exact, case-sensitive text "fn main"
```

Shell quotes group an argument; the shell removes them before FXI sees it.
Consequently, `fxi "fn main"` and `fxi 'fn main'` are identical: the two terms
can occur separately, in either order, anywhere in the same file. The same rule
applies to `class Foo`; FXI does not interpret programming-language syntax here.
To require adjacent text, preserve double quotes inside the argument, as in
`fxi '"fn main"'`. Bare terms ignore case; quoted phrases are case-sensitive
unless you add `-i`.

#### CLI Flags

Flags match ripgrep conventions for familiarity.

| Flag | Long | Description |
|------|------|-------------|
| `-e PAT` | `--regexp` | Pattern to search (can be repeated for OR) |
| `-i` | `--ignore-case` | Case insensitive search |
| `-w` | `--word-regexp` | Match whole words only |
| `-A NUM` | `--after-context` | Show NUM lines after each match |
| `-B NUM` | `--before-context` | Show NUM lines before each match |
| `-C NUM` | `--context` | Show NUM lines before and after (overrides -A/-B) |
| `-l` | `--files-with-matches` | Only print filenames, not matching lines |
| `-c` | `--count` | Print match count per file |
| `-m NUM` | `--max-count` | Limit to NUM results (default: unlimited) |
| `-p PATH` | `--path` | Search in specific directory |
| | `--color=WHEN` | When to use colors: `always`, `never`, `auto` (default: auto) |

**Differences from ripgrep:**

- `-v` (invert match) is not supported (indexed search only returns matching lines)
- Token search is case-insensitive by default for better code search recall

#### Examples

```bash
# Basic searches
fxi "TODO"                 # Find all TODOs
fxi "fn main"              # Find main functions (AND: both terms)
fxi '"fn main"'            # Find exact phrase "fn main"

# Case insensitive
fxi -i "error"             # Match "error", "Error", "ERROR", etc.

# Word boundary
fxi -w "main"              # Match "main" but not "domain" or "mainly"

# Multiple patterns (OR)
fxi -e "TODO" -e "FIXME"   # Find lines with TODO or FIXME
fxi -e "error" -e "warn"   # Find error or warning messages

# Context lines
fxi -A 2 "panic"           # Show 2 lines after each match
fxi -B 2 "panic"           # Show 2 lines before each match
fxi -C 3 "panic"           # Show 3 lines before and after
fxi -A 2 -B 1 "panic"      # 1 line before, 2 lines after

# Output modes
fxi -l "struct"            # List only filenames with matches
fxi -c "impl"              # Count matches per file

# Limit results
fxi -m 10 "use std"        # Show only first 10 matches
fxi -m 1000 "TODO"         # Increase limit for thorough search

# Search different directory
fxi -p ../other-project "pattern"

# Combine flags
fxi -i -C 2 -m 50 "fixme"  # Case insensitive, with context, limited
```

#### Output Format

Results are displayed in ripgrep-style format with colors:

```
src/main.rs
42:    let query = pattern.to_string();
43-    // context line after
--
src/server/daemon.rs
128:    fn handle_search(&self, query: String) {
```

- **Filename**: magenta (printed once per file as heading)
- **Line number**: green (`:` for match, `-` for context)
- **Match text**: red/bold highlighting
- **Separator**: `--` between non-contiguous matches

#### Performance

The daemon keeps immutable index readers loaded and reuses bounded, metadata-validated content snapshots. Every query is verified; full-result memoization is disabled. Latency depends on candidate volume, file sizes, and output mode. See the reproducible benchmarks below.

```bash
# Keep indexes loaded for repeated searches
fxi daemon start

# Searches now reuse the loaded index
fxi '"class Browser"'  # Search an exact phrase
```

### Interactive TUI

```bash
fxi                        # Launch interactive TUI
fxi search                 # Same as above
fxi search [path]          # TUI for specific directory
```

### Daemon (for instant searches)

```bash
fxi daemon start           # Start daemon in background
fxi daemon start --watch   # Start with file watching (auto-updates indexes)
fxi daemon stop            # Stop the daemon
fxi daemon status          # Check daemon status and stats
fxi daemon reload [path]   # Reload index for a path
fxi daemon foreground      # Run in foreground (for debugging)
fxi daemon foreground --watch  # Foreground with file watching
```

The daemon keeps indexes loaded in memory. Searches automatically use it when available, falling back to direct index loading otherwise.

#### File Watching

With `--watch`, the daemon monitors indexed directories for file changes and automatically updates indexes. Changes are debounced to handle rapid edits (e.g., IDE auto-save, git operations). The watcher respects `.gitignore` rules and skips common non-source directories (`node_modules`, `target`, `.git`, etc.).

Small updates become searchable through an immutable in-memory index before
disk publication. The default quiet debounce is 1 ms, with a 100 ms maximum
event age; these are scheduling windows, not guaranteed end-to-end latencies.
Disk writes are grouped until 250 ms of quiet or ten seconds of continuous
updates. Large batches use the durable update path. See
[freshness semantics](docs/SEMANTICS.md#freshness) for bounds and recovery behavior.

When a watcher starts for a root, the daemon first reconciles the index with one incremental scan, so changes made while the daemon was down are picked up. While a root is watched, `fxi index` skips its own tree walk — the daemon owns freshness — and reports pending updates instead. Those changes may already be searchable in memory while awaiting disk publication. `fxi index --force` still rebuilds locally.

### Manage Indexes

```bash
fxi list                   # List all indexed codebases
fxi stats [path]           # Show index statistics
fxi remove <path>          # Remove index for a codebase
fxi compact [path]         # Compact delta segments
```

## Index Storage

Indexes are stored centrally in your app data directory (not in project folders):

| Platform | Location |
|----------|----------|
| Linux | `~/.local/share/fxi/indexes/` |
| macOS | `~/Library/Application Support/fxi/indexes/` |
| Windows | `%LOCALAPPDATA%/fxi/indexes/` |

Each codebase gets a unique folder based on a hash of its root path:

```
~/.local/share/fxi/
└── indexes/
    ├── myproject-a1b2c3d4e5f6g7h8/
    │   ├── meta.json
    │   ├── docs.bin
    │   ├── paths.bin
    │   └── segments/
    │       └── seg_0001/
    │           ├── grams.dict
    │           ├── grams.postings
    │           ├── tokens.dict
    │           ├── tokens.postings
    │           └── bloom.bin
    └── another-repo-i9j0k1l2m3n4o5p6/
        └── ...
```

### Subdirectory Support

fxi automatically detects your codebase root by looking for a `.git` directory:

```bash
$ cd ~/projects/myapp/src/components/Button
$ fxi stats
Root path:      /home/user/projects/myapp    # Auto-detected!
Index location: ~/.local/share/fxi/indexes/myapp-...
Document count: 1234
```

## Search Semantics

The full contract for what fxi matches, what it can miss, and how fresh
results are — including every documented divergence from ripgrep — is in
[docs/SEMANTICS.md](docs/SEMANTICS.md).

## Query Syntax

### Literals and Phrases

```
foo bar                    # AND: both terms must match
"exact phrase"             # Exact phrase match
^foo                       # Boosted term (default 2x priority)
^3:foo                     # Boosted term with custom weight
```

Searches automatically match both file content AND filenames - typing `config` will find files containing "config" as well as files named `config.json`, `config.rs`, etc.

### Boolean Operators

```
foo | bar                  # OR: either term matches
-foo                       # NOT: exclude matches
(foo | bar) baz            # Grouping
```

### Proximity Search

```
near:foo,bar,5             # Terms within 5 lines of each other
near:foo,bar,abc           # Default distance (10 lines) if not numeric
```

### Regex

```
re:/foo.*bar/              # Regex pattern
```

### File Search

Find files by name without content matching:

```
file:config                # Files with "config" in the name
file:*.json                # Files matching glob pattern
ext:rs                     # All .rs files
path:src/utils/*           # All files in src/utils/
```

### Field Filters

Combine filters with a search term:

```
ext:rs foo                 # Search "foo" in .rs files only
path:src/*.rs bar          # Search "bar" in files matching glob
lang:rust baz              # Search "baz" in Rust files
size:>1000 test            # Search in files larger than 1KB
size:<10000 test           # Search in files smaller than 10KB
line:100-200 TODO          # Search within line range
mtime:>2024-01-01 fix      # Search in recently modified files
```

### Options

```
sort:recency               # Sort by modification time
sort:path                  # Sort by path
top:100                    # Limit results
```

## TUI Keybindings

Press `F1` or `?` to show help in the TUI.

### Search Mode

| Key | Action |
|-----|--------|
| `↑/↓` or `Tab/Shift+Tab` | Navigate results |
| `Ctrl+d` / `Ctrl+u` | Page down / up |
| `gg` or `Ctrl+a` | First result |
| `G` or `Ctrl+e` | Last result |
| `Enter` | Execute search / Open file |
| `Ctrl+p` | Toggle preview mode |
| `Ctrl+w` | Delete word |
| `F5` | Rebuild index |
| `Esc` | Clear query / Exit |
| `Ctrl+c` | Exit |

### Preview Mode

| Key | Action |
|-----|--------|
| `j/k` | Scroll down / up |
| `Ctrl+d` / `Ctrl+u` | Half-page down / up |
| `Ctrl+f` / `Ctrl+b` | Full page down / up |
| `gg` / `G` | Top / Bottom |
| `n` / `N` | Next / Previous result |
| `o` or `Enter` | Open file in editor |
| `q` or `Esc` | Back to search |

## Architecture

```
+------------------+
|      TUI         |
+---------+--------+
          |
+---------v--------+
|   Query Engine   |
|  - Parser        |
|  - Planner       |
|  - Executor      |
+---------+--------+
          |
+---------v--------+
|   Index Reader   |
|  (mmap segments) |
+---------+--------+
          |
+---------v--------+
|  On-Disk Index   |
|  (app data dir)  |
+------------------+
```

## Performance and validation

Current measurements, correctness fixes, research experiments, and remaining gaps
are documented in [the live-update experiments](docs/performance-round7/NOTES.md),
[the source-pack comparisons](docs/performance-round6/NOTES.md),
[the source verification and position experiments](docs/performance-round5/NOTES.md),
[the broader comparisons and cache experiments](docs/performance-round4/NOTES.md),
[the watcher and metadata experiments](docs/performance-round3/NOTES.md),
[the indexing and query experiments](docs/performance-round2/NOTES.md), and
[the first engineering report](docs/audit-2026-09-17/PROGRESS.md).
The [benchmark harness](docs/audit-2026-09-17/benchmark.py) compares full matching
file sets against ripgrep on every run, with pinned Redis, CPython, and Linux corpora,
interleaved samples, and separate direct/server measurements.

Earlier Linux/Chromium “up to 400x” claims and million-file extrapolations are
not accepted as current evidence. The [audit](docs/audit-2026-09-17/AUDIT.md)
explains the scope, cache, and correctness problems in the old methodology.

Search worker concurrency can be explored with `FXI_SEARCH_PARALLELISM` (positive
integer). The default bounds source-file read tasks independently of index build
workers. Long-lived readers share one process-wide content cache, retaining at
most 1 GiB of text and 131,072 entries across all roots. Storage is allocated on
demand, and metadata is checked before reuse. Set `FXI_CACHE_MIB` before starting
the daemon to change the text budget (0 disables caching; valid range 0–4096).
One-shot CLI searches bypass content caching. These limits cover retained cache
contents, not index construction, active results, or total process RSS.

## License

MIT

For the optional Unix source-pack experiment, build with
`FXI_SOURCE_PACK=1 fxi index --force PATH`. It trades extra disk space and build
work for faster broad one-shot files-only searches. See the
[round-six measurements and limitations](docs/performance-round6/NOTES.md) and
[source-pack semantics](docs/SEMANTICS.md#optional-source-packs).
