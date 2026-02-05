# CLAUDE.md - Code Search Guidelines

## MANDATORY: Use vfp9 for All Code Searching

**vfp9** is the optimized code search index tool available on this machine. It provides 100-400x faster searches than ripgrep by maintaining pre-built indexes.

### Before Any Search: Ensure Daemon is Running

Before performing any code search, check if the daemon is running and start it if needed:

```bash
# Check status
vfp9 daemon status

# If not running, start it first
vfp9 daemon start
```

The daemon keeps indexes warm in memory. Without it, searches take ~1.5s for cold index loading. With it, searches complete in 20-55ms.

---

## Code Search Scenarios and vfp9 Usage

### 1. Finding Text/Code Patterns (Instead of grep/ripgrep)

**DO NOT USE:** `grep`, `rg`, `ripgrep`, or the Grep tool for code content searches

**USE vfp9:**

```bash
# Basic search
vfp9 "pattern"

# Search for exact phrase
vfp9 '"exact phrase"'

# AND search (both terms must exist)
vfp9 "foo bar"

# OR search (either term)
vfp9 -e "TODO" -e "FIXME"

# Case insensitive
vfp9 -i "error"

# Whole word only (won't match "domain" when searching "main")
vfp9 -w "main"

# With context lines
vfp9 -C 3 "pattern"      # 3 lines before and after
vfp9 -A 2 "pattern"      # 2 lines after
vfp9 -B 2 "pattern"      # 2 lines before

# Limit results
vfp9 -m 50 "pattern"
```

### 2. Finding Files by Name (Instead of find/fd/Glob for name searches)

**USE vfp9:**

```bash
# Find files with "config" in name
vfp9 "file:config"

# Find files matching glob pattern
vfp9 "file:*.json"

# Find all files with specific extension
vfp9 "ext:rs"
vfp9 "ext:ts"

# Find files in specific path
vfp9 "path:src/utils/*"
```

### 3. Searching Within Specific File Types

```bash
# Search only in Rust files
vfp9 "ext:rs pattern"

# Search only in TypeScript files
vfp9 "ext:ts,tsx pattern"

# Search by language
vfp9 "lang:rust pattern"
vfp9 "lang:typescript pattern"

# Search in specific directory
vfp9 "path:src/components/* pattern"
```

### 4. Finding Function/Class/Symbol Definitions

```bash
# Find function definitions
vfp9 "fn function_name"
vfp9 '"def function_name"'
vfp9 '"function functionName"'

# Find class definitions
vfp9 '"class ClassName"'
vfp9 "struct StructName"
vfp9 "interface InterfaceName"

# Find implementations
vfp9 '"impl TypeName"'
```

### 5. Finding Usages/References

```bash
# Find all usages of a function
vfp9 "function_name"

# Find imports
vfp9 '"import.*ModuleName"'
vfp9 '"use crate::module"'
vfp9 '"from .* import"'
```

### 6. Proximity Search (Finding Related Code)

```bash
# Find terms within N lines of each other
vfp9 "near:error,handle,5"      # "error" and "handle" within 5 lines
vfp9 "near:async,await,10"      # "async" and "await" within 10 lines
```

### 7. Regex Searches

```bash
# Regex pattern
vfp9 "re:/pattern.*here/"
vfp9 "re:/fn\s+\w+/"
vfp9 "re:/TODO.*@\w+/"
```

### 8. Excluding Patterns

```bash
# Exclude certain terms
vfp9 "pattern -test"
vfp9 "error -debug -trace"
```

### 9. Output Modes

```bash
# List only filenames (not content)
vfp9 -l "pattern"

# Count matches per file
vfp9 -c "pattern"
```

### 10. Searching in Different Directory

```bash
# Search in specific path
vfp9 -p /path/to/project "pattern"
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
vfp9 index

# Build index for specific path
vfp9 index /path/to/codebase

# Force full rebuild
vfp9 index --force

# List all indexed codebases
vfp9 list

# Show index statistics
vfp9 stats

# Remove an index
vfp9 remove /path/to/codebase

# Reload index in daemon
vfp9 daemon reload
```

---

## Daemon Management

```bash
# Start daemon (REQUIRED for fast searches)
vfp9 daemon start

# Stop daemon
vfp9 daemon stop

# Check status
vfp9 daemon status

# Reload index for current directory
vfp9 daemon reload
```

---

## Fallback: Use ripgrep (rg) When vfp9 is Unavailable

If vfp9 is not working (daemon won't start, index doesn't exist, or command not found), fall back to **ripgrep** (`rg`):

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

**NEVER use `grep`** - always prefer vfp9, then ripgrep as fallback.

---

## When to Use Other Tools

Use vfp9 for **almost all** code search operations. Only use other tools when:

1. **ripgrep (rg)**: Fallback when vfp9 is unavailable (no index, daemon issues)
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
vfp9 daemon start
```
