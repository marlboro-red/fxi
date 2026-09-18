# Source-pack compression experiments

These are offline verification-stage experiments on the existing 65,284-file,
1,311,592,608-byte Linux fixture, Apple M2 Max/64 GiB/macOS 26.1. They are not
end-to-end CLI measurements or a shipped source-pack format. Source-pack runtime
behavior is unchanged. The codecs are development dependencies only.

## First experiment

[Raw samples](linux-lab.json) compare independent 4/16/64 KiB LZ4 blocks,
16 KiB Zstandard levels 1/3, hybrids retaining the first 4 KiB uncompressed, and
packing only files no larger than 64 KiB. Sizes exclude serialized metadata.
The source corpus and every packed representation are resident during the test;
this does not measure cold storage or process memory savings. Encoding is serial
and excludes index construction. Every variant round-trips every byte exactly;
every timed file set matches a whole-source oracle independent of the index's
candidate planner. Eleven measured repetitions follow one warm-up, rotating
variant order and reversing it on alternating repetitions. Four Rayon tasks
verify candidates, including metadata checks and checksums. Timings exclude
index opening, planning, CLI startup and output serialization.

| Variant | Payload MB (decimal) | Phrase verification ms | `return` ms | Forced absent scan ms |
| --- | ---: | ---: | ---: | ---: |
| Raw 4 KiB | 1311.6 | 7.43 | 36.25 | 67.74 |
| LZ4 4 KiB | 505.9 | 25.98 | 57.68 | 178.99 |
| LZ4 16 KiB | 447.5 | 25.61 | 75.81 | 172.27 |
| LZ4 64 KiB | 420.3 | 25.82 | 97.90 | 171.25 |
| Zstd 1, 16 KiB | 302.4 | 62.22 | 168.14 | 390.59 |
| Raw prefix + LZ4 16 KiB | 553.1 | 24.20 | 47.69 | 155.70 |
| Raw prefix + Zstd 1, 16 KiB | 435.0 | 57.85 | 70.71 | 323.21 |
| Raw files <=64 KiB | 635.8 | 17.10 | 56.78 | 102.61 |

Phrase is `struct file_operations`: 3,787 candidates, 1,240 matches. `return`
has 44,315 candidates and 44,258 matches. Forced absence deliberately bypasses
candidate pruning to exercise complete negative verification; the ordinary
absent query has zero candidates and says nothing about codec speed.

The straightforward compression variants fail the warm verification latency
gate. A raw prefix helps early hits (`Copyright` is approximately unchanged),
but does not resolve the cost of deep or negative verification. Excluding large
files also regresses verification. No default policy should change on this
basis. The first Zstd version creates a fresh decoder context for each block;
the next experiment reuses contexts before drawing conclusions about that codec.

## Basis and reproduction

