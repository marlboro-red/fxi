# fxi

A terminal-first, ultra-fast code search engine built in Rust.

## Features

- **Hybrid indexing**: Trigram + token index for fast narrowing
- **Sub-50ms query latency** on warm searches
- **Memory-efficient**: Disk-backed structures with mmap
- **Rich query syntax**: Boolean operators, field filters, regex
- **Interactive TUI**: Real-time search with preview
- **Respects .gitignore**: Automatic filtering of ignored files
- **Incremental updates**: Delta segment support (LSM-style)

## Installation

```bash
cargo build --release
```

## Usage

### Build Index

```bash
fxi index [path]           # Index a directory
fxi index --force [path]   # Force full rebuild
```

### Search

```bash
fxi [query]                # Interactive search
fxi search [query]         # Interactive search with initial query
```

### Other Commands

```bash
fxi stats [path]           # Show index statistics
fxi compact [path]         # Compact delta segments
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
|  (segments)      |
+------------------+
```

## Index Structure

```
.codesearch/
├── meta.json          # Index metadata
├── docs.bin           # Document table
├── paths.bin          # Path store
└── segments/
    └── seg_0001/
        ├── grams.dict      # Trigram dictionary
        ├── grams.postings  # Trigram postings (delta-encoded)
        ├── tokens.dict     # Token dictionary
        ├── tokens.postings # Token postings
        └── linemap.bin     # Line offset maps
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
