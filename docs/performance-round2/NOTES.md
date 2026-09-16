# Second optimization round (in progress)

The latest accepted implementation is `d6298ca`. Fresh Linux, CPython and Redis
runs show substantial gains in construction and broad searches, with every
measured result checked against ripgrep. Linux's selective one-shot startup gap
is resolved in these samples. Small-corpus absent queries still lose, some warm
query leads are modest, and freshness/feature gaps remain. This does **not**
establish FXI as the user's requested overwhelming winner.

The chronological experiments below include rejected changes and superseded
measurements. Latest full files-only results are in `*-lazy-files-validation.json`.
They cover controlled UTF-8 fixtures with warm OS caches, not cold storage,
every grep feature, every indexing tool, or update latency. The default Linux
builder still uses more peak RSS than tgrep; a measured 1,000-file-segment
configuration reverses that comparison at the cost of a larger index.

## Corpus and protocol

Linux commit `9b87fdc9af2fbfcdb5c24a64139685ef80f6573f`, cloned with depth 1.
The macOS checkout has case collisions. `scripts/materialize-corpus.py` reads Git
objects directly, retains exact eligible blob contents, and explicitly renames
24 colliding paths. Its external manifest records every source and materialized
path, Git blob ID, content hash and size. This is a controlled fixture, not an
unmodified Linux checkout. It contains 65,287 files and 1,311,967,080 bytes.
Manifest SHA-256: `7740ff10d829246f7cd562fb29438ab9df5d5190267ac6dc827b75788595039f`.

The benchmark records binary hashes and revisions, randomizes tool order with
fixed seeds, forces rebuilds, and measures peak process RSS with macOS time.
Build comparisons include tgrep on the same materialized corpus. The initial
Linux query pilot has only three samples per query/tool/mode; use it to locate
weaknesses, not to make precise speedup claims. Server tests use equivalent but
distinct regex strings. Source/OS caches are warm; content caches differ between
tools and their memory cost matters. The tgrep server uses about 1.5 GiB RSS in
this pilot, versus roughly 430 MiB for FXI.

## Observations

- Mapped dictionaries and direct token interning: CPython direct selective
  median 16.40 ms, absent 15.12 ms; tgrep 12.42 and 5.48 ms. Startup remains a gap.
- Linux initial build: FXI 6.58 s / 1,965 MiB peak RSS, tgrep 7.81 s / 271 MiB
  (one sample). FXI index 629 MB versus tgrep 872 MB.
- Linux warm server `return`: FXI 707 ms versus tgrep 184 ms. `struct
  file_operations`: 51 ms versus 20 ms. These regressions relative to the
  smaller-corpus comparisons are important; do not hide them in an aggregate.
- Streaming encoded postings, preserving positions and the on-disk format:
  five builds, FXI median 5.69 s / 1,624 MiB; tgrep 7.96 s / 270 MiB. Individual
  samples and provenance are in `linux-stream-build.json`. The earlier single
  baseline is insufficient to precisely attribute a speedup percentage.

## Algorithm experiments

First eliminate redundant representation costs: whole-posting copies,
per-document temporary position lists, geometric array growth, and queued
complete segments. Then test whether global token-position comparison sorting
can be eliminated by stable inversion of the document-ordered input stream:
count token frequencies, prefix-sum bucket offsets in dictionary order, and
scatter each document's occurrences into its token bucket. This is an application
of established counting/inversion techniques, not a claim of novel sorting.