Independent blocks permit bounded decompression. Larger blocks can improve
compression but require more decoding to inspect a small region. The official
[LZ4 format](https://github.com/lz4/lz4/blob/dev/doc/lz4_Block_format.md) and
[Zstandard seekable format](https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md)
provide the relevant format background. Our lab uses independent raw LZ4 blocks
or Zstd frames and keeps descriptors in memory; it does not implement the Zstd
seekable container.

```sh
cargo test --example source_pack_compression_lab
cargo build --release --example source_pack_compression_lab
FXI_INDEXES=/path/to/matching/index-directory \
  target/release/examples/source_pack_compression_lab /path/to/corpus 11 > results.json
```

The JSON retains samples, candidate/match counts, payload sizes, serial encode
times and limitations. First-run provenance includes the harness, binary and
Cargo.lock SHA-256 values. Existing source files and indexes are read-only;
private temporary pack files are deleted when the process exits.

## Filtered compression experiment

[Second-run samples](linux-filtered-lab.json) reuse a Zstd decoder per thread
and add 2,048-bit per-block trigram filters. Each filter covers its block plus
up to 255 following source bytes, so it can reject impossible match starts for
literals of 3–256 bytes without losing boundary-spanning matches. Other lengths
use ordinary verification. The filter is built from the pack's own capture;
it must not be inferred from earlier postings. This lab keeps filters in memory;
a production representation must checksum the filter metadata before trusting
negative evidence. The generated boundary test checks 5,760 present substrings
across codecs and block sizes, plus absent queries and the original corruption
and freshness tests.

| Variant | Payload + filters MB | Phrase ms | `return` ms | `unlikely(` ms | Forced absent ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Raw baseline | 1311.6 | 7.30 | 37.16 | 5.25 | 70.78 |
| Filtered raw | 1402.8 | 3.92 | 36.84 | 4.31 | 47.46 |
| Filtered LZ4 4 KiB | 597.2 | 5.91 | 48.78 | 6.06 | 46.00 |
| Filtered Zstd 1, 4 KiB | 451.4 | 12.22 | 87.61 | 11.76 | 47.05 |
| Filtered raw prefix + LZ4 4 KiB | 693.9 | 5.69 | 40.32 | 5.66 | 46.92 |

The hybrid cuts payload-plus-filter bytes 47.1%, improves this phrase by 22.1%
and improves forced negative scanning by 33.7%, but costs 8.5% on `return` and
7.8% on `unlikely(`. `Copyright` is approximately unchanged (39.20 → 38.79 ms).
These are mixed results, not a universal improvement. The filtered raw control
shows that skipping blocks supplies the speed benefit; compression supplies the
storage benefit and adds decoding cost.

Reusing Zstd contexts reduces its overhead but does not reverse the warm-search
ranking. Simply choosing a denser codec or excluding large files does not solve
this workload. More compact static filters, such as
[binary fuse filters](https://arxiv.org/abs/2201.01174), remain a separate possible
experiment; their construction cost and per-block size must be measured here,
not assumed to beat this small fixed Bloom filter.

## Opt-in CLI prototype

`FXI_SOURCE_PACK=1 FXI_SOURCE_PACK_COMPRESSION=1 fxi index --force PATH`
selects the experimental hybrid writer. The default raw writer is unchanged.
Mixed raw/compressed segments are supported. The compressed reader maps its
per-file filters and descriptors, validating only the candidate file's metadata
on demand; it does not eagerly read the entire filter region. This matters for
startup latency. The serialized format adds block descriptors and file records,
so its actual size is larger than the lab's payload-plus-filter estimate.

Three alternating-order [full build pairs](builds.json) measure 1,834.75 →
1,254.35 MiB total index size, a 31.6% reduction (580.40 MiB). Source table+data
falls from 1,321,234,312 to 712,641,241 bytes, a 46.1% reduction. Median build time
is 6.990 → 10.488 seconds; median peak build RSS is 361.73 → 349.89 MiB. These
builds use a serial compressed writer. Both before/after source manifests match
`adb3052a8f1cd3f0d7b49c7dff2cc5ad36b831da7a4c238c2f899db85ae57854`.

Twenty-one interleaved [CLI query pairs](startup.json), checked against ripgrep:

| Files-only query | Raw ms | Compressed ms |
| --- | ---: | ---: |
| Absent symbol | 33.75 | 34.24 |
| `folio_wait_bit_common` | 35.45 | 35.89 |
| `struct file_operations` | 46.32 | 43.57 |
| `return` | 109.11 | 109.12 |
| `unlikely\(` | 46.38 | 45.77 |
| `Copyright` | 116.98 | 117.07 |
| `return.*0` | 290.48 | 352.76 |

The broad regex needs full decoding and regresses 21.4%. This is why the format
remains experimental and is not a replacement default. Literal results are
encouraging, but neither cold-storage behavior nor cross-platform performance is
established. We have not rerun competitor comparisons in this storage experiment.

Local prototype validation: 932 all-target Rust test executions, strict Clippy,
formatting, and Rust 1.88 all-target compilation pass. Tests cover exact decoded
bytes, generated literal boundaries/lengths, empty and Unicode files, stale
sources with restored mtime, malformed lengths and offsets, filter/payload/table
corruption, deletion, mixed formats, incremental updates, compaction, pinned
readers, and agreement with live CLI literal/regex/case/count/context output.
