# Chromium coverage and two-byte search follow-up

This follows the [original Chromium comparison](NOTES.md), on the same M2 Max
and shallow Chromium checkout. It addresses two concrete weaknesses: ingestion
silently excluded visible directory names, and two-byte queries verified every
indexed source file.

## Coverage correction

Full builds and scoped/full reconciliation now use normal ignore-file rules
without unconditional exclusions for `node_modules`, `target`, `venv`, or
`__pycache__`. `.gitignore`, `.ignore`, nested rules, and negations control these
exclusions. Hidden files, encodings, size limits, binary heuristics, and symlink
policy are unchanged. Existing indexes need reconciliation to admit newly
eligible files; upgrading a search executable does not alter their coverage.

Tests cover formerly excluded directory names, a root named `target`, scoped
updates versus a fresh build, ignore negations, and expansion/contraction after
ignore-file changes.

## Two-byte candidate filtering

For bytes `ab`, every occurrence in a file of at least three bytes must occur
in a trigram `xab` or `abx`. Unioning those existing postings gives the candidate
set. Files shorter than three bytes are included separately. Ordinary source
verification still decides matches, and stale/tombstoned documents are removed.
If a legacy index omitted any relevant stop-gram, narrowing conservatively falls
back to all live documents.

The regex planner applies this to bounded literal alternatives, including
Unicode case-fold alternatives. Optional/nullable expressions and branches that
can match one byte retain conservative fallbacks. There is no new index format,
sidecar, stored bigram index, or rebuild requirement for this optimization.

Regression checks compare files-only and ordinary results with an independent
full-source regex scan across file boundaries, ASCII continuations, UTF-8,
Unicode case folding, alternations, optional expressions, updates/deletions,
full/lean profiles, and compaction of legacy omitted postings. Candidate sets
are also compared directly with source byte windows.

## Measurement scope

The first comparison uses unchanged indexes from the original report, separating
query execution improvements from the coverage correction. Baseline is source
revision `923770d563c23436257841d655c74ed7572f5098`; initial byte-pair implementation
is `e33374f`. Measurements are fresh standalone CLI processes, without benchmark
daemons. One warmup precedes three measured repetitions. Tool order is shuffled
within each pair. For two-byte queries a full ripgrep scan precedes each pair;
longer-query controls use repeated searches after the oracle scan. These small
samples establish neither tail-latency guarantees nor cold-storage behavior.

Expected paths come from ripgrep, subtracting the previously classified ingestion
exclusions. Known NUL-containing files that FXI admits but ripgrep skips are
independently scanned for the literal bytes and added where appropriate. Exact
path sets, rather than counts, are checked. Thus these are equal-coverage binary
comparisons, not a claim of identical default coverage between FXI and ripgrep.

### Initial adjacent-trigram implementation

Median milliseconds; three measured samples per cell. All path sets matched the
coverage-adjusted independent oracle. These compare the old binary with the
first implementation, before the certified Bloom follow-up.

| Query | Mode | Baseline | Byte-pair narrowing | Speedup |
| --- | --- | ---: | ---: | ---: |
| `zx` | default | 16103.57 | 261.73 | 61.53× |
| `zx` | checked | 15395.57 | 552.50 | 27.87× |
| `zx` | packed | 3625.80 | 564.33 | 6.42× |
| `é` | default | 15950.24 | 392.69 | 40.62× |
| `é` | checked | 15380.54 | 789.22 | 19.49× |
| `é` | packed | 3614.94 | 618.70 | 5.84× |
| Rare long literal | default | 161.87 | 161.56 | 1.00× |
| Rare long literal | checked | 44.12 | 44.05 | 1.00× |
| Rare long literal | packed | 42.27 | 43.01 | 0.98× |
| Absent long literal | default | 159.43 | 161.64 | 0.99× |
| Absent long literal | checked | 38.43 | 39.04 | 0.98× |
| Absent long literal | packed | 36.72 | 36.59 | 1.00× |

The default two-byte cases improved by roughly 62× and 41×. Longer rare and
absent controls were essentially unchanged. The checked and packed modes still
showed substantial cache-dependent costs; enabling all experimental modes is
not a blanket recommendation.

## Certified Bloom refinement

