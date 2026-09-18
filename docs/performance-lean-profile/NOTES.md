# Lean index profile measurements

Build measurements: `350be18`; broad query measurements: `027fd09`.
18 September 2026. Apple M2 Max, 64 GiB, macOS 26.1.
Built with Rust 1.94.0. Each campaign used the same release binary for both
profiles. This measures the profile change,
not a comparison against other search tools or a cold-storage benchmark.

The controlled Linux-source corpus contains 65,284 files and 1,311,592,608 bytes.
The harness checks its content manifest before and after each build campaign;
all build campaigns retained manifest
`adb3052a8f1cd3f0d7b49c7dff2cc5ad36b831da7a4c238c2f899db85ae57854`.

## Build and storage

Three interleaved builds per variant, warm filesystem. Peak RSS comes from
macOS `/usr/bin/time -l`. Every build is checked against exact ripgrep file sets
for absent, selective, phrase and broad literal queries outside the timed region.
No local compilers or test suites ran during these benchmarks.

| Configuration | Full | Lean | Reduction |
| --- | ---: | ---: | ---: |
| No source pack: build | 4.49 s | 2.39 s | 46.7% |
| No source pack: peak RSS | 349.30 MiB | 142.41 MiB | 59.2% |
| No source pack: index size | 574.72 MiB | 218.12 MiB | 62.0% |
| Compressed source pack: build | 8.27 s | 6.43 s | 22.3% |
| Compressed source pack: peak RSS | 357.52 MiB | 326.84 MiB | 8.6% |
| Compressed source pack: index size | 1254.35 MiB | 897.75 MiB | 28.4% |

Lean removes about 356.60 MiB of token dictionaries, postings, positions and
stored line maps on this corpus. It retains gram evidence and source verification.
The packed comparison uses `FXI_SOURCE_PACK=1 FXI_SOURCE_PACK_COMPRESSION=1` for
both profiles. Compression remains opt-in and Unix-only. The packed build
retains substantially more peak RSS than the unpacked lean build; these
measurements do not isolate individual allocation lifetimes.

Final build reports: [unpacked](builds-final.json), [compressed packs](builds-packed-final.json).
Earlier pre-version-guard runs are retained as [unpacked](builds.json) and
[compressed packs](builds-packed.json).
The reports include every sample, exact commands, binary/harness hashes, component
sizes, corpus manifest and retained index directories. Reproduce with
`scripts/compare-index-builds.py --baseline-profile full --candidate-profile lean`
and the same binary for both; set `--source-pack 0` for unpacked or `--source-pack 1`
plus `FXI_SOURCE_PACK_COMPRESSION=1` for compressed packs.

## Search latency

Each row uses 21 randomized pairs of direct CLI processes on a warm filesystem.
Every sample's complete file set is checked against ripgrep; the full and lean
indexes have separate retained directories. These are regex files-only queries.
Default thread settings were used; negative-routing experiments were disabled.
Packed reads were enabled with `FXI_SOURCE_PACK=1` in the packed campaign.

| Query | No pack full / lean (ms) | Compressed pack full / lean (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 33.23 / 33.34 | 33.50 / 33.45 |
| `folio_wait_bit_common` | 35.11 / 35.02 | 35.05 / 34.89 |
| `struct file_operations` | 70.63 / 71.87 | 42.57 / 43.18 |
| `return` | 658.03 / 636.65 | 109.19 / 109.03 |
| `unlikely\(` | 80.55 / 79.32 | 45.58 / 45.35 |
| `Copyright` | 770.83 / 731.01 | 116.89 / 117.10 |
| `return.*0` | 717.14 / 706.08 | 298.64 / 300.08 |
| `return.*[0-9]{12}` | 945.40 / 927.75 | 592.08 / 611.19 |

There is no substantial search-latency improvement here: ordinary queries already
skip token loading. The packed negative regex is about 3.2% slower in the lean
sample; this campaign does not establish the cause or statistical significance.
Broad unpacked queries remain costly. Lean addresses unused evidence construction
and storage, not candidate verification or startup gram validation.

Raw samples: [unpacked searches](startup.json), [packed searches](startup-packed.json).
Use `scripts/compare-startup.py` with the report's separate `--indexes` and
`--candidate-indexes` directories and the same binary for both variants.

## Scope and validation

The generated independent CLI oracle runs both profiles. Lifecycle tests cover
updates, deletion, compaction, forced rebuild, conversion and memory previews;
legacy metadata and missing required gram/token evidence are checked separately.
The complete Rust suite, Clippy on Rust 1.98, MSRV 1.88, fuzz compilation, doctests
and Python harness regressions passed locally for the profile implementation.

These results do not measure update latency, compaction performance, cold storage,
native Linux or Windows performance, or a new competitor ranking. Full remains
the default until broader workloads have been evaluated. Revision-bound source
capture, stable segment publication and streaming compaction remain unfinished.

The broad query timings use `027fd09`, before the follow-up format-version safeguard:
lean now publishes version 3 and readers validate the version/profile pair.
That safeguard changes version handling, not posting layouts, extraction or query
verification. The historical timing samples above have not been relabeled as
measurements of the later binary.

The final build measurements were repeated on `350be18` with version-3 lean
indexes. An actual pre-profile binary also [rejected the new format](legacy-reader.json)
without changing `CURRENT`. Full-profile version-2 compatibility is retained.

A final-format startup check repeated 21 pairs for absent, selective and phrase
queries against the newly built version-3 index ([samples](startup-final.json)):

| Query | Full (ms) | Lean (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 33.95 | 33.68 |
| `folio_wait_bit_common` | 38.05 | 37.69 |
| `struct file_operations` | 68.07 | 68.28 |

These checks likewise show no substantial query-latency gain or regression.

A final-binary manual check also exercised a lean negative-routing certificate:
present/absent file sets were correct, and corrupting gram evidence invalidated
the shortcut and produced an error through the ordinary validation path.

Final code commit `350be18` passed 949 local Rust test executions and all eight
[CI jobs](https://github.com/marlboro-red/fxi/actions/runs/35311002268), including
Linux, macOS and Windows build/test jobs.
