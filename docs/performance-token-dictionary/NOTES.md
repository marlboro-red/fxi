# Lossless token dictionary metadata compression

This round reduces token dictionary storage without removing token APIs or
positions. The baseline is `70b7ef5`; the first format candidate is `7d7cccd` and
the retained decoder specialization is `7990163`. The result is 51.9 MiB less
dictionary storage and eager-reader RSS, with roughly unchanged loading/search
times in the final comparisons. This is a storage/memory improvement, not a
search speedup.
The current reader accepts both legacy and compact dictionaries, including mixed
segments during updates and compaction. Memory previews retain their existing
representation. New full builds, deltas and compaction write the compact format.
Older binaries do not understand the new token dictionary format; use the new
reader, or rebuild with the older binary if downgrading.
Existing indexes are not rewritten merely by opening them: rebuild or compact to
apply the size reduction to old segments. Restart a running daemon after upgrading
so its reader and writer also use the new binary.

## Why this representation

Across the immutable 65,284-file, 1,311,592,608-byte Linux fixture, 2,949,515 token
entries occupy 107,997,294 dictionary bytes, but their token strings occupy only
19,511,712 bytes. Most space is fixed-width metadata. Prefix compression with
implicit offsets offers a larger theoretical reduction, but would require block
reconstruction or additional decoded storage for random lookup. The first
implementation preserves borrowed UTF-8 strings and the existing offset table.

The new header is `ff ff ff ff 46 58 54 31` (`u32::MAX` followed by `FXT1`), then a
little-endian u32 entry count. Each entry retains its u16 byte length and raw UTF-8
token, followed by unsigned variable-length posting offset, posting length and
document frequency; dictionaries with positions also store position offset and
length. Offsets are u64, other numeric fields u32. Fields retain strict overflow
and truncation checks. The shared decoder serves normal reads and compaction.
Token order, posting ranges, frequencies, document membership and position streams
still undergo validation. No checksum or corruption check was removed for speed.

## Build and size results

Three alternating-order pairs per source-pack setting, separate output directories,
warm filesystem, Apple M2 Max/64 GiB/macOS. Ordinary desktop/background activity
remained running. These short build samples do not establish small CPU speedups.
Every build passes four independent ripgrep file-set checks outside timing, and
source manifests before/after agree with the prior common-corpus hash:
`adb3052a8f1cd3f0d7b49c7dff2cc5ad36b831da7a4c238c2f899db85ae57854`.

| Metric | Baseline | First candidate |
|---|---:|---:|
| Token dictionaries, bytes | 107,997,294 | 53,595,984 |
| Entire index, source packs off, MiB | 626.60 | 574.72 |
| Entire index, source packs on, MiB | 1,886.63 | 1,834.75 |
| Build, source packs off, median seconds | 4.911 | 4.591 |
| Build, source packs on, median seconds | 8.184 | 7.973 |
| Build peak RSS, source packs off, median MiB | 337.30 | 345.56 |
| Build peak RSS, source packs on, median MiB | 347.56 | 350.47 |

Dictionary bytes decrease **50.4%**; total bytes decrease **8.3%** without packs or
**2.75%** with packs. Build RSS does not improve in these samples. Dictionary size
is identical across all three builds of each variant. Position and source-pack
payloads are unchanged. This does not solve FXI's overall size gap to csearch.

## Initial reader tradeoff

Seven alternating-order eager-reader pairs show 625.95 → 574.06 MiB median peak
RSS, but 121.99 → 147.58 ms median opening time. This raised concern about metadata
decoding cost and prompted the follow-up below. Exact token API times were essentially
flat; substring token API medians improved approximately 6–12%. Path counts and
64-bit sorted-path fingerprints agree; this probe is not an independent token
semantic oracle. Full query/delta compatibility tests cover semantics separately.

Ordinary CLI searches load tokens lazily. Eleven paired packed-index one-shot
samples show absent 34.97 → 35.31 ms, selective 37.13 → 37.59 ms, phrase
47.66 → 48.03 ms, broad `return` 113.12 → 113.32 ms: effectively unchanged on
this machine. All responses matched ripgrep. No local compiler or tests ran
during performance measurements.

## Follow-up and retained implementation

The metadata decoder now inlines bounded five/ten-byte decoding rather than using
the posting decoder's cold multibyte path. Deterministic differential tests check
240,000 input prefixes plus final-byte, overflow and noncanonical boundaries
against the strict general decoders. This changes neither the format nor writer.

Nine new eager-reader pairs measured 141.84 → 142.48 ms median opening time and
626.00 → 574.08 MiB peak RSS. A further 31 randomized pairs isolated eager opening
by terminating each probe immediately after it printed the opening measurement,
before the long token lookup batches. They measured **136.770 → 137.502 ms**, with
a median paired after/before ratio of **1.0124**. Timing varies substantially under
desktop load; these results support roughly unchanged loading, not a speedup.

We also compared the first and specialized decoders on exactly the same compact
index in 31 randomized pairs. Medians were 143.35 → 133.92 ms, but the median
paired ratio was 1.0003. This noisy experiment does **not** establish an isolated
decoder speedup. The retained combination's demonstrated benefits are size and
memory; the larger follow-up did not reproduce the initial apparent loading loss.

Final CLI confirmation, eleven pairs on packed indexes, measured absent
35.32 → 35.84 ms, selective 37.19 → 37.36 ms, phrase 48.16 → 48.05 ms and broad
111.21 → 112.90 ms, with complete ripgrep result parity. Build measurements above
use the first candidate's unchanged writer; eager-reader and final CLI checks use
the final candidate. All raw results retain binary/harness hashes.

## Evidence and reproduction

Final local validation: 925 Rust test executions including two doctests (the
library suite runs in both library and binary targets), five benchmark-harness
tests, strict all-target Clippy, formatting, and Rust 1.88 all-target compilation
pass. Coverage includes legacy/compact mixed updates, pinned readers across
compaction, compact truncation/order/frequency/range errors, stateful generated
updates and generated query/CLI oracles. A separate utility fix rejects overflowing
ten-byte u64 integers; its regression fails on the original decoder.

- [Layout estimates](layout-estimates.jsonl), [estimation script](measure-layout.py).
- [Unpacked builds](builds-unpacked.json), [packed builds](builds-packed.json).
- [Initial eager readers](eager-readers.json), [initial CLI startup](startup-initial.json).
- [Final eager readers](eager-readers-optimized.json),
  [focused opening](eager-open-focused.json), [isolated decoder](decoder-isolated.json),
  [final CLI startup](startup-final.json), [focused harness](../../scripts/compare-eager-open.py).
- [Build harness](../../scripts/compare-index-builds.py),
  [token probe](../../examples/token_dictionary_probe.rs),
  [token harness](../../scripts/compare-token-dictionaries.py).

Build comparisons accept `--corpus`, `--baseline`, `--candidate`, `--source-pack`,
`--repetitions` and `--output`; resulting JSON records retained index paths and
binary hashes. Token comparisons accept the same corpus/binaries plus `--indexes`
and `--candidate-indexes`. Both probe binaries were compiled against their
respective release library using `rustc --edition 2024 -O -C lto=fat`, the matching
`--extern fxi=.../libfxi.rlib` and dependency directory. This keeps probe compilation
options identical while measuring the two implementations. The startup comparison
uses `FXI_SOURCE_PACK=1` and the four patterns in its raw result file.
