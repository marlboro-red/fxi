# Round four: broader comparisons and cache experiments

Measurements are on the same M2 Max host and controlled Linux source fixture as
rounds two and three. These are warm-filesystem results, not cold-storage or
cross-platform claims. Every timed search must return the exact ripgrep file set;
duplicate paths, command errors, daemon fallback and incomplete server searches
are rejected. Compilation and test runs are kept outside timing runs.

## Competitors and reproducibility

`provenance.json` records shallow (`--depth 1`) source revisions for Zoekt and
Google codesearch, and the verified official Go toolchain archive hash. The
competitor sources are unmodified. Zoekt indexing disables ctags because the
comparison is text search, raises file-size and trigram-count limits to cover the
same fixture, and otherwise uses its default shard size and four indexing workers.
Build logs, commands, binary hashes, source manifest hashes and raw timing samples
are retained. Comparison commands are in the benchmark scripts.

`compare-indexers.py` checks full corpus coverage after every fresh build before
allowing a tool into query timing comparisons. All tools receive explicit
case-sensitive content queries and return complete matching-file sets. Zoekt's
`type:file` query selects files-only execution, rather than extracting all line
matches and then discarding them for display. The current 65,287-file fixture is
below its CLI's default match caps; equality checks remain mandatory.

## Full-fixture screening

`linux-indexers-screening.json` contains one build and seven interleaved direct
samples per query. FXI and Zoekt cover all 65,287 files. The first csearch coverage
probe used the empty pattern, which hits an upstream special case that searches
an empty reader and returns no filenames in this revision. That probe is marked
invalid in the raw record. A corrected `^` probe finds 65,284 files, missing three
Python files. The original invalid missing-path list is abbreviated and its count
preserved. csearch was excluded from this screening's search timings.

The separate common fixture removes exactly those three reported exclusions.
`common-corpus.json` retains their names; `materialize-common-corpus.py` reproduces
the subset. This does not erase the full-corpus coverage gap or make it an FXI
victory by default. It lets unmodified implementations search identical bytes.

Screening observations, not repeated-build conclusions:

- Build: FXI 4.399 s; csearch 9.453 s; Zoekt 109.256 s.
- Peak build RSS: FXI 333.8 MiB; csearch 281.3 MiB; Zoekt 1,149.4 MiB.
- Index footprint: FXI 626.6 MiB; csearch 72.7 MiB; Zoekt 3,240.7 MiB.
- Selective direct query: FXI 13.749 ms; Zoekt 56.706 ms.
- Broad `return` direct query: **FXI 442.802 ms; Zoekt 138.921 ms**.

These are different index capabilities: csearch stores file-level trigram
candidates, FXI also retains token/position data, and Zoekt stores content and
positional information. The storage and construction differences are real, but
must not be described as identical internal work.

## Repeated common-corpus comparison

`linux-common-indexers.json` uses the accepted FXI changes through `2fc82df`,
65,284 identical files, three fresh builds per tool, and eleven interleaved query
samples. All tools pass complete corpus coverage and exact matching-file parity.
Build medians and index sizes:

| Tool | Build seconds | Peak build RSS MiB (median) | Index MiB |
|---|---:|---:|---:|
| FXI | 4.532 | 348.2 | 626.6 |
| csearch | 9.741 | 292.0 | 73.0 |
| Zoekt | 110.774 | 1161.0 | 3239.8 |

Direct CLI query medians, milliseconds:

| Query | FXI | csearch | Zoekt |
|---|---:|---:|---:|
| Selective | 13.066 | 15.809 | 55.659 |
| Absent | 11.494 | 4.228 | 55.654 |
| Phrase | 45.222 | 375.933 | 59.441 |
| Broad `return` | 592.705 | 1611.341 | 138.605 |
| Alternation | 14.304 | 34.423 | 57.201 |
| Internal literal | 13.390 | 16.077 | 59.234 |

FXI builds 2.15x faster than csearch and 24.4x faster than Zoekt here. But csearch
uses an 8.6x smaller index and answers the absent query 2.7x faster; Zoekt answers
the broad direct query 4.3x faster. These are separate copied-fixture results,
not a before/after comparison against the screening. The warm content-cache
improvements do not benefit one-shot readers, which deliberately bypass that cache.

## Separate search work from API payloads and query syntax

`compare-indexer-servers.py` measures native API requests with a new connection,
including transfer and JSON decoding, but excluding CLI process startup. The
stock Zoekt API returns considerably more metadata: about 18.4 MB for the broad
file list versus FXI's 1.64 MB. Those API totals are not engine-only timings.

`scripts/zoekt-path-server/main.go` is a small adapter around the unmodified Zoekt
search library. It returns paths through the same length-prefixed JSON transport
as FXI. Build it from the pinned Zoekt checkout:

```sh
go build -o /tmp/fxi-research-bin/zoekt-path-server /path/to/fxi/scripts/zoekt-path-server/main.go
```

The adapter source and binary hashes are recorded. It does not alter indexing or
search internals. Both adapter and stock API runs are retained. Partial-load
responses with Zoekt's `Crashes` counter are retried only during startup; any
incomplete response in a measured query fails the run.

The first server probes appended equivalent `(?:)` groups to avoid accidental
whole-result reuse. This exposed syntax-sensitive planning in Zoekt, so those
runs are explicitly marked `pattern_variants: true`; they must not stand in for
ordinary literal queries. Canonical, unchanged queries are measured separately.
All three paths still must return exactly the same files for every sample.

Eleven canonical API samples (`linux-indexer-server-canonical.json`):

| Query | FXI API ms | Stock Zoekt API ms | Zoekt paths adapter ms |
|---|---:|---:|---:|
| Selective | 0.810 | 0.872 | 0.605 |
| Absent | 0.248 | 0.326 | 0.148 |
| Phrase | 9.862 | 5.740 | 1.617 |
| Broad `return` | 45.486 | 200.125 | 59.283 |
| Alternation | 1.123 | 1.266 | 1.002 |
| Internal literal | 0.601 | 3.885 | 3.420 |

FXI is not the universal winner. Zoekt's phrase and some small-query advantages
survive comparable path-only transport; its broad one-shot advantage is also
real on this fixture. These measurements identify verification and startup as
separate targets rather than attributing every difference to posting lookup.

The final common-corpus run, using the accepted binary and eleven canonical
samples (`linux-common-servers-final.json`), confirms the same mixed picture:

| Query | FXI API ms | Stock Zoekt API ms | Zoekt paths adapter ms |
|---|---:|---:|---:|
| Selective | 0.918 | 0.855 | 0.637 |
| Absent | 0.279 | 0.329 | 0.141 |
| Phrase | 9.615 | 5.707 | 1.593 |
| Broad `return` | 51.520 | 204.446 | 58.968 |
| Alternation | 1.224 | 1.155 | 0.962 |
| Internal literal | 0.613 | 3.682 | 3.507 |

Zoekt remains about 6x faster on the warm phrase through comparable path-only
transport. FXI's broad and internal-literal advantages survive that comparison;
the much larger stock-API broad gap includes its larger response payload.

## Prepare regex verification once

`f29c3a4` holds the compiled regex and, where proven sound, its literal finder
once per files-only query. Previously each candidate repeated a shared cache
lookup and finder construction. Twenty-one paired warm Linux samples: `return`
62.578 -> 59.947 ms, phrase 14.803 -> 14.429 ms, with tiny searches essentially
unchanged (`linux-prepared-regex-warm.json`). This is a modest improvement, not a
new indexing algorithm. Full tests, strict Clippy and Rust 1.88 checks pass.
Regression coverage compares regex results with a per-line oracle across anchors,
empty matches, Unicode, CRLF, line filters, limits and multiple segments.

A further experiment prepared case-insensitive regexes for bare identifiers,
avoiding whole-file ASCII lowercasing. Initially it showed little end-to-end benefit:
`return` 485.687 -> 482.553 ms; `struct` 591.152 -> 566.238 ms, with a small
selective-query penalty. It was initially rejected; the patch and raw samples are
retained (`prepared-literal-experiment.patch`, `linux-prepared-literal-warm.json`).
The bare-identifier Unicode oracle tests are retained for subsequent experiments.

## Remove the cache budget cliff

`2d864f8` changes oversized scans from complete cache bypass to non-evicting
admission: reuse validated hits and fill spare capacity, but never displace other
entries. Normal scans retain LRU eviction. Byte, entry and per-file caps remain
unchanged; admission is rechecked under the insertion lock. Changed files still
invalidate their cached snapshots.

The profile in `linux-cache-footprints.json` explains the cliff: insensitive
`return` has 1,083,096,516 cacheable candidate bytes, only **0.87% above the 1 GiB
budget**. The old policy reread everything on every query. Twenty-one isolated
paired samples improve from **494.481 to 117.950 ms**, a **4.2x** speedup
(`linux-scan-admission-isolated-return.json`). In the separate sequence starting
with `struct`, that query improves 586.959 -> 125.543 ms and subsequent `return`
488.393 -> 138.466 ms (`linux-scan-admission-warm.json`). Regex controls are
essentially unchanged (`linux-scan-admission-regex.json`).

This uses more of the existing cache allowance: process RSS after `struct` rises
from about 328 to 1,444 MiB. Retained text has a 1 GiB cap, but process RSS includes
indexes, allocations and other state. Its first query also regresses from 633 to
698 ms while filling the cache. These are warm-query gains with real memory and
initial-admission costs. A cache already occupied by unrelated content need not
admit the whole new working set.