Integer radix sorting is another candidate, but a lower asymptotic comparison
count alone does not prove a speedup. [IPS4o research](https://arxiv.org/abs/2009.13569)
compares numerous algorithms, distributions and allocation strategies;
[DovetailSort](https://arxiv.org/abs/2401.00710) explicitly addresses duplicate
keys. Our opportunity is to exploit source ordering and dense token IDs so that
we need less sorting at all. Benchmark the real pipeline, including allocation,
index size and query correctness, before adopting an experiment.

Subsequent Linux build experiments (three interleaved FXI/tgrep builds each):

| FXI revision/experiment | FXI median seconds | FXI peak MiB (median) | tgrep seconds | tgrep peak MiB |
|---|---:|---:|---:|---:|
| Exact capacities `648ea00` | 5.25 | 1,397 | 7.98 | 268 |
| Direct handoff `ba20f56` | 5.18 | 1,388 | 7.88 | 266 |
| Token counting inversion `3264b9f` | 4.32 | 1,413 | 7.87 | 266 |
| Trigram counting prototype | 4.15 | 1,539 | 7.88 | 265 |

The trigram prototype is **not adopted**: its small timing improvement does not
justify the increased resident memory here. The exact tested diff is retained
in `gram-counting-experiment.patch`, applying to `3264b9f`; its raw benchmark
records the experimental binary hash. It uses a 64 MiB direct-address table
with touched-key enumeration, preserving document order to avoid sorting all
postings. Full Rust tests and strict Clippy passed before measurement. Demand
paging did not make its real memory cost negligible.

Before balanced scheduling, build file discovery was parallel and segment membership was not deterministic;
index size and peak memory vary across runs. These consecutive experiment
batches are useful for screening, but promising final changes still need an
interleaved old/new binary comparison on the same corpus. The timing gains do
not remove FXI's major construction-memory disadvantage.

| Later experiment | FXI median seconds | FXI peak MiB (median) | tgrep seconds | tgrep peak MiB |
|---|---:|---:|---:|---:|
| Pack strings after scanning `5b96bb1` | 4.31 | 1,573 | 7.84 | 270 |
| Balance source bytes across segments `4d635e4` | 4.30 | 746 | 7.77 | 270 |
| Scan directly into packed storage `872f495` | 4.36 | 653 | 7.80 | 263 |

Packing after scanning failed as a standalone memory optimization. The final
scanner avoids those per-file token String allocations entirely. Hash collisions
in its packed interner are resolved by comparing token bytes; a forced-collision
test and the existing 605-case tokenizer differential corpus cover this path.

The fixed 2,000-file schedule hid enormous size skew: source bytes in the token
inversion build ranged from 10.8 to 308 MiB per segment. Size-balanced scheduling
keeps the same 33 segments at approximately 37.915 MiB each, with deterministic
path-order tie breaking. It performs an extra metadata pass for multi-segment
builds. This is load balancing, not a hard memory bound. It increases this
fixture's index from approximately 630 MB to 691 MB because segment membership
changes dictionary duplication/compression. Query impact must be measured too.

Bypassing content-cache admission for scans exceeding the total entry or byte
budget reduced the Linux pilot's warm `return` time to 576 ms, still far behind
tgrep's 187 ms (`linux-scan-cache.json`, three samples). It preserves owned
snapshots and UTF-8 validation, and avoids evicting useful cache entries with a
scan that cannot fit. This is not a solution to the remaining broad-query gap.

## Additional correctness finding

Persisted Bloom filters previously used aHash. Its [documented contract](https://docs.rs/ahash/latest/ahash/)
allows hash outputs to differ across machines or library versions, even with
fixed keys. Using such a filter to reject segments can hide matches after an
index moves between incompatible builds. The replacement uses versioned,
fixed-width SplitMix64-derived probes ([public-domain reference](https://prng.di.unimi.it/splitmix64.c)),
a payload checksum, and a safe fallback that ignores legacy, unrecognized or
damaged optional filters. This checksum does not extend to other index files.

## Later search changes and benchmark limitations

Earlier screening runs were vulnerable to background Rust analyzer activity.
Their raw samples are retained, but precise causal speedups should not be inferred
from those batches. `scripts/quiet-benchmark.py` pauses only this workspace's
analyzer and its subprocesses, restores them even on failure, and is used for
subsequent decisive runs. Compiles/tests must also be kept outside timing runs.

Retaining common grams by default fixes an avoidable full-corpus scan: the old
stop-gram metadata discarded all four grams in `return` despite storing their
postings. Legacy omitted grams still have conservative fallbacks. Evaluating
compound positive gram plans within each segment avoids repeated global unions
and Rayon dispatches. Both changes preserve candidate completeness.

The daemon now shares one bounded content cache across readers and generations:
1 GiB of text by default, 131,072 entries, configurable with `FXI_CACHE_MIB`.
One-shot CLI readers bypass admission. Entries still validate file metadata on
use, and source reads remain owned snapshots. tgrep's pinned server also uses a
1 GiB text cache but does not perform FXI's metadata validation on each cache hit;
these are pipeline comparisons, not isolated index algorithm comparisons.

Moving response data instead of cloning it and negotiating a compact paths-only
RPC reduce broad-query result costs. Old clients retain the old response shape;
new clients can fall back to old servers. These do not cache complete results.

`linux-system-screening.json` contains seven samples per query and mode at
`951922d`, all checked against ripgrep. It was **not analyzer-isolated**, so it is
screening evidence only. Warm server medians (FXI/tgrep milliseconds):

| Query | FXI | tgrep |
|---|---:|---:|
| Selective | 6.18 | 7.91 |
| Absent | 5.39 | 6.74 |
| Phrase | 14.88 | 20.51 |
| Common `return` | 99.99 | 189.48 |
| Alternation | 6.58 | 13.12 |
| Internal literal | 6.05 | 7.82 |
| Case insensitive | 7.33 | 9.52 |

Direct selective startup still loses (28.13 versus 18.39 ms); direct broad
`return` wins (388.53 versus 1805.37 ms). These seven regexes on one machine
cannot establish superiority across tools, workloads, encodings or update rates.

## Allocator experiment (not adopted)

[mimalloc](https://github.com/microsoft/mimalloc) addresses allocation locality
and contention ([research report](https://www.microsoft.com/en-us/research/wp-content/uploads/2019/06/mimalloc-tr-v1.pdf)).
We tested optional mimalloc 0.1.52 against the saved system-allocator binary from
`951922d`. The exact diff is `mimalloc-experiment.patch`; full tests and strict
Clippy passed with the feature. `linux-allocator-comparison.json` records three
interleaved, analyzer-isolated builds of each, and every built index enumerated
all 65,287 fixture files.

System build times: 4.265, 4.416, 4.250 s; peak RSS: 995.6, 980.9, 876.2 MiB.
Mimalloc: 4.193, 4.307, 4.310 s; peak RSS: 750.5, 803.6, 886.4 MiB.
There is no meaningful speed win and RSS savings vary. The dependency and
allocator change are excluded from the default build. Representation changes
remain the more promising route to a substantial memory reduction.

## Compressing postings during construction

The [SPIMI construction approach](https://nlp.stanford.edu/IR-book/html/htmledition/single-pass-in-memory-indexing-1.html)
and [incremental compressed indexing research](https://arxiv.org/abs/1305.0699)
suggest avoiding global occurrence tuples rather than merely sorting them
faster. This is established algorithmic direction, not a novelty claim.

`56121af` groups only one file's token positions at a time and appends compressed
document/count/position deltas to each token's stream. It eliminates the full
segment's 12-byte occurrence triples. Duplicate local token strings, unordered
input positions, repeated positions, missing positions and large deltas are
covered by a full-sort/decode differential regression. Full Rust tests and
strict Clippy passed. All 233 Linux index data files match the preceding binary,
except the six-byte Bloom prefix changed by the separate compatibility fix;
metadata timestamps and leases were excluded. Index size is unchanged.

Five interleaved analyzer-isolated builds (`linux-compressed-positions.json`):

| Binary | Median build seconds | Median peak RSS MiB |
|---|---:|---:|
| Before (`951922d`) | 4.365 | 769.5 |
| Direct compressed positions (`56121af`) | 4.355 | 477.0 |
| tgrep | 7.866 | 266.0 |

Every resulting index enumerated all 65,287 fixture files. This is a substantial
representation-memory reduction without a meaningful speed change. It does not
yet erase tgrep's builder-memory advantage.

A follow-up experiment compressed token document lists directly too. Five
interleaved isolated runs (`linux-compressed-tokens.json`) showed 4.431 s /
494.8 MiB versus 4.394 s / 474.9 MiB for compressed positions alone. It was
**reverted**: less intermediate data does not automatically mean lower RSS or
faster execution when allocation and concurrent producer overlap change.
`direct-token-postings-experiment.patch` preserves the exact tested change
against `56121af`; full tests and strict Clippy passed before measurement.

## Counting exposes a different bottleneck

The harness now supports `--output-mode count`, checks every per-file count
against ripgrep, detects duplicate output records, and rejects explicit daemon
fallbacks. Files-only queries stop on their first hit; their results cannot be
used as a proxy for count/content performance.

`linux-count-screening.json` measured the preceding `56121af` binary on the
same Linux fixture. Its nominal warm `return` count (2.97 s) was **not a valid
successful-daemon measurement**. A focused repeat captured `Message too large`
followed by direct fallback on both attempts
(`linux-count-baseline-fallback-check.json`). The server had serialized all
matching lines even though the user only requested counts, exceeding the
100 MiB protocol limit. The earlier harness checked final answers but missed
the retry. The corrected harness rejects this condition.

`cfd7ca1` negotiates per-file counts directly, retaining old-client and old-server
compatibility. Regex counting avoids retaining line text; other query operators
preserve the previous verification semantics with per-file reduction. Global
CLI limits retain path order, and unlimited count responses need no per-match
record cap. Tests compare aggregate counts against full content results across
sequential/parallel fixtures, limits, CRLF, Unicode, repeated words, blank lines,
filters and edits. Protocol defaults/roundtrips and daemon compatibility are
covered. Full Rust tests, strict Clippy, Rust 1.88 checks and all 91 extension
tests passed.

Five isolated samples per query/mode in `linux-aggregated-counts.json` show:
`return` direct 754 ms versus tgrep 1,930 ms; successful daemon 442 ms versus
2,423 ms. Every file count matched ripgrep, with no fallback. However, warm
phrase counting remained 105 ms versus tgrep 24 ms. Reducing output alone does
not solve unnecessary per-line regex calls; that is the next experiment.
After the common count query, observed daemon RSS was about 1.17 GiB for FXI and
5.05 GiB for tgrep; this is a point-in-time process measurement, not peak heap
usage or the configured content-cache size.

`4468740` counts by jumping from each match to the next line when the HIR proves
that whole-buffer existence is equivalent to per-line existence. Exact literal
existence uses SIMD substring search; other proven line-local regexes use regex
search from the next line. Empty, anchored, context-sensitive and line-filtered
queries keep the per-line path. A 3,906-string × 24-pattern differential test
compares this with independent per-line regex counting; full tests, strict
Clippy and Rust 1.88 checks pass.

Five isolated samples, every per-file count checked against ripgrep, no daemon
fallback (`linux-line-jump-counts.json`):

| Count query (warm daemon) | FXI ms | tgrep ms |
|---|---:|---:|
| Selective | 5.97 | 7.89 |
| Absent | 5.41 | 7.47 |
| Phrase | 14.52 | 23.35 |
| Common `return` | 103.52 | 2,431.42 |
| Alternation | 6.60 | 13.71 |
| Internal literal | 5.77 | 8.41 |
| Case insensitive | 7.54 | 9.34 |

Direct phrase counting is 61.85 versus 95.08 ms and direct `return` counting is
427.57 versus 1,933.30 ms. Selective direct startup still loses: 26.84 versus
18.52 ms. The approximately 23× warm common-count win is a real improvement for
this workload, not evidence of universal dominance or novel indexing theory.
It includes engine, allocation, serialization and CLI costs. Observed daemon RSS
after the common count was 1.16 GiB for FXI versus 5.02 GiB for tgrep.

## A compact rank map for gram construction

`75d5759` replaces the full gram/document tuple array with a static rank map:
2 MiB membership bitset over the 24-bit byte-gram universe, plus 1 MiB of
per-word population prefixes. A gram's dense rank is its word prefix plus a
masked population count. Two document-ordered passes determine exact compressed
lengths and write deltas into one contiguous posting buffer. Only dictionary
keys need enumeration; there is no global posting sort. Non-byte u32 keys from
public writer callers retain the previous implementation. Duplicate input grams
are deduplicated per document.

This applies standard bitvector rank and direct compression ideas; it is not a
claim of a novel succinct data structure. The alternative is motivated by the
failed 64 MiB direct table experiment, with allocation costs explicitly included.

Five isolated interleaved builds (`linux-rank-compressed-grams.json`):
4.457 s / 328.5 MiB versus 4.378 s / 478.6 MiB before. That is a 31% peak-RSS
reduction with a 1.8% higher median build time in this sample; timings overlap,
so do not call it a speed win. The memory benefit justified adoption. All 233
index data files are byte-identical and every built index enumerated the full
fixture. Differential tests cover duplicates, arbitrary gram order, byte/word
boundaries, empty input, maximal document IDs and the non-byte-key fallback.
Full Rust tests, strict Clippy and Rust 1.88 checks passed.

## Selective startup remains a gap

`7657994` indexes the document vector directly when IDs are consecutive, using a
hash table only for sparse or reordered generations. Tests include empty,
nonzero-starting, sparse, reversed, duplicate and maximal-ID inputs. A fresh
process still performs required index validation; no integrity checks were
removed to obtain the improvement.

31 isolated, randomized interleaved one-shot samples on the same index
(`linux-document-lookup-startup.json`): absent 24.36 → 23.13 ms, selective
25.98 → 24.91 ms. tgrep measured 16.98 and 18.18 ms. This is a modest saving,
not resolution of the startup disadvantage.

`650d341` extends direct counting to plain literals, quoted phrases and boosted
versions, preserving their original case semantics. Five isolated interleaved
one-shot plain `return` count samples (`linux-plain-counts.json`) measured
560.59 → 448.27 ms, versus tgrep 2,078.66 ms. For this separate workload, tgrep
and ripgrep use case-insensitive fixed-string matching to match FXI's plain
literal semantics. Every per-file count was verified. Do not compare these
numbers directly with the earlier case-sensitive regex workload.


## Source-read safety and further startup work

`b10b058` rejects incremental segment-ID exhaustion before starting a writer,
considering both base and delta IDs. Its regression test verifies that the
previous generation and search results survive the rejected update.
`68109c6` reads source metadata and bounded content from the same opened file,
preventing path replacement between metadata collection and opening a different
file. Reads stop at the size limit plus one byte, reject growth beyond the limit,
and retain owned snapshots. Tests cover replacement, growth, shrinkage and bounds.
This does not make concurrent in-place edits transactional.

Five isolated interleaved Linux builds (`linux-source-handle-builds.json`) gave
4.448 → 4.421 s and 343.5 → 340.6 MiB peak RSS at default concurrency: no
substantial speed claim. Eight build threads measured 3.941 s / 320.8 MiB.
An earlier 4/8/12-thread sweep gave 5.777/4.093/4.433 s. This is machine- and
workload-specific: a separate strict-reader startup experiment got slower with
8 and then 4 threads. The global Rayon default remains unchanged.

A parallel token/gram inversion prototype measured 4.343 → 4.207 s while peak
RSS increased 336.6 → 386.6 MiB. Rejected: a roughly 3% build speed improvement
does not justify 15% more memory here. The patch and all five raw samples are
retained in `parallel-inversion-experiment.patch` and `linux-parallel-inversion.json`.
The same run measured tgrep at 7.954 s / 266.1 MiB.

`20862e5` maps immutable document/path metadata, decodes bounded records and avoids
an intermediate path allocation. Truncation and legacy-format tests cover the
change. In 31 interleaved one-shot samples, absent search improved 23.213 →
22.557 ms and selective search 24.981 → 24.453 ms. `ea7f1dc` then fused token
UTF-8/order/range validation into dictionary offset construction, retaining
validation while removing a second pass: absent 22.592 → 18.687 ms and selective
24.495 → 20.528 ms. tgrep measured 16.698/18.008 ms in the latter comparison.
Each comparison uses its own paired baseline; do not subtract numbers across runs.

A further file-handle/stat reuse prototype was neutral: absent 19.019 → 18.993 ms,
selective 20.705 → 20.800 ms. It was reverted; patch and samples remain in
`map-handle-experiment.patch` and `linux-map-handles-startup.json`. Its additional
legacy token-record and non-file payload rejection tests were retained in `02cf961`.
All adopted Rust changes passed full tests, strict Clippy and Rust 1.88 checking.


## Load only the indexes a direct query needs

`d6298ca` gives one-shot CLI readers dependency-aware token loading. Gram and
content searches avoid mapping and validating unused token dictionaries and
positions. Public library constructors still eagerly validate the full reader;
any token-dependent query loads and validates token data through a fallible
barrier. Errors propagate through nested query plans. Tests cover concurrent
initialization, complete token/position results, corrupt auxiliary data, nested
plans and required gram-data rejection. Immutable generation leases preserve
files until deferred loading finishes. This deliberately changes when unused
auxiliary corruption is detected; see `docs/SEMANTICS.md`.

31 isolated interleaved samples (`linux-lazy-tokens-startup.json`):

| One-shot query | Before ms | After ms | tgrep ms |
|---|---:|---:|---:|
| Absent | 18.671 | 10.492 | 16.510 |
| Selective | 20.534 | 12.364 | 17.811 |

Every answer was checked against ripgrep. This resolves the sampled Linux
startup disadvantage, not every startup workload or machine. Full Rust tests,
strict Clippy, release compilation and Rust 1.88 checks passed.

The benchmark now distinguishes FXI's exit convention (0 for a successful empty
search; 1 for an error) from grep-style tools (1 for no matches). Earlier
harnesses allowed 1 for all tools, which could mistake an FXI error for an absent
answer. Positive queries and parity checks substantially constrain that risk,
but it was still an inadequate check and is corrected for subsequent runs.


## Refreshed Linux files-only comparison

Five isolated samples per query and mode, current `d6298ca` binary, every sample
checked against ripgrep (`linux-lazy-files-validation.json`). All 14 workload
medians favor FXI. These are warm-storage source-code measurements on one M2 Max,
not evidence that every tool, feature, machine or workload is beaten.

| Mode | Query | FXI ms | tgrep ms |
|---|---|---:|---:|
| direct | selective | 13.80 | 18.42 |
| direct | absent | 12.08 | 16.97 |
| direct | phrase | 49.09 | 95.09 |
| direct | common | 402.96 | 1901.48 |
| direct | alternation | 15.31 | 25.76 |
| direct | internal_literal | 14.21 | 18.64 |
| direct | insensitive | 16.25 | 19.92 |
| server | selective | 6.36 | 7.93 |
| server | absent | 5.54 | 7.65 |
| server | phrase | 14.21 | 20.77 |
| server | common | 97.35 | 186.41 |
| server | alternation | 6.60 | 13.62 |
| server | internal_literal | 6.20 | 7.91 |
| server | insensitive | 7.63 | 8.78 |

FXI index: 657,068,414 bytes; tgrep: 872,213,156 bytes. No daemon fallback was
reported. This run started before the FXI-specific exit-code hardening; subsequent
corpus runs use that additional check. This limitation is retained with the data.


## Segment sizing and codec space experiments

Five isolated interleaved builds with the same `d6298ca` binary and corpus
(`linux-segment-size-sweep.json`), complete file enumeration verified after each:

| Files per segment | Build seconds | Peak MiB | Index bytes |
|---|---:|---:|---:|
| 1,000 | 4.366 | 225.6 | 736,314,771 |
| 2,000 (default) | 4.394 | 342.8 | 657,068,414 |
| 4,000 | 4.369 | 529.6 | 601,582,148 |
| tgrep | 7.923 | 266.9 | 872,213,174 |

Smaller segments reduce peak memory substantially with similar build times,
but increase index size by 12%. Larger segments reduce index size and increase
memory. This is a tradeoff, not a universal optimum; query costs also need checking.

`examples/posting_lab.rs` evaluates actual lists from the immutable Linux index.
A minimum-document-ID plus span bitmap is chosen only when smaller than VByte;
both candidate hybrids conservatively add one codec byte per list. Of 4,217,535
lists, 130,968 would use bitmaps, covering 67,394,907 of 122,496,564 postings.
Payload estimates: current VByte 134,779,665 bytes; bitmap/VByte hybrid
102,302,191 bytes; serialized-Roaring/VByte hybrid 138,997,200 bytes. Raw results
are in `linux-posting-codec-space.json`. Dense, short segment-local sets explain
why small raw bitmaps differ from general Roaring serialization here. No format
change or end-to-end speedup is claimed. Boundary tests, strict Clippy and Rust
1.88 checks pass for the experiment.

This follows established density-adaptive representation ideas. SIMD byte-code
and intersection work remains another candidate, including
[Stream VByte](https://arxiv.org/abs/1709.08990) and
[SIMD compression and intersection](https://arxiv.org/abs/1401.6399).
A faster decoder alone does not establish a faster query: candidate verification,
process startup, metadata checks and output often dominate. We have not reached
the limits of existing literature and should not claim novelty prematurely.

## Cross-corpus validation after lazy loading

Fresh CPython and Redis controlled fixtures, five samples per query/mode and
three builds per tool, use the stricter FXI exit-code check and assert daemon
liveness before and after every timed sample. All matching-file multisets agree
with ripgrep. Raw data: `python-lazy-files-validation.json` and
`redis-lazy-files-validation.json`.

CPython: direct selective 7.27 vs tgrep 12.16 ms; direct common 27.69 vs 119.87 ms;
warm common 9.55 vs 17.50 ms. Direct absent still loses: 5.97 vs 5.10 ms.
Redis: direct selective 5.68 vs 5.67 ms is effectively tied; direct common
13.36 vs 36.04 ms; warm common 5.68 vs 8.74 ms. Direct absent loses:
5.05 vs 4.38 ms. Small warm-query leads are often modest. These counterexamples
rule out the user's requested broad, overwhelming victory at this point.


## Reject sequential small-reader loading; retain the segment-size choice

A prototype avoided Rayon initialization when a gram-only reader had at most
four segments. Full Rust tests, strict Clippy and Rust 1.88 checks passed, but
31 interleaved samples rejected the performance hypothesis. CPython absent
4.983 → 5.288 ms; selective 6.479 → 7.029 ms. Redis absent 4.715 → 4.512 ms;
selective 5.191 → 4.941 ms; common 13.045 → 13.122 ms. A small Redis benefit
is insufficient for the CPython regression. The source was reverted and its
patch/data preserved (`sequential-small-open-experiment.patch`, applying to
`d6298ca`; `python-small-open-startup.json`, `redis-small-open-startup.json`).

Correction: the original `linux-small-segments-startup.json` comparison is
invalid: `compare-startup.py` selected the alternate index for warmup but omitted
it from timed samples, so both timed labels used the default layout. The original
raw file is retained for traceability, not evidence of layout equivalence.
The harness is fixed. A replacement 31-sample interleaved run, using the same
experimental binary on both layouts (both exceed four segments), measured
2,000 → 1,000-file segments: absent **10.301 → 12.445 ms**, selective
**11.845 → 14.207 ms**. tgrep measured 16.044 and 17.329 ms.
See `../performance-round3/linux-small-segments-corrected.json`. The corrected
measurements show a roughly 20% startup penalty for the smaller segments.
The default remains 2,000; users prioritizing
build memory can reproduce the alternative with `fxi index --force --chunk-size
1000 ROOT`. The 1,000-file configuration has not received the full seven-query,
both-mode validation used for the default, and a file-count cap is not a hard
memory bound for arbitrary file sizes.

## Remaining work against the requested standard

- Freshness: newly created or newly matching files await reconciliation/flush;
  no immediately searchable live overlay. Measure update visibility, ignore-rule
  changes and rename/delete storms before claiming parity with tgrep's live index.
- Peak memory and latency under concurrency, compaction and mixed updates remain
  separate from full-build measurements. Segment sizing is a configurable tradeoff.
- Cold-storage tests, additional machines and other competing indexes are still
  needed. The M2 Max measurements cannot establish universal leadership.
- Query/output capabilities still differ: PCRE2, multiline, encodings, invert
  matching and very large full-content responses need explicit treatment.
- Validate any bitmap codec in the real pipeline, including versioning, corruption
  handling, incremental updates, compaction, construction cost and verification.
  A 24% gram-payload estimate saves only about 5% of the whole Linux index.