The second implementation uses only content-bound, mapped Bloom filters to
reject absent trigrams before dictionary-page lookup. Positive/uncertain lookups
retain the normal dictionary and posting validation. Unbound legacy filters,
missing proofs, and invalid proofs cannot prune this union. Regression checks
exercise incomplete filters, corrupted proofs, replacement filters, and damaged
posting payloads.

The initial mixed-order experiment uncovered a material interaction: the first
implementation touches dictionary pages the Bloom-assisted implementation avoids.
Running one after the other therefore does not give both the same cache state.
Its raw samples are retained as an order-sensitive diagnostic, not as independent
post-scan latency measurements. The follow-up puts a separate full-tree ripgrep
scan before **each** binary invocation for `zx`.

Repeated-search medians (three samples; fresh CLI, no intervening full scan):

| Query | Mode | Without Bloom pruning | With Bloom pruning |
| --- | --- | ---: | ---: |
| `zx` | default | 175.72 ms | 176.06 ms |
| `zx` | checked | 68.87 ms | 53.68 ms |
| `zx` | packed | 79.58 ms | 64.17 ms |
| `é` | default | 230.84 ms | 231.16 ms |
| `é` | checked | 128.81 ms | 111.20 ms |
| `é` | packed | 96.34 ms | 82.78 ms |

Strict/default readers do not use this new Bloom shortcut; their timings are
unchanged within run-to-run variation. Only certified mapped filters qualify.

Individually preconditioned `zx` medians (one warmup, three samples; a separate
full-tree ripgrep scan before every FXI invocation):

| Mode | Without Bloom pruning | With Bloom pruning |
| --- | ---: | ---: |
| checked | 706.24 ms | 434.25 ms |
| packed | 784.88 ms | 493.83 ms |

This confirms a gain in both tested regimes for `zx`, while preserving the
large absolute difference between post-scan and repeated-search latency. The
individually preconditioned follow-up did not repeat `é`; its mixed-order samples
should not be used to claim an independent post-scan speedup. The retained
implementation is `dd84ef7`.

## Fresh-index coverage validation

A fresh default full-profile index using the final executable admitted **459,335
files**, exactly **990 more** than the original index, losing no previously
indexed paths. Every added path was under a formerly excluded directory name.
The `^` probe now differs from ripgrep by 1,097 missing and 41 extra paths,
reflecting the remaining encoding, binary, size, and BOM policies. The coverage
fix does not make FXI a scan of every byte in Chromium.

The single build took **21.08 s**, used **514.1 MiB peak RSS**, and wrote
**2,193.9 MiB** of index data. This is a coverage validation sample, not evidence
of a build-time improvement over the original differently conditioned run.
The extra index bytes represent the additional files; byte-pair narrowing
itself writes no new data.

On this fresh index, one warmup followed by seven repeated standalone searches
measured **185.54 ms for `zx`** (1,148 files) and **251.35 ms for `é`** (6,347 files).
Every sample matched the independently derived eligible path set. `zx` now
recovers two additional matches but still excludes nine ripgrep matches under
other ingestion policies. These repeat timings must not be substituted for the
full-scan-preconditioned timings above.

## Validation and remaining limits

- Coverage regression group: 14 tests passed.
- Library, binary, and integration suite after initial narrowing: 1,125 test
  executions passed, two ignored (the unit suite runs in both library and binary
  targets).
- After the Bloom refinement: six query-local integrity tests, the byte-pair
  source-oracle test, and six generated CLI comparisons passed. The latter cover
  full/lean, stable, and checked modes, with source packs on Unix.
- Clippy passed with warnings denied; formatting and whitespace checks passed.
- The Chromium tracked checkout remained clean; benchmarks used private app-data,
  index, and socket paths, with compilation outside timed measurements.

One-byte searches and unselective/nullable expressions can still scan the whole
eligible corpus. Default strict-reader startup remains significant. Source-pack
selection and cache sensitivity remain open work, and these measurements do not
establish a universal ranking against other tools. This follow-up compares FXI
binaries; it does not rerun every competitor or measure Windows/macOS/Linux
performance parity.

Raw samples, full path differences, source revisions, binary hashes, benchmark
scripts, the order-sensitive experiment, build output, and test logs are in
[`followup-evidence.json.gz`](followup-evidence.json.gz). The original comparison
and corpus provenance remain in [NOTES.md](NOTES.md).