Regression tests force shard collisions and concurrent admissions into a tiny
cache, verify entry/byte bounds and no eviction, and cover same-size edits,
oversized replacements, immutable snapshots, invalid UTF-8 and deleted files.
Full tests, strict Clippy and Rust 1.88 checks pass.

## Retest the matcher after removing the I/O bottleneck

`2fc82df` adopts the previously rejected prepared insensitive matcher after a
second experiment against the new cache policy. Twenty-one paired samples in
`linux-combined-literal-warm.json` improve `return` **119.916 -> 91.391 ms** and
`struct` **166.212 -> 115.798 ms**. Selective search regresses 5.763 -> 5.971 ms;
absent search is essentially unchanged. These runs use the same bare identifiers
and independent case-insensitive fixed-string ripgrep file sets on every sample.

The optimization now avoids whole-file ASCII scans, lowercasing and allocation
on resident content. This is evidence that experiments rejected under one
bottleneck should be retested when that bottleneck changes. It is not evidence
of a novel indexing algorithm. The combined implementation passes all targets,
strict Clippy and the Rust 1.88 check, including the Unicode folding oracle.

A final isolated before/after run compares the original round-four binary
(`8744546`) with all three accepted changes (`2fc82df`): bare `return` improves
**427.083 -> 90.213 ms**, **4.73x**, across twenty-one paired samples
(`linux-final-isolated-return.json`). This is the directly measured combined
gain; do not multiply speedups from separate runs. `validation.json` records
680 passing tests, zero failures or ignored tests, and the other validation commands.

## Rejected worker-local regex experiment

The regex crate documents a possible synchronization cost when multiple workers
share one compiled matcher. `worker-regex-experiment.patch` clones its mutable
search state per Rayon task while sharing compiled data. It passes all tests,
strict Clippy and the Rust 1.88 check, but twenty-one paired samples show no
useful gain: `return` 88.521 -> 89.115 ms, `struct` 127.524 -> 126.114 ms,
selective 6.124 -> 6.194 ms, absent 4.425 -> 4.472 ms
(`linux-worker-regex-warm.json`). The production change is reverted; its patch
and measurements remain available. No improvement is claimed from this probe.

## Next experiments suggested by the remaining losses

- **Broad one-shot verification:** test optional compressed source blocks or
  selective positional byte grams. Source blocks exchange disk space and build
  work for fewer small-file reads; positional grams exchange posting size for
  tighter candidate sets. Neither may silently weaken verification of edited
  files. Measure fresh-process latency separately from warm daemon latency.
- **Negative-query startup:** profile dictionary validation, segment fan-out and
  process startup against csearch's 4.2 ms result. A compact routing layer could
  avoid opening irrelevant segments, but must remain conservative and retain
  corruption checks for data actually used.
- **Storage:** extend the earlier offline gram-codec experiment to complete index
  layouts. csearch's 73 MiB result is a stronger storage target than tgrep; report
  which extra FXI features account for space, and benchmark decoding cost.
- **Warm phrases:** measure candidate amplification and verification separately
  against Zoekt's path-only adapter. Current whole-token positions cannot safely
  replace byte-substring semantics; any positional shortcut needs a recall proof.

These are hypotheses, not implemented improvements or novelty claims. No results
here establish superiority over livegrep, indexed ugrep, OpenGrok, or distributed
services, nor under cold storage, simultaneous clients, or other hardware.

## Reproduction commands

Use the pinned revisions and toolchain in `provenance.json`. Clone competitor
sources with `git clone --depth 1`, build their stock executables, and retain the
exact revisions rather than silently comparing different releases. Recreate the
controlled Linux fixture using the earlier reports before running:

```sh
python3 scripts/materialize-common-corpus.py \
  --benchmark docs/performance-round4/linux-indexers-screening.json \
  --output /tmp/fxi-common-corpus --manifest /tmp/fxi-common-corpus.json
python3 scripts/quiet-benchmark.py -- python3 scripts/compare-indexers.py \
  --corpus /tmp/fxi-common-corpus --fxi /absolute/path/to/fxi \
  --go-binaries /absolute/path/to/competitor-binaries \
  --repetitions 11 --build-repetitions 3 --output /tmp/common-indexers.json
python3 scripts/quiet-benchmark.py -- python3 scripts/compare-indexer-servers.py \
  --benchmark /tmp/common-indexers.json \
  --zoekt-server /absolute/path/to/zoekt-webserver \
  --zoekt-path-server /absolute/path/to/zoekt-path-server \
  --repetitions 11 --output /tmp/common-servers.json
```

The stored fixture paths are host-local; update the screening record's `corpus`
path when reproducing elsewhere. All timing scripts write their own JSON files;
do not redirect the quiet wrapper's informational stdout into a JSON result.
Do not build binaries or run tests concurrently with measured work.
