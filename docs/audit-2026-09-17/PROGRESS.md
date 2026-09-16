# Correctness fixes and performance experiments

This report follows the [original audit](AUDIT.md). Changes were committed separately as fixes and experiments progressed. The final measured application revision is `4ba30e9`; raw JSON records full revisions, binary hashes, corpus manifest hashes, and samples.

**Outcome:** the audited failures have regression fixes or an explicit supported scope. Warm-server medians beat the pinned tgrep build in all 14 measured query/corpus cases. This is not a universal performance claim: direct startup, build memory, large-scale behavior, and several operational limits still need work. No new indexing-theory result is claimed.

## Correctness work

| Audit findings | Change and regression coverage |
|---|---|
| 1, 2: publication and reader/writer races | Immutable generations; atomic `CURRENT` publication; flush/sync before publication; shared reader leases protect old segments from reclamation. Readers never remove writer temporaries. Tests cover concurrent opens, retained readers, failed rebuilds, compaction and deletion-only deltas. |
| 3: recovery deadlock | Recovery reuses the held writer lock. Startup reconciliation also requires successful lock acquisition. Timeout regression covers recovery. |
| 4: repeated compaction recall | Preserve coverage metadata for omitted grams across generations. Repeated compaction/update regression retains punctuation substring matches. |
| 5–7: query recall | OR retains unconstrained branches. Arbitrary substrings no longer require whole tokens or token adjacency. HIR-derived constraints conservatively include Unicode case alternatives, short terms, phrase edges and optional branches. |
| 8, 18: source safety | Original UTF-8 offsets from matching; owned snapshots for mutable source files. No mutable-source mmap or unchecked string borrow. Unicode/proximity and concurrent rewrite/truncate snapshot tests. |
| 9–12: freshness | Shared ignore-aware reconciliation for watcher hints, including deletion, directory rename, ignore changes, errors and periodic repair. Consistent versioned nanosecond timestamps, size comparisons and legacy normalization. Metadata-validated content snapshots; full-query caches removed from daemon and TUI. |
| 13–15: verification and result constraints | Files-only/content agree on per-line regex and line-filter behavior. Filename fallback reapplies filters and Boolean constraints; zero means unlimited. Invalid regexes error even with no candidates. |
| 16: encoding | Explicit UTF-8-only scope; reject unsearchable non-UTF-8 files at ingestion. This is a scope correction, not a byte-search implementation. |
| 17: incomplete/corrupt indexes | Required segment files must open; validate supported versions, ordering, posting ranges, and count/length allocation bounds. Missing/truncated files and impossible counts fail rather than silently dropping segments. This is not full bit-rot detection. |
| 19: editor races | Ignore superseded responses and errors; ignore completion after disposal; navigate using the response's resolved root. Four provider regressions added. |
| 20: ranking | Score the complete verified set before top-k. Boosts apply to matching lines, not every branch/file; quoted boosts preserve phrase semantics. Ranking remains an explicitly documented heuristic. |
| 21: Rust support | Declare Rust 1.88, check it in CI, and successfully build all targets with that toolchain. |

The HIR differential test compares files-only and content results with brute-force regex matching across generated patterns, Unicode, alternation, repetition, anchors, delta updates and compaction. The experimental signature test exhaustively checks 1,024 ten-byte binary-alphabet strings against all patterns of lengths 3–6. Neither is an exhaustive proof for the whole application.

## Measured search performance

Apple M2 Max, 64 GiB; release builds; ripgrep 15.2.0. tgrep remains pinned at `b1d0fc2f6245cc78f1943e5864ceeab812452404`. Public repositories were cloned with `--depth 1`.

- Redis `07a33b919581e2b0663e34731a490d768e553dfa`: 1,098 eligible files, 18,044,223 bytes.
- CPython `0a6c1ed34118b091230ee38fc047bcc2df8e5c5e`: 3,480 eligible files, 78,457,530 bytes.

These are controlled UTF-8 source subsets, not complete checkout sizes. Eligibility is explicit in the harness. Each final case has 15 deterministically shuffled/interleaved samples per tool, complete matching-file-set equality assertions, unlimited files-only output and equivalent regex semantics. OS caches are warm. Server requests use distinct equivalent regex strings; FXI has no result cache. Its content cache is warm and metadata-validated. Direct mode reopens the index in a new process. Final runs were sequential, outside our builds/tests; ordinary OS scheduling noise remains.

