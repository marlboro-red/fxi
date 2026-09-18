# Single-capture indexing

Baseline: `ea6f99b`. Capture implementation: `03db4e0`; Unix fixture/CI follow-up:
`689fb84`. Final compressed read-order adjustment: `5126a70`. Built with Rust 1.94.0 on an Apple M2 Max, 64 GiB, macOS 26.1.
All timings below use warm filesystem caches. These are FXI before/after
comparisons, not new competitor rankings.

## What changed

Postings and optional pack payloads now derive from one owned source read.
Workers stream source payloads to unpublished storage and retain only record
metadata. Detectable changes during the read abort publication. Compaction uses
validated revision-bound captures from input segments; it never rereads live
source to pair new bytes with old postings. Stale captures retain their original
stamps and remain ineligible for live queries.

The new raw/compressed headers mark capture provenance. Legacy packs remain
readable, but their contents are omitted from newly compacted packs. Missing or
corrupt packs likewise use live-source fallback; full rebuilding restores pack
coverage. The underlying source checksums and block encodings are unchanged.

This does not establish a filesystem-wide snapshot or guarantee visibility of
edits after capture. Unpacked source verification, query-local validation,
publication scheduling and streaming postings compaction remain separate work.

## Build measurements

Three alternating builds per variant and configuration, isolated output
directories, exact ripgrep file-set checks after every build. All campaigns use
the same 65,284-file / 1,311,592,608-byte controlled Linux-source corpus and verify
its manifest before and after. Manifest:
`adb3052a8f1cd3f0d7b49c7dff2cc5ad36b831da7a4c238c2f899db85ae57854`.
No local compilation or test suites overlapped these timed campaigns.

| Configuration | Before build | After build | Before RSS | After RSS |
| --- | ---: | ---: | ---: | ---: |
| [Lean, no pack](builds-unpacked.json) | 2.53 s | 2.49 s | 154.66 MiB | 144.12 MiB |
| [Lean, raw pack](builds-raw.json) | 5.24 s | 2.74 s | 322.66 MiB | 165.86 MiB |
| [Lean, compressed pack](builds-compressed.json) | 6.80 s | 3.48 s | 308.27 MiB | 153.62 MiB |
| [Full, compressed pack](builds-full-compressed.json) | 8.43 s | 5.50 s | 340.61 MiB | 317.83 MiB |

Lean compressed builds are 48.9% faster with 50.2% less peak RSS in this sample.
Raw-pack builds improve by 47.8%. Full-profile compressed builds improve by
34.8%. Unpacked times remain close: the extra post-read metadata check has no
substantial measured cost here. Index byte counts are identical before/after in
all four campaigns.

Reproduce with `scripts/compare-index-builds.py`, the saved baseline/candidate
binaries, matching `--baseline-profile` / `--candidate-profile`, and
`--source-pack 0` or `1`. Compressed runs additionally set
`FXI_SOURCE_PACK_COMPRESSION=1`. Reports include commands, binary/harness hashes,
every timing/RSS sample, component sizes, manifest and retained directories.

## Query measurements

21 randomized pairs of direct CLI files-only regex searches, both variants
using lean compressed packs (`FXI_SOURCE_PACK=1`). Exact complete path sets are
checked against ripgrep on every sample. No claim of a search-speed win follows
from the build improvements.

