# fxi search semantics

This document is the contract for what fxi matches, what it can miss, and how
fresh its results are. Where behavior differs from ripgrep, the difference is
listed explicitly. Claims marked with a test name are enforced by
`tests/parity_grid.rs` or the named unit test.

## Choose a pattern mode

The shell removes its own quotes before invoking fxi. These commands therefore
have different meanings:

| Command | Meaning |
|---|---|
| `fxi error` | Case-insensitive substring |
| `fxi 'fn main'` | File-level AND: both substrings, anywhere in the same file |
| `fxi '"fn main"'` | Exact, case-sensitive phrase on one line |
| `fxi -F 'fn main'` | The same literal text, without query-language parsing |
| `fxi --regex 'fn\s+main'` | Case-sensitive Rust regular expression |
| `fxi 'foo | bar'` | File-level OR |
| `fxi 'foo -bar'` | Files containing foo and no bar |
| `fxi -e foo -e bar` | OR of two patterns in the selected mode |

`-F` and `--regex` are mutually exclusive. Both are case-sensitive unless `-i`
is supplied. Default query mode keeps each `-e` branch's own semantics; combining
bare terms with `-e` does not make them case-sensitive. Use `fxi -F -- '-foo'`
for literal leading punctuation, and `fxi -- index` to search a subcommand name.

Within query mode, adjacency means AND and `|` means OR; parentheses group
expressions. Interior punctuation is literal: `foo-bar`, `a:b`, `f(x)`, and
`x^y` are not split into implicit operators. A leading `-` negates a term.
Quoted phrases accept escaped quotes and backslashes; regex delimiters accept
escaped slashes and slashes inside character classes.

Parsing is fallible and bounded to 64 KiB, 1,024 terms/nodes and 32 nested groups.
Malformed expressions, invalid regular expressions, incomplete fields, nonfinite
boosts and invalid numeric/date ranges produce errors. Library users should use
`try_parse_query`; compatibility `parse_query` retains an invalid node that
execution rejects, rather than converting a bad query into an empty search.

## Filters and file predicates

Filters apply globally to the entire query. Write `ext:rs (foo | bar)`; filters
inside parentheses, under NOT or mixed ambiguously with ungrouped OR are errors.
Repeated fields are errors, except complementary lower/upper size or time bounds.

| Filter | Meaning |
|---|---|
| `path:src/*.rs` | Glob over paths relative to the index root |
| `file:main.rs` | Filename filter |
| `ext:rs`, `lang:rust` | Extension or recognized language |
| `size:>1000 size:<10000` | Strict file-size bounds in bytes |
| `line:10-20` | Inclusive, positive source-line range |
| `mtime:2026-09-18` | UTC calendar day, ending before the next midnight |
| `mtime:>1704067200` | Strict bound in Unix seconds; dates also accepted |
| `near:foo,bar,10` | Terms in one shared proximity window; order-independent |

A Boolean predicate selects files first. Content output contains unique positive
matching lines in those files: `foo foo` does not duplicate rows/counts. NOT
contributes no fabricated source line. A pure-negative or filter-only result is
a file-level record with placeholder line number 1 and empty content; use `-l` for these
queries when only paths are wanted. Highlighting currently retains one matching
span per result line, rather than every matching occurrence.

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

- `-w` applies Unicode regex word boundaries to each positive or negative matcher,
  preserving its case mode and Boolean structure. Boost/proximity queries with
  `-w` currently return an explicit unsupported-combination error.
- `-i` affects phrases and regexes; it is a no-op for bare tokens, which are
  already case-insensitive (parity: *"-i mixed-case query"*).
- `-v` (invert) is **unsupported**: an index can return matching lines, not
  non-matching ones.
- `-m N` / `--max-count N` is a global result limit, not ripgrep's per-file
  limit. Zero means unlimited at the CLI; server resource caps still apply.
  Files-only limits select paths; counts limit matching rows across files.
  `top:N` is for ranked search and is rejected in the content CLI: use `-m`.
- `fxi PATTERN PATH` and `-p PATH` restrict results to that file/subtree while
  finding its owning index. Subtree matching respects path-component boundaries.
- Piped text always includes paths. `--heading` and `--no-heading` override the
  terminal default; overlapping context is merged in source-line order.
- `--json` emits one response object. `-l -0` emits NUL-delimited paths, including
  names containing newlines. They cannot be combined. JSON match offsets are
  UTF-8 byte offsets, not JavaScript string indices.