Redis warm-server medians in milliseconds. “Before” is the saved corrected binary (`d47e8e1`), remeasured after the optimization work, not the buggy pre-audit implementation.

| Query | Matches | Before FXI | Final FXI | tgrep | rg | FXI improvement |
|---|---:|---:|---:|---:|---:|---:|
| `raxFind` | 10 | 5.34 | 4.43 | 5.51 | 21.48 | 1.21× |
| `auditNonexistentSymbol94283` | 0 | 3.93 | 3.88 | 4.79 | 21.17 | 1.01× |
| `static void` | 269 | 8.05 | 5.43 | 6.72 | 23.88 | 1.48× |
| `return` | 774 | 18.88 | 7.17 | 8.90 | 29.87 | 2.63× |
| `raxFind\|dictRehash` | 13 | 16.80 | 4.65 | 5.64 | 21.38 | 3.62× |
| `.*raxFind` | 10 | 16.71 | 4.47 | 5.69 | 21.87 | 3.74× |
| `serverassert` (`-i`) | 60 | 16.66 | 5.15 | 6.27 | 21.75 | 3.24× |

CPython warm-server medians in milliseconds:

| Query | Matches | Final FXI | tgrep | rg |
|---|---:|---:|---:|---:|
| `PyObject_GenericGetAttr` | 79 | 5.18 | 7.12 | 51.54 |
| `auditNonexistentSymbol94283` | 0 | 4.44 | 5.31 | 50.66 |
| `static void` | 295 | 5.85 | 8.22 | 55.08 |
| `return` | 2575 | 13.96 | 17.87 | 79.50 |
| `PyObject_GenericGetAttr\|PyUnicode_DecodeUTF8` | 101 | 6.17 | 7.70 | 51.70 |
| `.*PyObject_GenericGetAttr` | 79 | 5.32 | 7.03 | 51.79 |
| `pyobject_genericgetattr` (`-i`) | 79 | 6.12 | 7.21 | 51.13 |

The CPython broad-query median fell from 51.49 ms at the overlapping-HIR stage to 13.96 ms in the final run. That is a sequence of experiments, not a fully randomized old/new binary A/B trial. Smaller differences should be treated cautiously; 15 samples do not establish p99 behavior or statistical significance for every comparison.

**Tradeoffs and losses:** tgrep still wins most direct-mode cases. FXI's final direct medians are 13.55–25.80 ms on Redis and 26.53–61.11 ms on CPython; tgrep's selective/absent direct cases are much faster. FXI does better on the broad direct query. These are warm-filesystem startups, not cold-disk measurements.

| Resource | Redis FXI / tgrep | CPython FXI / tgrep |
|---|---:|---:|
| Index bytes, decimal MB | 10.86 / 15.50 | 40.55 / 50.60 |
| Single build observation, seconds | 0.161 / 0.110 | 0.449 / 0.458 |
| Build peak RSS, MiB | 174.5 / 75.9 | 401.8 / 127.4 |
| Server RSS after final case, MiB (`ps`) | 33.5 / 41.8 | 103.2 / 120.3 |

The single build observations do not establish a throughput winner. FXI still uses much more build memory. Server RSS is a point-in-time OS measure, not peak memory or total cache accounting. Each FXI reader caps retained cached text at 64 MiB and 4,096 entries, admits files up to 128 KiB, and revalidates metadata. Active queries and multiple reader generations can retain additional allocations. The corrected Redis baseline ended around 26.7 MiB server RSS: part of the gain spends more memory on reusable text.

Raw evidence: [final Redis](final-redis-results.json), [final CPython](final-cpython-results.json), [remeasured corrected baseline](rechecked-baseline-results.json). Intermediate `*-results.json`, profiles and [thread sweep](thread-sweep.json) retain the experiment history rather than replacing inconvenient results.

## Why the changes helped

