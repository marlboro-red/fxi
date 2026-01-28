# fxi

A terminal-first, ultra-fast code search engine built in Rust.

## Features

- **100-400x faster than ripgrep** on large codebases (verified on Chromium)
- **Ripgrep-like CLI**: Familiar flags (`-i`, `-A`, `-B`, `-C`, `-l`, `-c`)
- **Persistent daemon**: Keeps indexes warm for instant searches
- **Hybrid indexing**: Trigram + token index for fast narrowing
- **Rich query syntax**: Boolean operators, proximity search, field filters, regex
- **Interactive TUI**: Real-time search with vim-style keybindings
- **Instant preview**: File preview with matched line highlighting
- **Respects .gitignore**: Automatic filtering of ignored files
- **Centralized indexes**: Stored in app data, not in project directories
- **Auto-detection**: Finds codebase root from any subdirectory

## Installation

```bash
cargo build --release
```

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

When the daemon is running (`fxi daemon start`), searches complete in **20-55ms** even on massive codebases like Chromium (439k files). Without the daemon, searches take ~1.5s for cold index loading.

```bash
# Start daemon for instant searches
fxi daemon start

# Now searches are 100-400x faster than ripgrep
fxi "class Browser"  # ~50ms vs ripgrep's ~8 seconds
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
fxi daemon stop            # Stop the daemon
fxi daemon status          # Check daemon status and stats
fxi daemon reload [path]   # Reload index for a path
```

The daemon keeps indexes loaded in memory. When running, searches are **800x faster** on large codebases. Searches automatically use the daemon if available, falling back to direct index loading if not.

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
    │           └── linemap.bin
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
| Warm query | <50ms | **20-55ms** (Chromium) |
| Cold startup | <2s | ~1.5s |
| Full build (1M files) | <5 min | - |
| Delta update (100 files) | <1s | - |
| RAM usage | <500MB | - |

## Benchmarks

All benchmarks run on Apple M2 Max.

### Indexing Performance

#### Linux Kernel

| Metric | Value |
|--------|-------|
| Files discovered | 92,041 |
| Files indexed | 91,995 |
| Total time | 18.5 seconds |
| CPU utilization | 247% |
| Throughput | ~4,970 files/sec |

#### Chromium

| Metric | Value |
|--------|-------|
| Files discovered | 480,647 |
| Files indexed | 439,380 |
| Total time | 2 min 40 sec |
| CPU utilization | 151% |
| Throughput | ~2,740 files/sec |

## License

MIT
