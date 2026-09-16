# Second optimization round (in progress)

These measurements do **not** establish FXI as a clear winner. Linux exposes
large construction-memory and initially warm broad-search disadvantages hidden
by the smaller corpora. Later changes reverse the sampled warm-query losses,
but selective one-shot startup and builder memory remain weaknesses. Matching-file sets were checked against ripgrep, including each
measured sample. These are UTF-8 source fixtures and files-only regex workloads;
they do not establish feature parity, cold-cache performance or update latency.

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
