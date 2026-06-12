# fxi search semantics

This document is the contract for what fxi matches, what it can miss, and how
fresh its results are. Where behavior differs from ripgrep, the difference is
listed explicitly. Claims marked with a test name are enforced by
`tests/parity_grid.rs` or the named unit test.

## Query model

A query string is parsed into an AST before any flag handling:

| Input | Meaning |
|-------|---------|
| `foo bar` | AND: both terms must appear somewhere in the **same file** |
| `"foo bar"` | Phrase: the exact substring `foo bar` |
| `foo \| bar` | OR |
| `-foo` | NOT: exclude files matching `foo` |
| `re:/pat/` | Regex (Rust `regex` crate syntax) |
| `-e a -e b` | OR of patterns (compiled to a regex alternation) |

**Difference from grep/ripgrep:** unquoted multi-word queries are a
*file-level* AND, not a line match. `fxi "static void"` finds files containing
both words anywhere; `fxi '"static void"'` finds the phrase.

## Matching semantics

### Bare tokens — always case-insensitive

A single-word query matches **case-insensitively as a substring**, with or
without `-i`. `fxi error` matches `error`, `Error`, and `ERROR_CODE`, and
equals `rg -i -F error` (parity: *"token equals rg -i"*, *"-i token"*).
This is deliberate: code search wants `handleError`, `HandleError`, and
`handle_error` to be one query.

Recall: candidates come from the union of
1. the token index (identifiers, lowercased, split on `_` and case
   boundaries),
2. trigram postings (byte-exact substrings),
3. when a trigram is too common to narrow (a *stop-gram*, present in more
   than half of all files): tokens *containing* the query as a substring,
   plus the intersection of the query's sub-token postings.

Known gap: a substring that spans punctuation (e.g. matching `r::st` inside
`vector::start`) whose trigrams are **all** stop-grams cannot be narrowed and
may be missed. This requires punctuation trigrams present in >50% of files —
rare, but possible in large C++ trees.

### Phrases — case-sensitive unless `-i`

`"exact phrase"` matches the exact byte sequence (parity: *"phrase, exact
case"*, *"phrase with punctuation"*). With `-i`, the phrase matches
case-insensitively (parity: *"-i phrase"*). Phrases never match across line
boundaries.

### Regex

`re:/pat/` uses Rust `regex` syntax — notably **no backreferences or
lookaround**. `-i` prepends `(?i)` (parity: *"-i regex"*). Literal-prefix
narrowing is only applied when the prefix is required by every match:
alternations disable it entirely and quantifiers exclude the optional
character (`test_extract_regex_prefix`).

### Flags

- `-w` rewrites to `\b…\b` regex semantics (parity: *"-w token"*,
  *"-w -i combination"*).
- `-i` affects phrases and regexes; it is a no-op for bare tokens, which are
  already case-insensitive (parity: *"-i mixed-case query"*).
- `-v` (invert) is **unsupported**: an index can return matching lines, not
  non-matching ones.
- `-m N` caps results after matching; `-l` and `-c` change output, not
  matching.

## Which files are searched

A file is indexed iff **all** of the following hold:

- not excluded by `.gitignore` / global gitignore / `.git/info/exclude`
- not hidden, and not under `.git`, `node_modules`, `target`, `__pycache__`,
  `.venv`, `venv`, `.codesearch`
- **not a symlink** — like ripgrep, only real files are indexed, so symlinked
  duplicates never appear in results (`test_symlinks_not_indexed`)
- not a known-binary extension (images, archives, media, wasm, etc. — see
  `is_known_binary_ext`)
- non-empty and at most 10 MB
- passes the content sniff: ≤10% NUL / non-text bytes in the first 8 KB

Consequences worth knowing:

- **UTF-16 files are not searched** (their NUL bytes fail the sniff).
  ripgrep transcodes BOM-marked UTF-16; fxi does not.
- Non-UTF-8 (but text-like) files are searchable by trigram/substring, but
  produce no identifier tokens.
- Files the indexer rejects are remembered (with mtime) in `meta.json`, so
  incremental scans skip them until they change.

## Freshness

Search results reflect **the index as of its last update**, with one
correction: every candidate file is re-read and verified at query time, so a
file whose content changed since indexing never produces stale *lines*.

The asymmetry to understand: stale matches are pruned, but **files created or
made-matching since the last index update are invisible** — narrowing cannot
surface a document the index has never seen.

How the index stays fresh:

- `fxi index` performs an incremental update (parallel tree scan, mtime
  comparison, delta segment for changes).
- A daemon started with `--watch` reconciles each root with one incremental
  scan when its watcher starts, then applies file events (debounced; flushed
  to a delta segment periodically — `FXI_DELTA_FLUSH_SECS`, default 300s).
  A newly created file is searchable only after the next flush.
- While a root is watched, `fxi index` skips its own scan and reports the
  daemon's pending-change count; `fxi index --force` rebuilds locally.

## Result caching

The daemon caches query results keyed on (pattern, options, limit). A cache
hit returns the previous result **for the same index version**; any index
update (reload, watcher flush, delta write) clears the cache. Repeated
identical queries are therefore answered in single-digit milliseconds without
a staleness penalty beyond the index's own freshness, described above.
