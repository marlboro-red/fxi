# Batched strict posting validation

Retained optimization: validate eight complete one-byte posting deltas at once for contiguous segment document IDs. Positive deltas plus checked cumulative addition prove ordering; first/last membership proves interval membership. Sparse IDs keep scalar bitmap membership checks. Malformed varints, duplicate IDs, overflow, foreign IDs, and frequency mismatches remain errors. This speeds the strict corruption validation introduced by the audit rather than bypassing it.

## Causal measurement

Same corpus and immutable indexes, source packs enabled, negative routing disabled, direct CLI file output, 17 randomized interleaved pairs for every query. The tracked `scripts/compare-startup.py` requires FXI exit 0 and compares every output set with ripgrep. Both binaries were warmed before measurement. No concurrent compilation or other benchmarks. This measures warm OS-cache process startup plus query execution, not cold storage or pure validator throughput.

Baseline `/private/tmp/fxi-phrase-after` SHA-256 `4ca543592aba72f271925dfb1bb49ccee8331ace99f4254fd88cafbd6c7eafb1`; candidate `/private/tmp/fxi-batched-validation-candidate` SHA-256 `ddaf7d5c52753c2f69888db9575b4efa014d2673b84de4e592ef834ac8d2f40e`. Source snapshot `/private/tmp/fxi-phrase-isolation/src` differs from the candidate only in `utils/encoding.rs` and an executor test comment, so production changes are isolated to this optimization.

| Layout | Query | Before median ms | After median ms | Reduction |
| --- | --- | ---: | ---: | ---: |
| 33 segments | absent | 39.723 | 33.856 | 14.8% |
| 33 segments | selective | 41.184 | 35.769 | 13.1% |
| 33 segments | phrase | 52.394 | 46.510 | 11.2% |
| 33 segments | broad | 116.761 | 110.599 | 5.3% |
| compacted | absent | 32.057 | 25.772 | 19.6% |
| compacted | selective | 34.783 | 28.396 | 18.4% |
| compacted | phrase | 54.172 | 47.708 | 11.9% |
| compacted | broad | 133.968 | 128.675 | 4.0% |

Raw samples, precise query strings, binary/harness hashes, and corpus paths: [33 segments](batched-validation-startup.json), [compacted](batched-validation-compacted.json). A preliminary run using an unnecessarily relaxed temporary harness was discarded; all reported samples use the original tracked strict harness.

## Correctness verification

Three differential regressions compare acceptance against a strict scalar decoder: exhaustive byte streams of length 0–2, every possible byte at every lane of short and batched streams, deterministic random mixed varints, legal overlong encodings, all-zero/overflow/truncated values, sparse document sets, interval endpoints, empty sets, expected-frequency mismatches, and horizontal sums crossing byte boundaries. All passed. Parent's combined validation included this exact candidate: 888 test executions, two doctests, Clippy, MSRV checks, and Windows MSVC Clippy passed.

No universal speed claim follows from this one corpus. Sparse segment IDs take the existing scalar path; long multi-byte deltas may gain less. This measured change removes about 5–6 ms of validation cost on these layouts while preserving the audit's fail-closed contract.
