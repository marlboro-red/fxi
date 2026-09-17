# Correctness-fix validation and performance

Machine: Apple M2 Max, 12 logical CPUs, 64 GiB RAM, macOS 26.1 arm64.
Compiler: Rust 1.94.0; minimum-version checks use Rust 1.88.0.
Production code through `be47cd5` (the documentation commit follows).

## Correctness and packaging

- `cargo test --all-targets`: 873 passing executions. This includes 371 shared
  unit tests run through both library and binary targets; it is not 873 distinct
  test cases. Integration, example and benchmark-target smoke coverage also ran.
- Native `cargo clippy --all-targets -- -D warnings`, Windows-target Clippy,
  `cargo fmt --check`, and `cargo +1.88.0 check --locked --all-targets` pass.
- Rustdoc: 2 passing compiled examples, 2 intentionally ignored examples.
- Extension: 103 tests, TypeScript checking and bundle build pass. A VSIX was
  built and its README, license and PNG/SVG assets inspected.
- Benchmark harness regression suite: 3 tests pass.
- Native Linux/Windows execution is delegated to repository CI. Cross-target
  checking is not evidence that native pipe/watcher behavior has run locally.

Regressions cover malformed queries followed by a healthy daemon Ping, corruption
without partial publication, failed reads and restoration, shutdown producer
barriers and busy writers, queued events after removal, preview-preserving reload,
ranked limits above 100, file/subtree scope, CLI JSON/NUL/context/broken pipes,
a controlling-PTY startup failure, and source edits between verification/rendering.

## Controlled before/after measurements

The pre-fix baseline hash matches the executable in the original query audit.
The candidate contains all fixes plus readiness polling and parallel validation.

| Binary | SHA-256 |
|---|---|
| Pre-audit baseline | `8879d77ae4b2c6350bf33b1a071bda9dde417d2ff36b3451d6e1cc9107a7c6a2` |
| Fixed candidate | `cfd35d9848e59b18387fb7e143fd6b796b903699f1580b978f96386aa6c06538` |

Corpus: the existing [65,284-file common Linux source corpus](../performance-round4/common-corpus.json),
with its same 33-segment index. These measurements do not cover every file in the
upstream Linux tree. Each query had 17 interleaved, deterministically shuffled
trials; every returned file set was checked against ripgrep. No compiler/test
processes ran during timing. These are process/CLI timings, with warm filesystem
caches, **not** cold-storage measurements or daemon API-only latency.

### Normal 33-segment layout

Milliseconds, medians:

| Query | Direct before | Direct fixed | Warm CLI before | Warm CLI fixed |
|---|---:|---:|---:|---:|
| `auditNonexistentSymbol94283` | 11.16 | 39.75 | 3.88 | 4.10 |
| `folio_wait_bit_common` | 12.85 | 41.87 | 4.35 | 4.43 |
| `struct file_operations` | 24.25 | 53.22 | 10.24 | 10.43 |
| `return` | 87.46 | 115.96 | 52.69 | 53.68 |

Raw samples: [direct startup](fixed-startup.json), [warm CLI](fixed-warm.json).
Warm results are close to baseline for these four queries; this is not a claim of
improved warm throughput. Direct startup is slower because every gram payload is
now validated instead of accepting malformed/partial lists. That cost remains.

The first daemon request also initializes and validates tokens/positions: the
single observed initial absence request was 18.36 ms before versus 171.54 ms fixed.
This is one first-request observation, not a distribution. Resident memory after
that probe was about 227 MiB versus 627 MiB, and after the broad warm query about
1,169 MiB versus 1,550 MiB. Validation touches mapped postings that previously
could remain unfaulted. RSS includes reclaimable mapped pages and is not allocated
heap, but it is still a real measured resident-memory cost.

### Fix-induced regressions caught and addressed

The initial shutdown-safe accept loop slept 10 ms after an empty accept. This
raised short warm CLI searches from roughly 4 ms to 8 ms. Readiness polling wakes
immediately for a new connection while still bounding idle shutdown detection.
[Screening samples](screening-warm.json) retain that rejected implementation;
[final warm samples](fixed-warm.json) show the correction.

Parallel payload validation changes neither the checks nor the accepted format.
It has little benefit when many segments already load in parallel, but is useful
for a compacted base segment. The isolated comparison below uses the same
compacted index in both variants, comparing **serial strict validation** with
**parallel strict validation**; it is not a pre-audit speedup claim.

| Query | Serial strict validation | Parallel strict validation |
|---|---:|---:|
| `auditNonexistentSymbol94283` | 171.64 ms | 32.85 ms |
| `folio_wait_bit_common` | 174.61 ms | 35.70 ms |
| `struct file_operations` | 191.76 ms | 54.47 ms |
| `return` | 270.29 ms | 133.20 ms |

[Compacted raw samples](parallel-validation-compacted.json),
[33-segment validation comparison](parallel-validation-startup.json), and
[initial direct screening](screening-startup.json). A regression corrupts a late
payload and dictionary ordering in the parallel branch, requiring errors.

### Watcher smoke measurement

Five isolated synthetic runs per binary, 256 probe files, 12 edits at 25 ms
intervals, default watcher settings. This is a small sequential before/after
check, not a statistically strong large-corpus comparison. Exact create/edit/delete
results and post-shutdown persistence were checked for every run.

| Metric (median) | Before | Fixed |
|---|---:|---:|
| Start of burst to complete visibility | 349.92 ms | 342.62 ms |
| Last edit to complete visibility | 22.75 ms | 20.82 ms |
| New file first visible | 25.16 ms | 23.87 ms |

[Before samples](fixed-freshness-before.json), [fixed samples](fixed-freshness-after.json).
The burst duration is largely the intentionally spaced edits. Five runs do not
establish p95/p99 behavior under large repositories, sustained bursts or contention.

## Reproduce

Use the recorded corpus/index or rematerialize the same common corpus. Save both
release executables before rebuilding; the scripts record executable/harness hashes.

```sh
python3 scripts/compare-startup.py --corpus CORPUS --indexes INDEXES \
  --baseline BEFORE --candidate AFTER --repetitions 17 \
  --patterns auditNonexistentSymbol94283 folio_wait_bit_common 'struct file_operations' return \
  --output startup.json
python3 scripts/compare-warm-files.py --corpus CORPUS --indexes INDEXES \
  --baseline BEFORE --candidate AFTER --repetitions 17 \
  --patterns auditNonexistentSymbol94283 folio_wait_bit_common 'struct file_operations' return \
  --output warm.json
python3 scripts/benchmark-freshness.py --tool fxi --binary AFTER \
  --repetitions 5 --burst-edits 12 --output freshness.json
```

For the compacted comparison, copy the index into an isolated `FXI_INDEXES`
directory and run `fxi compact CORPUS` before timing both variants against it.
Do not mutate an index between paired queries. Larger context/memory, build,
content/count and multi-tool benchmarks remain needed; these correctness repairs
do not establish a new overall ranking against other tools.
