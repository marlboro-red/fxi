# FXI audit — 17 September 2026

**Verdict:** FXI has a useful, optimized indexed-search foundation, but it is not currently pushing the limits of indexing technology. Its strongest work is implementation-level optimization and its integrated code-search experience. Several optimizations violate the fundamental requirement that candidates must include every genuine match. Correctness, safe index publication, and freshness need priority over more speed tuning.

Audited FXI commit `e7e5a6ff024e0db1a618967112197730ec49b326` and Microsoft **tgrep** commit `b1d0fc2f6245cc78f1943e5864ceeab812452404` (the Microsoft project referred to as “trigrep”). Public repositories were cloned with `--depth 1`. No product implementation was changed. Supporting scripts and observations are in this directory.

This is a source audit plus targeted execution on macOS, not a proof of absence of other bugs. Windows transports and editor interactions were inspected but not exercised on Windows or in a live VS Code UI. Crash/power-loss and hostile concurrency scenarios were analyzed, not exhaustively fault-injected. Large-monorepo claims remain unverified.

**Validation performed**

- `cargo test --all-targets`: passes. 244 unit tests run in each of the library and binary targets, plus 2 parity-grid tests and 70 compatibility tests; benchmark smoke executions also pass. The duplicated 244 tests are not 488 distinct cases.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`: pass.
- tgrep core checks: 260 unit tests plus 10 integration tests pass; the full tgrep CLI suite was not run.
- VS Code extension: 87 tests pass; `npx tsc --noEmit` passes.
- Additional small-corpus probes reproduce the failures below despite the existing green suite. [Probe source](probe.rs), [observations](probe-output.txt), [live/CLI probe](live-probe.py), [live observations](live-output.txt).
- Fresh release-build benchmark against tgrep and ripgrep, with full matching-file-set equality checked on each measured sample. [Harness](benchmark.py), [raw results](benchmark-results.json).

**Highest-priority findings**

1. **Index publication is not a transaction; compaction overwrites a live mapped segment. P0, source-confirmed design defect.** [compact.rs:123](../../src/index/compact.rs#L123) selects segment 1, creates that existing directory, and writes its files using `File::create` through [segment_io.rs](../../src/index/segment_io.rs). Active readers retain mmap handles to those same files. Truncation/rewrite can invalidate those readers or leave their dictionaries and postings inconsistent. Independently, replacing `docs.bin`, `paths.bin`, and then `meta.json` does not give new readers an atomic snapshot: they open these independently and can combine generations. Writer serialization does not serialize readers. Force rebuild deletes the published index before a replacement exists. Use unique immutable generation directories, publish one manifest pointer atomically after flushing/syncing required files, and retain old generations until readers release them. Do not overwrite mapped files.

2. **A reader deletes an active writer's temporary files. P1, reproduced.** [reader.rs:347](../../src/index/reader.rs#L347) calls `cleanup_tmp_files` without acquiring the mutation lock. Creating `docs.bin.tmp`, then opening a reader, deletes it. Concurrent indexing can consequently fail at rename or publish incomplete state. Recovery cleanup belongs under the writer lock, with ownership/generation information; ordinary readers should not mutate the index.

3. **Recovery can deadlock itself. P1, source-confirmed call path.** [daemon_core.rs:381](../../src/server/daemon_core.rs#L381) acquires `IndexLock`; metadata/delta/compaction failures call `trigger_rebuild` while the guard remains in scope. [trigger_rebuild:514](../../src/server/daemon_core.rs#L514) acquires the same exclusive lock via another file handle. The project's lock test explicitly verifies that a second handle cannot acquire it. Split locked/unlocked entry points or transfer the held guard; do not reacquire. This path can stall the watcher processor and keep later writers waiting indefinitely.

4. **Repeated compaction destroys recall even without concurrency. P1, reproduced.** [compact.rs:119](../../src/index/compact.rs#L119) recomputes stop-grams from the postings that remain, while the previous compaction physically removed postings for its stop-grams. The next compaction forgets those omissions and queries interpret the absent postings as no matches. In the probe, `"r::st"` finds three `vector::start` files before and after the first compaction, then zero after adding an unrelated file and compacting again. Preserve omitted-gram coverage metadata across generations, or retain postings; making a gram selective again requires reconstructing its missing historical postings. This is an invariant violation in maintenance, not a documented approximate-search tradeoff.

5. **OR drops any branch that cannot narrow. P1, reproduced.** [planner.rs:410](../../src/query/planner.rs#L410) only retains OR subplans with nonempty steps. `needle | x` misses a file containing `x` when the indexed `needle` branch has candidates elsewhere. The one-character branch needs an all-documents candidate set. Explicit `MatchAll` and `MatchNone` plan nodes would make the algebra clear: `A OR MatchAll = MatchAll`. This also affects OR with an unnarrowable regex.

6. **Case-insensitive substring recall is unsound; short substrings also disappear. P1, reproduced.** [planner.rs:160](../../src/query/planner.rs#L160), [executor.rs:604](../../src/query/executor.rs#L604). Query `need` misses `PREFIXNEEDLESUFFIX`: token equality does not find the interior substring and byte-exact trigrams cannot see uppercase bytes. `oo` misses `foobar` because two-byte searches only use whole-token lookup. This is broader than the documented punctuation/stop-gram exception and contradicts bare-token case-insensitive substring semantics. Index folded trigrams with a sound Unicode strategy or scan when the index cannot prove a safe restriction. Token lookup may supplement candidates, not substitute for arbitrary substring recall.

7. **Positional token filtering rejects valid substring phrases. P1, reproduced.** [planner.rs:306](../../src/query/planner.rs#L306), [reader.rs:653](../../src/index/reader.rs#L653). `"bar baz"` must match `foobar bazqux` under documented exact-substring semantics; it returns nothing because the planner additionally requires complete `bar` and `baz` tokens. Token adjacency is safe only when query semantics guarantee those token boundaries. Retain trigram verification for arbitrary phrase edges; use positional acceleration only where its preconditions are proven.

8. **Valid Unicode text crashes the CLI. P1, reproduced with exit 101.** Searching `foo` in `K foo` produces offsets 2..5 from the lowercased string, although the original match starts at byte 4. [executor.rs:1459](../../src/query/executor.rs#L1459) reports offsets from transformed text; [output.rs:227](../../src/output.rs#L227) slices the original at byte 2, inside the Kelvin sign. Clamping to string length does not establish a UTF-8 boundary. Use a matcher returning original offsets, or maintain an explicit transformation-to-original mapping. Also test expanding lowercase mappings and Unicode case-fold equivalence.

9. **Watcher deletion events are discarded. P1, live reproduced.** [daemon_core.rs:1322](../../src/server/daemon_core.rs#L1322) checks `path.is_file()` before accepting even a deletion. Deleted paths fail that check. After deletion, cached content results and `ext:txt` continue to return the removed file. Rename events are flattened into modifications and old names suffer the same problem; directory operations need their own handling. Process removal paths using index membership rather than current existence, handle rename pairs/subtrees, and reconcile after overflow or notification failure. There is no periodic repair loop comparable to tgrep's reconciliation.

10. **Freshness guarantees are contradicted by both caches. P1, reproduced for file cache and live daemon.** [reader.rs:789](../../src/index/reader.rs#L789) returns cached text without checking metadata. Query a file, replace its text, query again with the same reader: the old match survives. The daemon's result cache also bypasses verification. The semantics document's assertion that every candidate is re-read and stale lines are pruned is false. Define snapshot versus live semantics explicitly. In live mode, invalidate on events immediately and validate file identity/stamps as appropriate; expose index generation/freshness to callers.

11. **Cache invalidation has a generation race. P1, source-confirmed interleaving.** [daemon_core.rs:94](../../src/server/daemon_core.rs#L94) swaps readers and clears shared caches, but an already-running query against the old reader can subsequently populate those caches. `reader_version` is incremented but never read to validate cache hits/inserts. Old results can survive a completed reload. Store caches with their immutable reader generation or include and check generation at insertion and lookup.

12. **Timestamp units differ between writers; incremental scans miss same-second edits. P1, source-confirmed and targeted probes.** Full/incremental scans store seconds ([build.rs:699](../../src/index/build.rs#L699)); watcher deltas store nanoseconds ([daemon_core.rs:1244](../../src/server/daemon_core.rs#L1244)). Recency and `mtime:` filters interpret the latter incorrectly, and subsequent scans think unchanged files changed. Separately, seconds-only equality ignores edits within the same second—even when file size changes. The probe preserves mtime while changing size/content and the new term remains undiscoverable after update. Use one versioned timestamp representation plus size/file identity, with a hash or authoritative event/reconciliation path where needed.

**Additional correctness and product defects**

13. **`-l` changes regex semantics and ignores line filters. P1, reproduced.** [executor.rs:468](../../src/query/executor.rs#L468) checks regexes against an entire string, whereas content search checks each line. `re:/^needle$/` matches the line `needle\n` in normal mode but reports no file in `-l`. `needle line:20-30` returns a file in `-l` whose only hit is on line 1. Share one matching contract, with a boolean/first-match collector for files-only mode.

14. **Filename fallback bypasses filters and has inconsistent unlimited behavior. P2, reproduced/source-confirmed.** [executor.rs:260](../../src/query/executor.rs#L260) appends filename matches from all valid documents, after filtered content execution. Ranked `ext:rs needle` returns `needle.md`. Boolean constraints likewise are not fully reapplied. `find_filename_matches` immediately stops when limit is zero even though zero means unlimited elsewhere. Make filename search an explicit, properly filtered plan branch.

15. **Invalid regexes become successful empty searches. P2, reproduced.** `re:/[/` returns no matches instead of an error because [executor.rs:130](../../src/query/executor.rs#L130) converts compilation failure into `None`. Parse/compile once before candidate execution and return a structured error. This also avoids a thundering herd of compilations across candidate workers on a cache miss.

16. **Non-UTF-8 search support is overstated. P2, source-confirmed.** The indexer can record trigrams for non-UTF-8 files, but both verification paths reject their contents: `read_to_string` and `from_utf8(...).ok()?`. Such files cannot actually produce the substring matches promised by `SEMANTICS.md`. Either use byte-oriented verification/defined decoding or document exclusion.

17. **Missing/corrupt segments silently produce incomplete answers. P1, source-confirmed.** [reader.rs:376](../../src/index/reader.rs#L376) skips missing segments and logs-and-skips unreadable ones, still returning a usable reader. There is no trustworthy completeness marker in results. Fail opening or use a safe filesystem fallback; a successful empty result must not mean “part of the index could not be read.” Validate format versions, lengths, offsets and counts before trusting disk allocations/slices.

18. **Live-file mmap has an unsafe stability assumption. P1 risk, source review; not crash stress-tested.** [executor.rs:94](../../src/query/executor.rs#L94) validates a mapped editable file once, then [reader.rs:310](../../src/index/reader.rs#L310) constructs unchecked `&str` repeatedly. An external in-place rewrite can invalidate UTF-8 after validation; truncation can fault mapped access. Read mutable source into owned buffers, or enforce a real immutable-snapshot guarantee. Index mmap is appropriate when generation files are immutable.

19. **VS Code can display an older query after a newer one. P2, source-confirmed.** `SearchPanelProvider.handleSearch` posts each asynchronous response unconditionally; transport request IDs associate responses with promises but do not establish which query is current. The daemon supports out-of-order completion. Add a UI query sequence/cancellation check, including stale errors. The provider also ignores `resolved_root` and opens relative results against the workspace folder, which is wrong when the daemon resolves a subfolder workspace to a parent repository root. Tests cover message shapes and client correlation, not these provider behaviors.

20. **Ranked top-k is approximate without a sound bound. P2 design limitation.** [executor.rs:195](../../src/query/executor.rs#L195) stops after approximately 1.5× the requested line count, then scores/sorts. A higher-scoring or more recent file encountered later can never win. Parallel completion and index document order affect output. `^term` boosts are collapsed into a file-level multiplier rather than a proper per-term relevance contribution. Decide whether approximate early results are intentional; for exact top-k, use bounded scoring with proven upper bounds or score the full candidate set.

21. **Declared Rust support does not match the locked build. P2, metadata-confirmed.** `Cargo.toml` declares Rust 1.85, but `cargo metadata` reports locked `ratatui`/`ratatui-core` requiring 1.86 and `time` requiring 1.88. CI only tests the current stable compiler. Raise the declared minimum to the supported build floor (and test that floor), or select compatible dependencies/code. No actual Rust 1.85 build was attempted.

**Performance and architecture assessment**

Worth preserving: selectivity-ordered trigram intersections, filtered posting decoding, Roaring operations, per-segment parallelism, segment Bloom filters, flat posting construction, delta/varint compression, positional token storage, bounded writer queue, and avoiding redundant content/context reads. These are substantive engineering, not empty marketing.

However, the current design still has large avoidable costs:

- Content queries collect every matching line and sort before the daemon/CLI applies the result limit. `-m 10` does not mean bounded work or memory. Counts also materialize line strings. Introduce collectors for existence, count, bounded ordered output, and streaming output; preserve each mode's ordering contract.
- Result caches are bounded by 128 entries, not bytes. Each entry may contain enormous strings/context, up to a ten-million-result daemon cap, and responses clone cached vectors. Multiple loaded repositories multiply the budget. Use byte-accounted admission/eviction and bounded transport queues/backpressure.
- Full-build chunks are limited by 2,000 **files**, not bytes/postings/positions. The bounded two-job queue is good but cannot guarantee bounded RAM with heterogeneous files. Compaction materializes merged posting and position maps for the whole index. Use byte budgets and streaming k-way merge/external spill.
- The regex planner only extracts a prefix, gives up on alternation, and disables narrowing for `-i`. It misses highly selective mandatory literals later in a regex. Parse `regex-syntax` HIR and derive conservative AND/OR constraints, with a tested MatchAll fallback.
- Path/extension filtering and filename fallback scan document metadata; substring fallback scans token dictionaries. Add normalized path indexes/metadata bitmaps or an FST where profiling warrants it, and order intersectable filters by estimated cost/selectivity.
- Case-insensitive verification lowercases entire contents. A compiled byte/Unicode matcher can reduce allocation while preserving original offsets.
- Hardcoded candidate-count parallelism thresholds, file-count chunk sizes, stop-gram cutoffs, and compaction thresholds are heuristics, not universal optima. Instrument candidate counts, bytes verified, postings decoded, cache bytes, and latency distributions before tuning.

**Comparison with Microsoft tgrep**

Both tools solve the same core problem: avoid scanning every source file on every local search using a persistent trigram inverted index, then verify candidate files. FXI additionally targets ranked interactive code discovery with token-aware queries, proximity, a TUI and VS Code integration. tgrep puts more emphasis on grep compatibility and operationally reliable indexed regex search.

The following comparison is based on the pinned source, not either README's speed claims:

| Area | FXI | tgrep | Tradeoff / assessment |
|---|---|---|---|
| Query narrowing | Exact-byte trigrams plus token/position index; prefix-only regex extraction | HIR-derived regex plans, explicit MatchAll, OR plans | tgrep has the stronger general regex planner; FXI's token features are useful only when used soundly |
| Case-insensitive candidates | Tokens plus exact-case grams; `-i` regex scans | Original and ASCII-folded grams, conservative handling for unsupported narrowing | tgrep's approach provides a better foundation for common code-search workloads; Unicode still needs explicit semantics |
| Postings | Delta/varint document IDs, Roaring execution, separate token positions | Fixed 6-byte postings: ID plus offset-mod-8 mask and next-byte mask | FXI can be smaller; tgrep spends extra bytes to reject false candidates cheaply and simplify decoding |
| Fresh edits | Persistent delta batches, default 60-second flush before visibility | Live in-memory overlay plus tombstones over disk index, later persistence | tgrep decouples visibility from disk-flush latency; overlay consistency and memory accounting add complexity |
| Recovery/coverage | Skips broken segments; no periodic watcher repair | Coverage evidence, scan fallback, watcher recovery/reconciliation, immutable reader snapshots | tgrep is materially stronger here; not a claim of complete crash-proofness |
| Build/merge memory | File-count chunks; map-materializing compaction | Byte-budgeted batches and streaming/external build machinery | tgrep addresses heterogeneous files more directly; machinery is more complex |
| User experience | Rich file-level Boolean/ranking/proximity, TUI, extension | Much broader rg-compatible flags and regex/encoding behavior | FXI has a credible differentiated product direction; grep interchangeability is presently tgrep's strength |

Sources: pinned [query planner](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-core/src/query.rs), [trigram extraction](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-core/src/trigram.rs), [posting format](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-core/src/ondisk.rs), [hybrid reader](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-core/src/hybrid.rs), [builder](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-core/src/builder.rs), and [server](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-cli/src/serve.rs).

The repository README for tgrep describes a simpler four-byte posting format; the inspected implementation uses six bytes. This is another reason to inspect source rather than copy documentation claims.

**Fresh benchmark evidence**

Apple M2 Max, 64 GiB; release binaries; ripgrep 15.2.0. Redis commit `07a33b919581e2b0663e34731a490d768e553dfa`, copied to a controlled corpus of **1,098 nonempty UTF-8 source/documentation files, 18,044,223 bytes**. The harness selects `.c/.h/.tcl/.py/.md/.rs`, excludes symlinks, NUL-containing files, hidden/excluded paths and oversized files. This intentionally equalizes scope; it is not the whole Redis checkout.

Each query uses regex semantics in all tools and files-only unlimited output. Nine samples per tool/query/mode, interleaved in deterministically shuffled tool order, warm OS cache, end-to-end subprocess time including output capture. All observed matching-file sets agree, with no missing/extra paths. Server samples append distinct empty noncapturing groups to bypass FXI's full-result cache while retaining warm content caches. Both servers are dedicated to the fixture. Watching is disabled. Separate repeated-query measurements are retained in JSON.

Median milliseconds, warm servers, **result cache bypassed**:

| Regex | Matching files | FXI | tgrep | rg |
|---|---:|---:|---:|---:|
| `raxFind` | 10 | 4.28 | 5.38 | 20.14 |
| absent symbol | 0 | 3.86 | 4.93 | 21.52 |
| `static void` | 269 | 8.72 | 6.75 | 24.58 |
| `return` | 774 | 26.15 | 8.67 | 29.70 |
| `raxFind\|dictRehash` | 13 | 25.01 | 5.70 | 21.31 |
| `.*raxFind` | 10 | 24.76 | 5.86 | 21.37 |
| `serverassert` with `-i` | 60 | 24.58 | 6.27 | 21.94 |

Without servers (warm filesystem, index reopened for each process), FXI medians range **12.42–34.50 ms** and tgrep **4.72–35.89 ms**. FXI repeated-result-cache medians are **3.61–4.61 ms**, a separate workload from actually searching.

Interpretation: FXI wins the selective/absent server cases here; tgrep wins the broader, alternation, internal-literal, and insensitive cases. FXI is slower than rg for several regex shapes because it pays indexing overhead then still scans. This supports improving the planner rather than adding more low-level micro-optimizations. It does not establish a universal tool ranking or explain all latency differences causally.

One build sample per tool: FXI 0.137 s, ~175.7 MiB maximum RSS, 10.80 MB index; tgrep 0.114 s, ~76.6 MiB RSS, 15.50 MB index. Build timings include `/usr/bin/time`/process overhead and are too short/single-sample to establish a reliable throughput winner. The local size/memory tradeoff is informative: FXI's smaller persistent representation does not imply a smaller builder working set. Index sizes depend on segmentation and corpus. No cold-page-cache claims, million-file extrapolations, p99 claims, or Chromium/Linux reproduction are made.

**Why the existing benchmark claims are not established**

- `scripts/benchmark.sh` and `scripts/benchmark-chromium.sh` warm the exact FXI queries, then repeatedly time them without clearing result caches. They do not reproduce README methodology claiming cache-cleared novel queries.
- Both give rg `--no-ignore` while FXI respects ignore rules and hard exclusions. The searched corpus differs.
- Bare FXI tokens are case-insensitive; bare rg regexes are not. “Phrase” rg commands omit `-F`, so regex punctuation can change meaning or fail to compile (`DCHECK(` is an example).
- Counting output lines does not prove matching-file-set equivalence, much less line/span equivalence. Equal counts can hide entirely different answers.
- `/usr/bin/time -p` precision is poor for single-digit millisecond results, three runs are insufficient for tail behavior, and fixed order confounds cache/thermal effects.
- The README's claim that small count deltas come from symlinks/encodings has not been demonstrated per missing file. The reproduced recall bugs provide other plausible explanations; do not assign a cause without examining differences.
- Linear extrapolation from 449k files to 1M ignores file-size distributions, vocabulary/posting growth, compaction and RAM pressure. “Up to ~400x” remains an unverified workload-specific historical claim.

**Where to invest next**

1. **Establish exactness and publication safety.** Encode candidate-superset invariants in differential/property tests; cover mixed OR branches, phrase edges, case folding, short terms, stop-grams, missing/corrupt segments, both output modes, and generation changes. Replace mutable publication and nested locking. Add fault-injection/restart tests at every publication boundary.
2. **Make freshness explicit and fast.** Shared eligibility logic for scans/watchers, consistent timestamps, deletion/rename/subtree handling, immediate cache invalidation, generation-safe caches, reconciliation, and an in-memory delta overlay. Distinguish “indexed through generation X” from “current filesystem.”
3. **Adopt HIR planning and folded candidate indexes.** Highest demonstrated search-performance opportunity, while fixing recall. Google's [codesearch regex planner](https://github.com/google/codesearch/blob/master/index/regexp.go) and tgrep provide useful precedents.
5. **Bound work and memory end to end.** Byte-budgeted ingestion, streaming compaction, mode-specific result collectors, byte-limited caches, cancellation and output backpressure. Benchmark branch switches and concurrent edits/searches, not only steady-state queries.
6. **Experiment with stronger substring evidence.** tgrep-style next-byte/location masks are a relatively small format experiment. Positional trigrams/block-level postings can reduce verification I/O for long files, at storage/build cost; [Zoekt's design](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md) is a useful reference. Sparse/selective longer n-grams are another experiment, but require a demonstrably sound extraction/query pairing and measured workload benefit. Do not remove common grams in ways that turn “cannot narrow” into “no match.”
7. **Invest in differentiated code discovery after correctness.** Symbol/definition/reference fields, identifier-aware ranking, indexed paths and metadata, optional syntax-aware extraction, and reliable editor navigation would develop FXI's strengths. Embeddings are optional for natural-language discovery, not a replacement for exact lexical/regex recall.

A credible next benchmark suite should publish corpus commits and eligibility manifests, exact commands, raw samples, per-query full result diffs, candidate/verification counters, warm-index versus result-cache versus cold-start modes, build/update/compaction RSS and disk bytes, and p50/p95/p99 under concurrent churn. Performance regressions and correctness failures should be separate outcomes; a faster query that misses matches is a correctness failure.

To rerun the Rust observations, copy `probe.rs` into `tests/audit_probe.rs`, run `cargo test --test audit_probe -- --nocapture`, then remove that temporary copy. These are observational probes that print discrepancies, not regression tests claiming incorrect results are acceptable. The Python scripts run from the FXI repository root; public checkout paths and binary locations are explicit in their source.
