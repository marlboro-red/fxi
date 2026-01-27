# fxi

A terminal-first, ultra-fast code search engine built in Rust.

## Features

- **Hybrid indexing**: Trigram + token index for fast narrowing
- **Sub-50ms query latency** on warm searches
- **Memory-efficient**: Disk-backed structures with mmap
- **Rich query syntax**: Boolean operators, field filters, regex
- **Interactive TUI**: Real-time search with preview
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

### Search

```bash
fxi                        # Interactive search (works from any subdirectory)
fxi [query]                # Interactive search with initial query
fxi search [query]         # Same as above
```

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
```

### Boolean Operators
```
foo | bar                  # OR: either term matches
-foo                       # NOT: exclude matches
(foo | bar) baz            # Grouping
```

### Regex
```
re:/foo.*bar/              # Regex pattern
```

### Field Filters
```
path:src/*.rs              # Path glob
ext:rs                     # File extension
lang:rust                  # Language filter
size:>1000                 # Minimum file size
size:<10000                # Maximum file size
```

### Options
```
sort:recency               # Sort by modification time
sort:path                  # Sort by path
top:100                    # Limit results
```

## TUI Keybindings

| Key | Action |
|-----|--------|
| `↑/↓` | Navigate results |
| `Enter` | Open in editor |
| `Ctrl+P` | Toggle preview |
| `F5` | Rebuild index |
| `Esc` | Clear query / Exit |
| `Ctrl+C` | Exit |

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

| Operation | Target |
|-----------|--------|
| Cold startup | <300ms |
| Warm query | <50ms |
| Regex (narrowed) | <200ms |
| Full build (1M files) | <5 min |
| Delta update (100 files) | <1s |
| RAM usage | <500MB |

## License

MIT