- `--color auto` colors only terminal output when `NO_COLOR` is absent and `TERM`
  is neither missing nor `dumb`. Explicit `always`/`never` override detection.
- No matches exits 0; invalid input and failed operations exit nonzero. This is
  deliberately different from grep/ripgrep's no-match exit 1. Broken stdout
  pipes exit quietly and successfully. Stdin content search is unsupported;
  invoking without a pattern requires an interactive terminal.

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
- Intentionally excluded content is remembered with its metadata so incremental
  scans can skip it until it changes. Read/traversal/metadata failures are errors,
  not exclusions: they leave the published generation intact and watcher work
  pending for retry.
- Root and indexed relative paths must be valid UTF-8. Unsupported native path
  bytes are rejected explicitly rather than converted lossily.

## Freshness

Search results reflect **the index as of its last update**, with one
correction: every candidate file is verified against a read or metadata-validated snapshot at query time, so changed candidates are rechecked instead of blindly returning stored lines.
Context is rendered from the same immutable per-file snapshot used to verify its
matches. Concurrent edits can still occur during or after the source read; there
is no transactional snapshot of the whole tree.

The asymmetry to understand: stale matches are pruned, but **files created or
made-matching since the last index update are invisible** — narrowing cannot
surface a document the index has never seen.

How the index stays fresh:

- `fxi index` performs an incremental update (parallel tree scan, mtime
  comparison, delta segment for changes).
- A daemon started with `--watch` reconciles each root when its watcher starts.
  Precise file notifications then use a scoped version of the same ignore-aware
  walker, preserving ancestor ignore rules without scanning unrelated subtrees.
  Directory/ignore-rule changes, ambiguous notifications, overflow and external
  generation changes require a full scan. Explicit file hints force re-indexing
  even when file size and modification time are preserved. Startup registration
  and periodic five-minute reconciliation also repair membership.
- The default quiet debounce is 1 ms and maximum event age is 100 ms.
  `FXI_DEBOUNCE_MS` and `FXI_MAX_BATCH_AGE_MS` override these; the config-file
  equivalents are `watcher.debounce_ms` and `watcher.max_batch_age_ms`.
  Notification delivery, indexing work and writer contention add latency.
- Up to 256 changed files with at most 8 MiB of accepted source content use a
  fully indexed in-memory snapshot for early visibility. It includes real
  postings, positions and tombstones, and uses the ordinary query engine for all
  output modes. This bounds source material, not total process RSS. Unchanged
  segments and base paths are shared; document metadata and appended paths are
  copied. Each preview is rebuilt from the durable reader and the whole pending
  path set, so repeated saves do not accumulate memory-only segments.
- By default, persistence is scheduled after 250 ms of quiet or ten seconds
  of continuous updates. Larger batches and rebuilds use the durable path
  immediately. `FXI_DELTA_FLUSH_SECS > 0` instead sets the first-event delay for
  persistence, without delaying eligible memory previews. Graceful shutdown stops and joins producers, reconciles their final batches and
  acknowledges success only after pending updates are persisted. Contended or
  failed work is retried within a bounded shutdown window; incomplete persistence
  returns an error. `daemon stop --force` may discard pending work.
  Interrupted/uncommitted updates are repaired
  when a watched daemon starts again. Direct readers can lag behind a running
  daemon's in-memory view. Existing generation durability barriers are retained.
  Persistence/compaction still runs on the update processor and can delay events
  arriving during that work.
- `fxi index` always performs reconciliation, including watched roots. After a
  successful CLI build or compaction, it reloads the daemon's generation before
  reporting success. `fxi remove` unloads/stops a loaded root and removes its
  index under the writer lock; queued watcher hints cannot recreate it.
- `daemon start --watch` fails explicitly if an existing daemon has watching
  disabled. Stop/restart to change mode. `daemon status` reports watch mode and
  actual watched roots; it does not claim to measure query-cache hits or RSS.
- All index writers (CLI builds, daemon flushes, compaction) hold a
  per-index advisory lock, so two writers can never interleave segment or
  metadata writes. Ordinary watcher batches defer when another writer holds
  that lock, allowing other roots to progress. Failed batches remain pending
  and retry with a one-second backoff.
- Searching without a daemon prints a note to terminal stderr when the index is
  more than an hour old (`FXI_STALE_WARN_SECS`, 0 disables).

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
byte offsets. Context queries retain verified source snapshots through rendering,
so a concurrent edit cannot mix old matches with newly read context. This can
increase peak memory for broad context queries; no-context/ranked requests do
not retain those extra source snapshots.

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
Unix files-only scans estimated to fit this cache use up to eight verification
tasks by default; ordinary reads retain four. `FXI_SEARCH_PARALLELISM` overrides
both policies. This avoids repeated cache churn; it does not guarantee that every working set
becomes resident. Filling the cache can increase first-query latency and RSS.