1. **HIR planning:** mandatory internal literals, alternation and bounded case alternatives replace prefix-only extraction. Exhausting an expansion budget broadens candidates; it never discards possible matches.
2. **Overlapping constraints:** bounded case expansion initially split `serverassert` across its selective grams. Overlap reduced candidates from 1,098 to 131; initial engine measurements fell from about 12.5 ms to 1.6 ms.
3. **Existence specialization:** provably line-local patterns can scan a whole buffer. A literal with nullable, assertion-free surroundings (`.*needle`) reduces to literal existence. Anchors, empty matches and unsafe cases retain per-line verification.
4. **Measured concurrency:** the Redis sweep favored four read workers over twelve. Source-read task size now limits concurrency separately from indexing; `FXI_SEARCH_PARALLELISM` permits further experiments. This default is a heuristic, not a hardware-independent optimum.
5. **Fresh content reuse:** sharded byte-bounded caches work for parallel verification too. Limiting admission to 128 KiB avoids larger entries repeatedly displacing small source files; the CPython admitted working set fits the existing budget. No result-cache shortcut was reintroduced.
6. **Avoid tiny parallel dispatch:** CPython insensitive lookup spent about 5.7 ms dispatching many small alternative/segment intersections. Keeping up to four segments local reduced the observed lookup phase to about 0.85 ms. This threshold needs larger-scale validation.

The profiling executable reports candidate files/bytes, planning, approximate lookup and end-to-end engine timings. Its lookup evaluator is intentionally restricted to positive regex plans; it is diagnostic, not a replacement executor or an independent correctness oracle.

## Research experiment: correlated substring evidence

The [gram laboratory](../../examples/gram_lab.rs) compares complete trigram postings with:

