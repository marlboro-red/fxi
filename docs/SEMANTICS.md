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

Candidates use conservative trigram constraints derived from the matching
expression, including Unicode case alternatives. Short substrings and omitted
stop-grams broaden the candidate set; neither whole-token equality nor token
positions may exclude a valid substring. Punctuation-spanning substrings retain
recall even when every relevant gram is omitted. Fresh indexes retain common
grams by default; only legacy or explicitly configured omitted-gram sets require
that fallback. Compaction preserves this distinction.

### Phrases — case-sensitive unless `-i`

`"exact phrase"` matches the exact byte sequence (parity: *"phrase, exact
case"*, *"phrase with punctuation"*). With `-i`, the phrase matches
case-insensitively (parity: *"-i phrase"*). Phrases never match across line
boundaries.

### Regex

`re:/pat/` uses Rust `regex` syntax — notably **no backreferences or
lookaround**. `-i` prepends `(?i)` (parity: *"-i regex"*). HIR-based planning
can use required literals anywhere in the expression, conservatively combines
alternatives, and respects optional repetitions. Patterns are verified per line;
invalid regexes return an error even when the candidate set is empty.

### Ranked output and boosts

Ranked output scores the complete verified result set before applying `top:N`.
A boost such as `^3:needle` applies to lines containing that term; an unmatched
OR branch does not boost other lines. If multiple boosted positive terms match a
line, the largest boost applies. `^3:"exact phrase"` retains phrase case semantics.
Ranking otherwise combines file match count, filename relevance, depth, and recency;
it is a heuristic relevance model, not BM25. Files-only/content output does not use
ranked filename fallback.

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
- Only valid UTF-8 content is indexed and searched. Other encodings need
  conversion first; byte-oriented matching and automatic transcoding are not supported.
- Files the indexer rejects are remembered (with mtime) in `meta.json`, so
  incremental scans skip them until they change.

## Freshness

Search results reflect **the index as of its last update**, with one
correction: every candidate file is verified against a read or metadata-validated snapshot at query time, so a
file whose content changed since indexing never produces stale *lines*.

The asymmetry to understand: stale matches are pruned, but **files created or
made-matching since the last index update are invisible** — narrowing cannot
surface a document the index has never seen.

How the index stays fresh:

- `fxi index` performs an incremental update (parallel tree scan, mtime
  comparison, delta segment for changes).
- A daemon started with `--watch` reconciles each root with one incremental
  scan when its watcher starts, then reconciles debounced file events through
  the same ignore-aware walker as CLI indexing. Ready batches publish immediately
  by default, after the 100 ms quiet debounce (or two-second maximum event age).
  `FXI_DELTA_FLUSH_SECS` can add a publication delay; its default is 0. A newly
  created file becomes searchable after notification delivery, debounce and
  reconciliation complete; this is not an immediate live overlay. Startup
  registration, notification errors and periodic five-minute reconciliation
  also repair membership. Reconciliation currently scans file metadata; a
  sound event-only update path is a future optimization.
- While a root is watched, `fxi index` skips its own scan and reports the
  daemon's pending-change count; `fxi index --force` rebuilds locally.
- All index writers (CLI builds, daemon flushes, compaction) hold a
  per-index advisory lock, so two writers can never interleave segment or
  metadata writes. Ordinary watcher batches defer when another writer holds
  that lock, allowing other roots to progress. Failed batches remain pending
  and retry with a one-second backoff.
- Searching without a daemon prints a stderr note when the index is more
  than an hour old (`FXI_STALE_WARN_SECS`, 0 disables).

## Result caching

Whole-query results are not memoized: an index generation alone does not
prove that editable source files are unchanged. Every query verifies its
candidates against current file snapshots. Small-file content caching checks
file size, high-resolution modification/creation timestamps and, on Unix,
file identity/change timestamps before reusing an immutable copy. A concurrent
edit can race a query; results are not a transactional snapshot of the entire
filesystem.

Immutable cached snapshots can retain complete byte positions for up to two
trigrams, capped at 32 occurrences each. Proven exact line-local literals can
use these positions to verify the full literal at possible offsets instead of
rescanning all bytes. Overflow falls back to the ordinary matcher. This evidence
is shared across compatible literals and discarded with its source snapshot;
it never bypasses metadata validation or caches a whole-query answer. Position
payload is bounded to 256 bytes per snapshot, plus entry/allocation metadata,
separately from the retained-text byte budget.

New or newly matching files still require an index update to become candidates.


## Candidate planning and verification

Regexes are parsed into `regex-syntax` HIR. Mandatory literals, alternatives,
small character classes, and required repetitions produce conservative byte-gram
constraints. Expansion limits fall back to broader candidates. Unicode-insensitive
literals expand their possible encodings; token boundaries do not restrict substring
recall. Every candidate is verified against source content.

Files-only searches can use whole-buffer matching when the regex provably cannot
cross or inspect line boundaries. Anchored, empty, and other context-sensitive
patterns retain per-line matching. Content output always preserves original UTF-8
byte offsets.

Content caches are sharded and shared across readers in the process. Defaults
limit retained text to 1 GiB and entries to 131,072; `FXI_CACHE_MIB` accepts
0–4096 MiB (0 disables admission). One-shot CLI readers bypass admission.
Cached snapshots are validated against file metadata before each reuse. Unix
validation includes device, inode and change time. On non-Unix platforms, a
metadata hit also rereads and compares the complete UTF-8 source bytes before
reusing cached content or positional evidence; size and modification/creation
timestamps alone cannot distinguish rapid same-size rewrites. This adds read
cost on Windows, so Unix warm-cache timings do not establish Windows performance.
Concurrent queries can keep additional snapshots alive beyond those cache bounds.
Scans estimated to exceed the cache budget reuse valid entries and may fill spare
capacity, but cannot evict other entries. Smaller scans use normal LRU admission.
This avoids repeated cache churn; it does not guarantee that every working set
becomes resident. Filling the cache can increase first-query latency and RSS.


## Index loading and validation

Public `IndexReader::open` and `open_uncached` validate gram and token dictionary
structure and posting ranges when opening an index. One-shot CLI searches load
token dictionaries, token postings and positions only if their query plan needs
them. Loading then performs the same validation and propagates errors through
the query, including nested plans. A gram-only query can therefore succeed when
unused token data is damaged; this is not a complete index integrity check.
Immutable generation leases keep deferred files available for the reader's lifetime.

## File result ordering

Files-only searches return paths in lexical path order. A nonzero file limit
selects that ordered prefix of verified matches, including across segments and
parallel workers. Zero means unlimited. Path-only filters use the same order.

### Optional source packs

On Unix, `FXI_SOURCE_PACK=1 fxi index --force PATH` adds source copies to each
immutable segment. Incremental updates inherit existing packs and create packs
for new segments; compaction recreates them with the remapped document IDs.
Generation publication synchronizes packs and readers pin their lifetime just
like postings. Building packs requires a second source read and additional disk
space. It does not change the existing stale-index candidate visibility contract.

One-shot files-only scans with at least 128 candidates may use packed bytes;
small scans and warm daemon queries retain the ordinary path. Every use first
validates source size, device, inode, modification time and change time. Changed,
missing, unsupported or corrupt evidence falls back to live source verification.
`FXI_SOURCE_PACK=0` disables packed reads. Non-Unix platforms do not use packs.

The versioned table has an XXH3 integrity checksum and each source has both a
whole-file and 4 KiB block checksum. These detect accidental corruption, not
malicious alteration. A proven exact, nonempty case-sensitive literal can return
a positive match after checking all blocks covering that match. A negative must
check all source blocks. The whole source was validated as UTF-8 during capture;
an unchanged source stamp preserves that fact. Corruption in an untouched packed
tail cannot invalidate an already verified positive witness in the live source.
Other verification uses a whole-file checksum and UTF-8 validation. Existing
point-in-time limitations during concurrent source writes remain unchanged.