## Index loading and validation

Public `IndexReader::open` and `open_uncached` validate gram and token dictionary
structure, posting ranges and complete gram payloads when opening an index.
Malformed varints, unknown/out-of-order document IDs and frequency mismatches
fail closed. Token postings and position streams are validated when loaded. CLI and daemon searches load
token dictionaries, token postings and positions only if their query plan needs
them. Loading then performs the same validation and propagates errors through
the query, including nested plans. A gram-only query can therefore succeed when
unused token data is damaged; this is not a complete index integrity check.
Immutable generation leases keep deferred files available for the reader's lifetime.
Daemon reloads and watched updates use the same dependency-aware loading. Public
reader constructors and compaction retain eager token validation. Deferral reduces
initial query latency and resident memory; a future token-dependent query pays
the loading cost and receives any validation error before using that data.

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

#### Experimental compressed source packs

On Unix, setting both `FXI_SOURCE_PACK=1` and
`FXI_SOURCE_PACK_COMPRESSION=1` during indexing writes `FXISRC03` packs for new
segments. Existing inherited packs retain their format; rebuild or compact with
both settings to convert them. Readers automatically recognize both formats.
The compression setting affects writing, while `FXI_SOURCE_PACK=0` still disables
all packed reads. Compression remains opt-in because its speed tradeoffs depend
on the query. Windows behavior is unchanged.

Each file retains its first 4 KiB uncompressed. Later independent 4 KiB blocks
use LZ4 only when smaller than the original. A 2,048-bit trigram filter describes
each block plus at most 255 following bytes. For exact case-sensitive literals
of 3–256 bytes, a filter can rule out match starts in that block. Every occurrence
of such a literal has all its trigrams within this extended interval, so a filter
cannot reject a real occurrence. Possible blocks and any required next-block
boundary bytes are decoded and checksummed before returning a positive result.
Other lengths and other verification use complete decoding, a whole-file
checksum and UTF-8 validation. Compression can make that full-read path slower.

File records have the same source identity/timestamp evidence as raw packs.
Per-file descriptors and filters live in the immutable mapped data file, with a
checksum referenced by the checksummed source table. Readers validate a file's
metadata before trusting its filters, without eagerly loading every filter in
the segment. Negative filters describe the captured source, so rejecting a block
does not require reading its compressed payload. Corrupt metadata or any invalid
payload that is needed for verification triggers the ordinary live fallback.
These are accidental-corruption checks, not authentication. Filters must never
be constructed from the earlier tokenization snapshot.

Legacy readers that do not understand `FXISRC03` ignore the optional accelerator
and read live sources. No primary-posting format changes are involved. The
[compression experiments](performance-source-compression/NOTES.md) record the
measured tradeoffs and remaining limits.

### Experimental certified negative routing

`FXI_NEGATIVE_ROUTING=1` enables certificate creation during index publication and
its use for direct files-only searches. Both build and query processes need the
setting. It is independent of source packs and remains off by default.

The proof currently accepts only exact case-sensitive regex literals of at least
three bytes, with no additional filters or line breaks. It pins the generation,
checks a checksummed certificate bound to the exact metadata and generation, and
compares each opened Bloom handle's Unix identity, size, mtime and ctime before
probing a checked mapped view. Only when every segment rejects the literal does
it compare every certified core dependency stamp and return an empty result.
Invalid queries, damaged/missing/changed evidence, and unsupported platforms
fall back to ordinary opening and its existing validation and errors. Stale-index
warnings and the existing index-update visibility contract are preserved.

Certificate creation structurally validates inherited as well as new document,
path, gram dictionary and complete gram posting data, and verifies every Bloom covers
its gram dictionary. Unused token/line-map/source-pack data retain their existing
independent validation. Checksums detect accidental corruption, not adversarial
modification. The same immutable-index and Unix metadata-validation assumptions
as the ordinary reader and source cache apply.

Certificates are freshly generated, never inherited. Collecting older hardlinks
can change ctime on the current generation's inherited files, immediately making
a delta certificate unusable. This safely restores ordinary validation; it can
remove the performance benefit after updates. The experiment retains the ctime
check rather than weakening validation to keep a certificate usable.
