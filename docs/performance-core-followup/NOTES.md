# Core weaknesses and usability follow-up

This follows the [post-audit comparison](../performance-after-audit/NOTES.md).
Same Apple M2 Max/macOS host and 65,284-file common Linux-source corpus; filesystem
caches are warm. Compilers/tests were stopped during timing. Ordinary desktop
background activity remained. These results do not establish cold-storage,
concurrent-client or cross-platform performance.

## Avoid unused daemon token loading

Daemon query readers previously opened token dictionaries, postings and positions
eagerly, including for queries whose plans use only grams and source verification.
They now share the CLI's fallible, dependency-aware loading. Core documents,
paths and complete gram payloads still validate before searching. Public reader
constructors and compaction remain eager. Token-dependent operations validate
before access; generation leases preserve deferred files through updates/GC.

Seven interleaved fresh-daemon trials per binary/query, each followed by five
warm requests. Every answer matched an independent ripgrep file set. Process
startup/socket binding are excluded; the first request includes index opening.
RSS is resident memory after that first request, not peak allocation or disk use.

| Query | First response before/after, ms | RSS before/after, MiB | Warm response before/after, ms |
|---|---:|---:|---:|
| Absent symbol | 147.950 / 33.926 | 626.5 / 230.2 | 0.381 / 0.353 |
| `struct file_operations` | 187.121 / 72.829 | 858.4 / 462.0 | 5.892 / 5.879 |
| `return` | 1419.647 / 1286.189 | 1542.3 / 1145.2 | 37.700 / 37.712 |

[Raw observations and executable hashes](daemon-load.json),
[harness](../../scripts/compare-daemon-load.py). The candidate deliberately retains
the old executor, isolating reader loading from subsequent phrase optimizations.
Its other CLI/certificate changes are not exercised by these API requests.
This saves about 397 MiB on this index; a future token-dependent query would pay
the deferred initialization cost. Broad first-query content-cache population
remains expensive.

## Consistent fast paths for equivalent phrase searches

[The separate paired experiment](../performance-phrase-preparation/NOTES.md)
compares binaries differing only in the executor. Quoted phrases now prepare
their verifier once and use the same safe literal/position acceleration as
equivalent regexes. Nonbinding file limits no longer force sorting all false
positives before verification. Binding limits retain lexical result semantics.

The quoted `"struct file_operations"` request improved 65.563 → 4.942 ms; its
equivalent regex improved 5.761 → 4.578 ms. The 13× quoted improvement does **not**
mean the prior regex-based comparison against Zoekt improved 13×. Regex broad
controls were effectively unchanged. Regression coverage includes case/Unicode,
empty/newline phrases, limits/order and edits/deletion of cached source.

## Faster complete posting validation

For a segment with contiguous document IDs, strictly positive deltas and checked
cumulative addition prove that all IDs lie between the first and last. The
validator now checks those endpoints and processes eight one-byte deltas at once
when possible. Sparse document sets retain per-ID membership checks; malformed
varints, zero deltas, overflow and frequency mismatches still fail closed.

In 17 interleaved paired direct searches, absent-query latency fell 39.723 →
33.856 ms on the 33-segment index, and 32.057 → 25.772 ms on the compacted index.
Selective, phrase and broad controls also improved in both layouts. These are
end-to-end process timings, including startup, rather than decoder microbenchmarks.
[Implementation proof, differential tests and all raw samples](../audit-2026-09-18/batched-validation.md).

## Final warm API check against Zoekt

Final combined executable, the same pinned Zoekt builds/indexes, 11 interleaved
requests per pattern with exact ripgrep parity. Native API connection, transfer
and JSON decoding are included; CLI startup is excluded. Case-sensitive regex
queries and complete files-only answers match the preceding comparison.

| Query | FXI, ms | Zoekt stock, ms | Zoekt paths adapter, ms |
|---|---:|---:|---:|
| Selective symbol | 0.848 | 0.800 | 0.612 |
| Absent symbol | 0.313 | 0.319 | 0.140 |
| `struct file_operations` | 4.735 | 5.701 | 1.671 |
| `return` | 38.303 | 212.712 | 61.300 |
| Symbol alternation | 0.897 | 1.147 | 0.921 |
| `.*folio_wait_bit_common` | 0.607 | 3.768 | 3.623 |

[Raw results, provenance and executable hashes](warm-api.json). The paths adapter
uses unmodified Zoekt search internals with comparable compact output. Its phrase
advantage remains about 2.8×; FXI leads broad results by about 1.6× and the
internal-literal case by about 6×. Sub-millisecond differences deserve caution.
The stock broad API transfers much more data, so that ratio is not engine-only.
Direct competitor timings, builds and storage were not rerun in this final check.

## Correctness and usability fixes

- Old negative-routing certificates certified weaker validation rules. Their
  validation epoch is now rejected, forcing ordinary validation; a regression
  covers matching old stamps over malformed payloads.
- `fxi -e PATTERN PATH` now treats the lone positional as a search scope. It
  previously became another search term and could return files outside that scope.
- `--pattern` is the clear long spelling of `-e`; `--regexp` remains a legacy
  alias and does not select regex mode. Use `--regex` to select that mode.
- Search flags without a pattern fail with guidance, including explicitly passed
  default values. Search flags on unrelated subcommands no longer disappear.
- Explicit interactive search checks for a terminal before initializing it.
  Missing indexes and inaccessible search paths have actionable errors.

## Remaining structural costs

On the existing 33-segment fixture, token dictionaries/postings/positions total
about 373 MiB, roughly 60% of the index excluding optional source packs. Current
CLI query plans do not use those token plans; public token APIs still exist.
Lazy loading saves runtime work, not these disk/build costs. An optional smaller
index format needs explicit capability/fallback semantics and build/update/
compaction tests before removing that evidence.

Optional source copies account for another 1,251 MiB. They accelerate broad
direct searches but have an explicit storage tradeoff. Gram validation still
costs startup time; certifying it safely requires versioned validation evidence
bound to exact document membership and immutable file identities. Inherited
hard-link ctime changes must invalidate evidence, even when inconvenient.

Warm phrase searches still pay for checking current source metadata. Broad
initial cache fills, global request admission/cancellation, retained context
snapshots and publication/compaction stalls remain separate open problems.
These changes do not establish universal superiority over other search tools.

## Validation

The combined production changes pass 888 Rust test executions (`--all-targets`,
including 377 unit tests exercised in both library and binary), two doctests,
three benchmark-harness tests, formatting, strict native and Windows-MSVC
cross-target Clippy, and Rust 1.88 checks. The encoding tests compare against a
scalar reference over exhaustive short streams, every byte/lane, randomized
mixtures, sparse sets, overflow and noncanonical varints. Native CI is checked
separately after pushing; cross-compilation is not a native runtime test.
