# fxi

A terminal-first, ultra-fast code search engine built in Rust.

## Features

- **Up to ~400x faster than ripgrep** on selective queries against large codebases (verified on Linux kernel and Chromium)
- **Ripgrep-like CLI**: Familiar flags (`-i`, `-A`, `-B`, `-C`, `-l`, `-c`)
- **Persistent daemon**: Keeps indexes warm for instant searches
- **Hybrid indexing**: Trigram + token index for fast narrowing
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
fxi "pattern"              # Search for pattern
fxi "fn main"              # Search for literal text
fxi "class Foo"            # AND search: files containing both "class" and "Foo"
fxi '"exact phrase"'       # Phrase search: exact string match
```

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

When the daemon is running (`fxi daemon start`), selective searches complete in **tens of milliseconds** even on massive codebases like Chromium (449k files), and repeated queries are served from the daemon's result cache in single-digit milliseconds. Without the daemon, add ~50ms-1s for cold index loading.

```bash
# Start daemon for instant searches
fxi daemon start

# Now searches are up to ~400x faster than ripgrep
fxi '"class Browser"'  # ~111ms vs ripgrep's ~9.9 seconds on Chromium
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

The daemon keeps indexes loaded in memory. When running, searches are **up to ~400x faster** on large codebases. Searches automatically use the daemon if available, falling back to direct index loading if not.

#### File Watching

With `--watch`, the daemon monitors indexed directories for file changes and automatically updates indexes. Changes are debounced to handle rapid edits (e.g., IDE auto-save, git operations). The watcher respects `.gitignore` rules and skips common non-source directories (`node_modules`, `target`, `.git`, etc.).

When a watcher starts for a root, the daemon first reconciles the index with one incremental scan, so changes made while the daemon was down are picked up. While a root is watched, `fxi index` skips its own tree walk — the daemon owns freshness — and reports any pending debounced changes instead. `fxi index --force` still rebuilds locally.

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

## Performance Targets

| Operation | Target | Achieved |
|-----------|--------|----------|
| Warm query (selective) | <50ms | **8-110ms** novel query, **4-26ms** repeated (Chromium) |
| Cold startup | <2s | ~50ms-1s (query-dependent) |
| Full build (1M files) | <5 min | ~31s for 449k files (extrapolates to ~70s) |
| Delta update (100 files) | <1s | 0.4s (Linux, 93k files); 3.2s (Chromium, 449k files — scan-bound) |
| RAM usage | <500MB | 2.6-2.9GB peak during indexing (1.6GB with `--chunk-size 2000`) |

## Benchmarks

Measured 2026-06-12 on Apple M2 Max (12 cores, 64GB) against ripgrep 15.1.0.

**Methodology:** fxi daemon running with the index loaded. The **fxi** column is novel-query latency: the daemon's query-result cache is cleared (`fxi daemon reload`) before each timed run, mean of 3 runs. The **fxi repeated** column is the same query again, served from the daemon result cache. **rg** is the mean of 3 runs with warm OS file cache. Matching-file counts from both tools are shown for validation; small deltas (<0.3%) come from fxi skipping symlinked files and differences in binary/encoding detection.

### Searching — Linux Kernel (93,407 files, 1.5GB source)

| Query | fxi | fxi repeated | rg | Speedup (novel) | fxi files | rg files |
|-------|-----|--------------|-----|------------------|-----------|----------|
| `"static void"` | 1251ms | 197ms | 3211ms | **2.6x** | 24,217 | 24,273 |
| `"unsigned long"` | 1053ms | 164ms | 3385ms | **3.2x** | 20,642 | 20,695 |
| `"struct file_operations"` | 58ms | 8ms | 3378ms | **58x** | 1,259 | 1,260 |
| `"unlikely(!page)"` | 11ms | 5ms | 3386ms | **313x** | 52 | 52 |
| `-i deadlock` | 57ms | 8ms | 3277ms | **58x** | 986 | 1,005 |
| `re:/spin_lock_irqsave\(&\w+/` | 165ms | 21ms | 3375ms | **20x** | 3,207 | 3,217 |
| `-l "kmalloc"` | 74ms | 9ms | 3506ms | **47x** | 3,651 | 3,671 |
| `-C 3 "module_init("` | 197ms | 14ms | 3500ms | **18x** | 3,134 | 3,155 |
| `file:*.dts` | 22ms | 7ms | 88ms | **4.0x** | 3,580 | 3,580 |
| `static void init` (all-of-file AND) | 800ms | — | n/a | — | 29,573 | n/a |

### Searching — Chromium (449,092 files, 6.7GB source)

| Query | fxi | fxi repeated | rg | Speedup (novel) | fxi files | rg files |
|-------|-----|--------------|-----|------------------|-----------|----------|
| `"class Browser"` | 113ms | 9ms | 8840ms | **78x** | 2,964 | 2,970 |
| `"void OnError"` | 24ms | 5ms | 8917ms | **373x** | 466 | 466 |
| `"namespace content"` | 357ms | 24ms | 8952ms | **25x** | 9,329 | 9,331 |
| `"std::unique_ptr"` | 2215ms | 215ms | 9377ms | **4.2x** | 43,918 | 43,918 |
| `-i deprecated` | 427ms | 60ms | 9757ms | **23x** | 7,119 | 7,149 |
| `re:/scoped_refptr<\w+>/` | 632ms | 43ms | 9011ms | **14x** | 7,895 | 7,895 |
| `-l "WeakPtr"` | 428ms | 27ms | 9248ms | **22x** | 18,407 | 18,436 |
| `-C 3 "RunUntilIdle()"` | 318ms | 94ms | 9033ms | **28x** | 3,677 | 3,677 |
| `file:*.mojom` | 75ms | 6ms | 898ms | **12x** | 1,880 | 1,880 |

Note on `-i`: case-insensitive queries narrow through the lowercased token index, so they can miss mixed-case occurrences that only appear as substrings spanning token boundaries (the file counts above show the gap vs ripgrep: ~2% on these queries).

### Indexing

| Metric | Linux Kernel | Chromium |
|--------|--------------|----------|
| Files indexed | 93,407 | 449,092 |
| Full build | 8.6s | 27.7s |
| Throughput | ~10,820 files/sec | ~16,240 files/sec |
| Incremental update (50 changed files) | 0.44s | 3.2s |
| No-op scan (nothing changed) | 0.6s | 2.9s |
| Peak RSS during build | 2.6GB | 2.9GB |

Peak RSS scales with `--chunk-size` (default 5000 files per segment): `--chunk-size 2000` builds the kernel index in 1.6GB at a ~3% time cost, at the price of more segments to consult per query.

Incremental updates write delta segments for changed files only; the change-detection scan walks the tree with parallel walker threads.

## License

MIT