- Independent 8-bit offset-modulo and following-byte-bucket masks, analogous to established augmented-gram designs (not a reproduction of tgrep's exact implementation).
- A joint 8×8 signature preserving which following-byte buckets occur at which position residues.
- An adaptive version retaining joint evidence only where at least half of the marginal combinations are absent.

First-principles hypothesis: separate masks lose correlation. If `next=A` only occurs at phase 1 and `next=B` only at phase 2, separate masks also permit `(A,2)` and `(B,1)`; a joint signature rejects those combinations. Every real occurrence sets the required bit, so bucket collisions create only extra candidates. Missing/adaptively omitted joint evidence falls back to weaker evidence. End-of-file grams do not require a following byte.

The joint/adaptive schemes are experimental combinations, **not claimed novel**. All representations retain all grams in this lab, unlike production's stop-gram omission. Queries are deterministic sampled source substrings plus mutations and six fixed literals; these are not a representative user-query trace. Adaptive parameters were fixed before measuring CPython, but this is only a second-corpus check, not a statistical generalization study.

Aggregates count repeated candidate file bytes across queries. Payload sizes estimate uncompressed posting fields only; they exclude dictionaries, allocator overhead, actual disk compression and production build/update machinery.

### Redis: 566 queries, zero observed false negatives

| Representation | False candidates | Candidate bytes, GB | Summed median lookup, ms | Estimated posting MB |
|---|---:|---:|---:|---:|
| trigram | 12,256 | 1.516 | 2.575 | 7.99 |
| independent | 3,724 | 1.199 | 2.354 | 11.98 |
| joint | 2,563 | 1.134 | 2.551 | 23.97 |
| adaptive_joint | 2,833 | 1.161 | 3.604 | 14.74 |
### Cpython: 476 queries, zero observed false negatives

| Representation | False candidates | Candidate bytes, GB | Summed median lookup, ms | Estimated posting MB |
|---|---:|---:|---:|---:|
| trigram | 27,476 | 4.577 | 8.364 | 27.80 |
| independent | 8,701 | 3.654 | 7.602 | 41.70 |
| joint | 6,914 | 3.518 | 8.528 | 83.40 |
| adaptive_joint | 7,370 | 3.564 | 10.913 | 51.04 |

**Decision: retain the experiments, do not change the production posting format yet.** Independent masks substantially improve precision in this probe. Full joint signatures save only another ~5.4% of Redis candidate bytes and ~3.7% of CPython candidate bytes while doubling that posting payload. Adaptive allocation reduces the extra payload to roughly 22–23%, but its current branch/selection work is slower and candidate-byte savings are smaller. No end-to-end production speedup follows from these results alone. Fixed execution order, hot in-memory structures and microsecond timer resolution further limit the lookup timing comparison.

### Literature and next hypotheses

GitHub describes sparse, variable-length grams selected using bigram-weight boundaries, and reports that follow masks saturated in its workload. That motivates testing sparse grams rather than assuming wider masks will always work. [GitHub engineering](https://github.blog/engineering/the-technology-behind-githubs-new-code-search/)

Zoekt's positional grams retain offsets and can test appropriately spaced gram pairs, trading more index space for stronger substring evidence. This is a credible alternative for large candidate files, not an exotic technique we need to invent. [Zoekt design](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)

The 2025 evaluation of FREE, BEST, LPMS and related methods shows workload-dependent tradeoffs between construction cost, storage and query-aware selection. It does **not** establish one strategy as universally best for code search. [Zhang et al., PVLDB](https://www.vldb.org/pvldb/vol18/p5703-zhang.pdf)

Cursor describes sparse grams, augmented masks, compact on-disk lookup structures and a base-plus-edit-layer design. These provide additional concrete baselines for experiments. [Cursor engineering](https://cursor.com/blog/fast-regex-search)

We have not reached the limits of this literature. Next experiments should compare fixed grams, sparse grams, positional/block evidence and byte-weighted evidence allocation under the same immutable generation/update protocol. A useful objective is **verified bytes and I/O avoided per index byte and build millisecond**, not candidate count alone. Any proposed new technique needs a candidate-superset argument, adversarial boundary tests, held-out query families, and an end-to-end implementation before calling it an improvement.

## Remaining work and limits

- Direct-open latency: dictionaries and document metadata are still deserialized eagerly. Evaluate mapped/packed lookup structures and lazy optional token/position metadata.
- Builder/compaction memory: file-count chunks and materialized merges still need byte budgets and streaming/external merge experiments. No million-file throughput claim is supported here.
- Freshness: reconciliation still scans metadata and watchers do not provide an immediate in-memory overlay. Deliberately preserved mtime/size can evade index update detection; current-content verification removes stale positives but cannot discover every new match missing from an older index. TUI preview-cache behavior also warrants a separate pass.
- Durability and corruption: generation concurrency regressions pass on macOS, but no exhaustive crash/power-loss fault injection or Windows execution was performed. There are no per-file checksums proving arbitrary bit-rot detection; structural validation is not that guarantee.
- Large output: content/count paths still materialize matches, and the daemon retains its ten-million-result safety cap. Streaming collectors, cancellation, explicit truncation reporting and backpressure remain important.
- Concurrency: no p95/p99 under competing queries, branch switches or continuous edits. No cold-page-cache, Linux or Windows performance results. Four-worker/four-segment thresholds and cache admission need varied hardware and corpus validation.
- Comparison scope: only the pinned tgrep and ripgrep builds were timed. Zoekt, other indexers, and alternative indexing structures have not been benchmarked head-to-head here.

## Validation and reproduction

- `cargo test --all-targets`: 246 unit tests in each of the library/binary targets (duplicated tests, not 492 distinct tests), 12 index regressions, 9 query regressions, 2 parity-grid tests, 70 compatibility tests, benchmark smoke executions and the signature-lab exhaustive test pass.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and `cargo +1.88.0 check --locked --all-targets` pass.
- Extension: 91 tests and `npx tsc --noEmit` pass. No live VS Code UI session was exercised.

```sh
cargo build --release
python3 docs/audit-2026-09-17/benchmark.py --repetitions 15 --output /tmp/redis-results.json
python3 docs/audit-2026-09-17/benchmark.py --source /tmp/fxi-research-cpython --suite python --repetitions 15 --output /tmp/python-results.json
cargo test --example gram_lab
cargo build --release --examples
# Use the controlled corpus path recorded in a benchmark JSON's `base`:
target/release/examples/gram_lab /path/to/controlled/corpus
FXI_INDEXES=/path/to/benchmark/indexes target/release/examples/query_profile /path/to/controlled/corpus
```

The harness expects the depth-one Redis/tgrep clones at the paths stated in its header. Run measurements without concurrent builds/tests. It creates isolated corpus/index/socket directories and terminates only the servers it starts. Source commits and corpus hashes must match before comparing numbers.
