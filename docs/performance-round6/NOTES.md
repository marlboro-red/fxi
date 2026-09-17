# Round six: generation-owned source packs

Unix/M2 Max, Rust 1.94 performance binaries, warm filesystem. All timed file sets
are checked against ripgrep. Tests and compilation run outside timing windows.
This is an opt-in acceleration with an explicit storage/build tradeoff, not a
claim that default small-index FXI has become universally fastest.

## Implementation

`FXI_SOURCE_PACK=1 fxi index --force PATH` creates source.data/source.table in each
segment. Delta generations inherit immutable packs; new segments get new packs.
Compaction captures fresh packs after remapping IDs. Generation publication syncs
all files. Orphan data files are unlinked before replacement, never truncated
through inherited hard links. No editable source is memory mapped.

Packs are optional evidence: unsupported format, corrupt table or used data,
changed source identity/timestamps, deletion, and invalid source reads fall back
to existing live verification. The whole source is validated as UTF-8 at capture.
Tables and whole files have XXH3 checksums; format 02 adds 4 KiB block checksums.
Exact case-sensitive literal existence may stop after a checksum-verified positive
witness. Negative evidence requires all blocks; cross-block matches include
validated overlap. General verification retains complete checksum/UTF-8 checks.
XXH3 is accidental-corruption detection, not authentication.

The reader uses packs for uncached files-only scans with at least 128 candidates.
Small scans avoid table-load overhead; daemons retain their bounded source and
position cache. Packed scans use available cores by default while respecting
FXI_SEARCH_PARALLELISM. The original four-task policy remains for ordinary reads.
Non-Unix platforms disable packed evidence. FXI_SOURCE_PACK=0 disables reads.

## Screening experiments

Same common corpus as round four: 65,284 files. Eleven alternating paired samples
per query, new CLI process per sample. These are separate experiments, not one
combined before/after series.

| Experiment | Broad return before/after ms | Phrase before/after ms |
|---|---:|---:|
| Whole-file checksums, four tasks | 593.286 / 121.175 | 50.158 / 30.843 |
| Same whole-file binary, 4 vs 12 tasks | 120.702 / 96.232 | 30.948 / 25.251 |
| Block checksums and packed scheduling | 634.406 / 87.412 | 48.553 / 25.019 |

Format 02 selective control: 13.148 / 13.077 ms. Absent: 11.279 / 11.454 ms.
The format 02 single build screening took 7.292 s including durable publication;
all index files totaled 1,978,273,181 bytes. Repeated final measurements follow.
Do not compare these standalone FXI samples to historical competitor samples to
claim a measured competitive ratio.

## Existing token-position feasibility

The independent `interior_token_lab` proves that fully bounded interior ASCII
tokens of an exact case-sensitive literal can safely constrain indexed substring
candidates. Endpoint fragments are excluded; tokenizer ordinal gaps include
unindexed short/long tokens. Unicode-insensitive literals are excluded.

On thirteen fixed literals, token plus ordinal-position constraints remove 2.42%
of candidate visits and 6.21% of candidate bytes, with no new index bytes. Phrase
candidates fall 3,787 to 2,853; `const struct file_operations` falls 3,744 to 1,800.
These are precision observations, not latency predictions. The experimental
module checks every true file against independent ripgrep results. Loading token
dictionaries for one-shot search could erase the savings; a warm-only experiment
is being evaluated separately.


## Fresh three-tool comparison

`linux-packed-common-indexers.json` rebuilds all three tools three times on the
same 65,284-file corpus, validates complete coverage after every build, then
interleaves eleven samples per query with exact ripgrep parity. All tools use
complete file-only outputs. Zoekt ctags is disabled, matching round four.

| Tool | Build median s | Peak build RSS MiB | Index MiB |
|---|---:|---:|---:|
| FXI, source packs enabled | 7.090 | 354.2 | 1886.6 |
| csearch | 9.916 | 291.9 | 73.0 |
| Zoekt | 112.192 | 1324.3 | 3239.8 |

