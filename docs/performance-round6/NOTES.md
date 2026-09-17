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
