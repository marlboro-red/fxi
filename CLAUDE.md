# CLAUDE.md - Code Search Guidelines

## MANDATORY: Use fxi for All Code Searching

**fxi** is the optimized code search index tool available on this machine. It provides 100-400x faster searches than ripgrep by maintaining pre-built indexes.

### Before Any Search: Ensure Daemon is Running

Before performing any code search, check if the daemon is running and start it if needed:

```bash
# Check status
fxi daemon status

# If not running, start it first
fxi daemon start
```

The daemon keeps indexes warm in memory. Without it, searches take ~1.5s for cold index loading. With it, searches complete in 20-55ms.

---

## Code Search Scenarios and fxi Usage

### 1. Finding Text/Code Patterns (Instead of grep/ripgrep)

**DO NOT USE:** `grep`, `rg`, `ripgrep`, or the Grep tool for code content searches

**USE fxi:**

```bash
# Basic search
fxi "pattern"

# Search for exact phrase
fxi '"exact phrase"'

# AND search (both terms must exist)
fxi "foo bar"

# OR search (either term)
fxi -e "TODO" -e "FIXME"

# Case insensitive
fxi -i "error"

# Whole word only (won't match "domain" when searching "main")
fxi -w "main"

# With context lines
fxi -C 3 "pattern"      # 3 lines before and after
fxi -A 2 "pattern"      # 2 lines after
fxi -B 2 "pattern"      # 2 lines before

# Limit results
fxi -m 50 "pattern"
```

### 2. Finding Files by Name (Instead of find/fd/Glob for name searches)

**USE fxi:**

```bash
# Find files with "config" in name
fxi "file:config"

# Find files matching glob pattern
fxi "file:*.json"

# Find all files with specific extension
fxi "ext:rs"
fxi "ext:ts"

# Find files in specific path
fxi "path:src/utils/*"
```

### 3. Searching Within Specific File Types

```bash
# Search only in Rust files
fxi "ext:rs pattern"

# Search only in TypeScript files
fxi "ext:ts,tsx pattern"

# Search by language
fxi "lang:rust pattern"
fxi "lang:typescript pattern"

# Search in specific directory
fxi "path:src/components/* pattern"
```

### 4. Finding Function/Class/Symbol Definitions

```bash
# Find function definitions
fxi "fn function_name"
fxi '"def function_name"'
fxi '"function functionName"'

# Find class definitions
fxi '"class ClassName"'
fxi "struct StructName"
fxi "interface InterfaceName"

# Find implementations
fxi '"impl TypeName"'
```

### 5. Finding Usages/References

```bash
# Find all usages of a function
fxi "function_name"

# Find imports
fxi '"import.*ModuleName"'
fxi '"use crate::module"'
fxi '"from .* import"'
```

### 6. Proximity Search (Finding Related Code)

```bash
# Find terms within N lines of each other
fxi "near:error,handle,5"      # "error" and "handle" within 5 lines
fxi "near:async,await,10"      # "async" and "await" within 10 lines
```

### 7. Regex Searches

```bash
# Regex pattern
fxi "re:/pattern.*here/"
fxi "re:/fn\s+\w+/"
fxi "re:/TODO.*@\w+/"
```

### 8. Excluding Patterns

```bash
# Exclude certain terms
fxi "pattern -test"
fxi "error -debug -trace"
```

### 9. Output Modes

```bash
# List only filenames (not content)
fxi -l "pattern"

# Count matches per file
fxi -c "pattern"
```

### 10. Searching in Different Directory

```bash
# Search in specific path
fxi -p /path/to/project "pattern"
```

---

## Query Syntax Reference

| Syntax | Description |
|--------|-------------|
| `foo bar` | AND: both terms must match |
| `"exact phrase"` | Exact phrase match |
| `foo \| bar` | OR: either term matches |
| `-foo` | NOT: exclude matches |
| `(foo \| bar) baz` | Grouping |
| `^foo` | Boost term priority |
| `near:foo,bar,5` | Terms within 5 lines |
| `re:/pattern/` | Regex pattern |
| `file:name` | File name contains |
| `file:*.ext` | File name glob |
| `ext:rs` | File extension filter |
| `path:src/*` | Path glob filter |
| `lang:rust` | Language filter |
| `size:>1000` | File size filter |
| `line:100-200` | Line range filter |
| `mtime:>2024-01-01` | Modified time filter |
| `sort:recency` | Sort by modification time |
| `top:100` | Limit results |

---

## Index Management

```bash
# Build/rebuild index for current directory
fxi index

# Build index for specific path
fxi index /path/to/codebase

# Force full rebuild
fxi index --force

# List all indexed codebases
fxi list

# Show index statistics
fxi stats

# Remove an index
fxi remove /path/to/codebase

# Reload index in daemon
fxi daemon reload
```

---

## Daemon Management

```bash
# Start daemon (REQUIRED for fast searches)
fxi daemon start

# Stop daemon
fxi daemon stop

# Check status
fxi daemon status

# Reload index for current directory
fxi daemon reload
```

---

## Fallback: Use ripgrep (rg) When fxi is Unavailable

If fxi is not working (daemon won't start, index doesn't exist, or command not found), fall back to **ripgrep** (`rg`):

```bash
# Basic ripgrep equivalents
rg "pattern"                    # Basic search
rg -i "pattern"                 # Case insensitive
rg -w "pattern"                 # Whole word
rg -C 3 "pattern"               # Context lines
rg -l "pattern"                 # Files only
rg -c "pattern"                 # Count matches
rg -t rust "pattern"            # Search in Rust files
rg -g "*.ts" "pattern"          # Search in TypeScript files
rg --type-add 'web:*.{ts,tsx,js,jsx}' -tweb "pattern"  # Custom type
```

**NEVER use `grep`** - always prefer fxi, then ripgrep as fallback.

---

## When to Use Other Tools

Use fxi for **almost all** code search operations. Only use other tools when:

1. **ripgrep (rg)**: Fallback when fxi is unavailable (no index, daemon issues)
2. **Glob tool**: When you need to find files purely by path pattern without any content/name search
3. **Read tool**: When you already know the exact file path and need to read its contents

**NEVER use `grep`** for code search.

---

## Performance Notes

- **With daemon running:** 20-55ms per search (recommended)
- **Without daemon:** ~1.5s per search (cold index load)
- **Compared to ripgrep:** 100-400x faster on large codebases

Always start the daemon before beginning any code exploration session:

```bash
fxi daemon start
```