| Query | FXI ms | csearch ms | Zoekt ms |
|---|---:|---:|---:|
| Selective | 13.634 | 15.931 | 56.936 |
| Absent | 11.736 | 4.227 | 55.797 |
| Phrase | 25.197 | 379.627 | 60.372 |
| Broad return | 89.179 | 1599.271 | 139.555 |
| Alternation | 15.019 | 34.836 | 57.338 |
| Internal literal | 14.055 | 16.361 | 59.333 |

The optional packed configuration wins six of six against Zoekt and five of six
across all tools here. It is not universal dominance: csearch remains faster on
absence, uses far less disk and less build memory; these are warm-filesystem
one-shot results, not cold-storage, simultaneous-client, line/count output or
Windows results. FXI still verifies current source metadata; Zoekt searches its
indexed snapshot. The source pack costs about 1.23 GiB beyond the default index.

## Rejected warm interior-token filter

`interior-tokens-experiment.patch` preserves a default-off warm-only implementation
and six added tests. All 723 test executions passed with the experiment enabled.
Twenty-one paired queries in `linux-interior-warm.json` showed phrase
11.639 -> 10.454 ms and const phrase 11.325 -> 10.234 ms, but static-const regressed
46.760 -> 48.026 ms, selective 4.353 -> 4.559 ms, and broad 68.636 -> 69.677 ms.
The implementation is not shipped. The standalone precision lab remains useful.

## Cached verification task count

Twenty-one paired same-binary samples separate task scheduling from other code
changes. Four versus twelve tasks reduced phrase 11.613 -> 10.767 ms, static-const
47.930 -> 42.439 ms and return 66.071 -> 52.794 ms. The 8-versus-12 follow-up was
essentially tied on broad/static-const; phrase was slightly faster with eight.
All recorded first-query timings also improved in the 4-versus-12 run.

Redis 4-versus-8 controls are effectively neutral, with return 4.976 -> 4.826 ms.
CPython return improves 7.453 -> 6.864 ms; its other three controls are neutral or
slightly better. Eight is the conservative measured default for Unix cached
files-only scans estimated to fit the cache. Ordinary/oversized/non-Unix scans
retain four tasks; explicit FXI_SEARCH_PARALLELISM overrides both. This changes
scheduling, not source freshness, matching semantics, or the cache byte budget.

The final scheduler binary confirms the task-setting sweep over 21 paired CLI
samples: phrase 11.548 -> 10.694 ms, static-const 48.105 -> 42.513 ms, broad
66.131 -> 52.911 ms, selective 4.410 -> 4.439 ms. No one-shot code path changes.

Twenty-one canonical warm API samples against freshly built stock Zoekt indexes:

| Query | FXI API ms | Stock Zoekt API ms | Zoekt paths-only API ms |
|---|---:|---:|---:|
| Selective | 0.598 | 0.893 | 0.593 |
| Absent | 0.275 | 0.330 | 0.127 |
| Phrase | 5.894 | 5.764 | 1.611 |
| Broad return | 37.222 | 205.543 | 59.554 |
| Alternation | 1.002 | 1.187 | 0.917 |
| Internal literal | 0.622 | 3.776 | 3.526 |

The comparable paths-only phrase gap remains about 3.7x; it has not disappeared.
The full-response broad ratio includes Zoekt's much larger payload and is not an
engine speedup. FXI's strong warm broad and internal-literal results survive the
comparable-output test; selective is tied, and absence still favors Zoekt.

## Smaller-corpus and ordinary-query controls

Twenty-one paired direct samples on the existing Redis/CPython fixtures, using
one identical packed index for old/new binaries (the old binary ignores packs):

| Corpus/query | Baseline ms | Packed ms |
|---|---:|---:|
| Redis static void | 7.933 | 5.745 |
| Redis raxFind | 5.228 | 5.280 |
| Redis return | 12.744 | 6.326 |
| Redis absent | 4.642 | 4.702 |
| CPython static int | 13.880 | 8.054 |
| CPython PyErr_SetString | 8.842 | 6.647 |
| CPython return | 29.812 | 10.094 |
| CPython absent | 5.067 | 5.122 |