| Query | Before (ms) | Capture implementation (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 33.34 | 33.72 |
| `folio_wait_bit_common` | 36.38 | 37.27 |
| `struct file_operations` | 43.36 | 43.76 |
| `return` | 108.82 | 109.68 |
| `unlikely\(` | 46.02 | 46.20 |
| `Copyright` | 116.89 | 117.61 |
| `return.*0` | 279.67 | 290.36 |
| `return.*[0-9]{12}` | 541.66 | 568.36 |

The harder packed regex samples are about 4–5% slower. This is a measured
regression in this campaign; its cause and statistical significance have not
been established. Streaming changes pack layout from document-ID order to worker
completion order, so a separate locality experiment evaluated ID assignment; it was rejected
as described below. [Raw query samples](startup-compressed.json).

Cold storage, native Linux/Windows performance, and worst-case memory under
different worker counts/file sizes are not measured here. Memory is bounded by
in-flight workers and file limits plus retained posting/record metadata; this is
not a process-wide RSS budget.

## Rejected capture-order experiment

Assigning document IDs in source-payload completion order avoids physically
reordering the pack. The experimental patch is retained in
[capture-order.patch](capture-order.patch), applicable to `689fb84`; it was tested
and then reverted. [Build samples](builds-order-experiment.json) and
[query samples](startup-order-experiment.json) preserve the negative result.

Build medians were 3.44 / 3.46 seconds. The index grew by about 1.8 MiB, from
changes in gram posting encoding. Selective/absent query medians regressed by
roughly 4–5%; the regex with only 28 matching files was essentially unchanged. The experiment
showed no sufficient overall benefit on this warm-cache workload. Cold-storage
locality might behave differently; that is unmeasured and not a shipping claim.

## Final query validation

The final adjustment restores metadata validation before descriptor checksumming:
stale captures are rejected before touching their descriptors. It changes neither
index construction nor document IDs. The build measurements above therefore use
the same construction algorithm as the final code. The original and rejected
query campaigns remain recorded above; this campaign does not isolate the causal
effect of check ordering.

[Final samples](startup-final.json) repeat all eight queries with 21 randomized
pairs against the original baseline, using the same retained indexes. Every
sample matched ripgrep's complete file set.

| Query | Before (ms) | Final (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 33.62 | 34.14 |
| `folio_wait_bit_common` | 35.63 | 35.41 |
| `struct file_operations` | 43.91 | 43.14 |
| `return` | 109.46 | 110.25 |
| `unlikely\(` | 46.28 | 46.13 |
| `Copyright` | 117.15 | 117.42 |
| `return.*0` | 281.40 | 293.68 |
| `return.*[0-9]{12}` | 553.87 | 543.83 |

Most queries are close. `return.*0` still measures 4.4% slower; the 28-match
regex measures 1.8% faster in this campaign, after being slower in the first.
The broad-regex samples have large spreads. This evidence supports the build
improvement, not a general search speedup or resolution of all query regressions.

## Watched update validation

Three alternating rounds of two repetitions per variant (six samples each),
using `scripts/benchmark-freshness.py --tool fxi --repetitions 2 --atomic-save
--edit-source CORPUS/fs/ext4/inode.c`. Both pack environment flags are enabled;
these fixtures use the default full profile, 256 synthetic files and a 206,062-byte
real source edit payload. Baseline and final binaries match the final query report.
Default watcher configuration and native watching are used. Timed runs did not
overlap compilation or tests.

| Median | Before | Final |
| --- | ---: | ---: |
| Complete result visibility | 35.61 ms | 35.99 ms |
| Visibility after last edit | 33.72 ms | 34.24 ms |
| Server RSS after visibility | 13.98 MiB | 14.12 MiB |

All twelve samples passed exact create/edit/retained-match/deletion checks and
graceful-shutdown persistence verification. Visibility includes CLI probing and
is interval-censored; raw reports retain the last incomplete probe and attempts.
Six samples on this small fixture cannot establish tail latency or behavior under
large edit bursts. No update-latency improvement is claimed.

Raw rounds: [before 0](freshness-before-0.json), [after 0](freshness-after-0.json),
[before 1](freshness-before-1.json), [after 1](freshness-after-1.json),
[before 2](freshness-before-2.json), [after 2](freshness-after-2.json).

## Correctness validation

The final production code passes 953 all-target Rust test executions and Clippy
with warnings denied. Regression coverage exercises source mutation around capture,
legacy/current pack mixtures, corruption fallback and compaction provenance.
MSRV and fuzz-target compilation are checked separately. Benchmark equality checks
cover their workloads; they are not a proof of every search option or concurrent
filesystem behavior.