On the common Linux corpus, bare case-insensitive identifier queries (checked
against ripgrep -i -F) improve return 661.012 -> 112.439 ms and file_operations
75.258 -> 37.604 ms; selective folio_wait_bit_common regresses 15.176 -> 15.756 ms.
These use whole-file packed validation, not the exact-case literal block shortcut.
The extra storage therefore also helps common ordinary CLI queries, but the
small selective counterexample is retained rather than hidden.

## Negative-routing experiment, first version

An opt-in prototype certifies existing core structural validation using Unix
identity/mtime/ctime/length stamps and checksummed metadata bound to one immutable
generation. It requires a separately issued certificate and an exact case-sensitive
regex literal. Every segment Bloom must reject the query; any changed/missing
core dependency falls back to ordinary validation and its existing errors.

For auditNonexistentSymbol94283, every gram exists globally, but no segment has
all grams. All 33 existing Blooms reject it. Their combined 5,053,475 bytes are
smaller than the 84,343,552 dictionary bytes scanned at ordinary startup.
Nevertheless, reloading/checksumming those Blooms loses: 21 paired samples show
absence 10.455 -> 11.248 ms, selective 12.449 -> 12.863 ms, phrase 25.690 ->
26.188 ms, broad 86.703 -> 86.342 ms. The certificate's 102 stamps were verified
current, so this is an actual eligible-path regression, not stale-proof fallback.
The explicit environment was FXI_NEGATIVE_ROUTING=1; both tools used the same
fresh certificate-bearing packed index. The opt-in build also used
FXI_SOURCE_PACK=1 and took 7.21 s in this single screening sample.

The prototype preserves corruption errors, unsupported-query fallback and stale
warnings. An additional lifecycle test demonstrates safe fallback after old
hardlinks are collected: unlinking old aliases changes ctime on inherited files
in the new generation too. No ctime check is dropped to hide this limitation.
A certified mapped-Bloom revision is being evaluated separately.

The mapped revision checks each OPEN Bloom handle's certified stamp before mapping
and probing checked header/word ranges with the shared portable hash function.
It reuses the prior payload/coverage validation instead of rehashing every word.
The remaining core stamps are checked only after every segment rejects.

Twenty-one paired samples in `linux-mapped-negative-routing.json`:

| Query | Without certificate preflight ms | Mapped preflight ms |
|---|---:|---:|
| Absent | 11.710 | 5.818 |
| Selective | 13.336 | 12.913 |
| Phrase | 24.302 | 24.383 |
| Broad return | 87.917 | 86.492 |

The environment was FXI_NEGATIVE_ROUTING=1; the baseline binary predates the
feature, and both read exactly the same certificate-bearing packed index. This
is a useful opt-in reduction, not a measured win over csearch's 4.227 ms result
from the earlier fresh three-tool comparison. The prototype stays explicitly
experimental because inherited-link collection can remove its benefit.

Validation includes bit-probe parity for every supported hash count, malformed
and truncated mapped headers, same-length core corruption with restored mtime,
missing/corrupt/mismatched Blooms and certificates, stop-gram fallback, delta and
compaction generations, inherited-link collection, and a real CLI regression for
corruption errors, invalid regex, and newly indexed matches. Local validation:
727 all-target test executions plus the subsequently added CLI test; strict
Clippy on Rust 1.98 and Rust 1.88 all-target checks passed.

For a fresh experimental combined index and queries in the same shell:

```sh
export FXI_SOURCE_PACK=1 FXI_NEGATIVE_ROUTING=1
fxi index --force PATH
fxi -l 're:/struct file_operations/' -p PATH
```

Source packs remain useful without certificates. Negative routing is a startup
optimization only; it does not change the remaining warm phrase loss to Zoekt.
