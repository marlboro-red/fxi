# Architecture follow-up measurements

Baseline `735d32d`; Apple M2 Max, 64 GiB, macOS 26.1, Rust 1.94.0 release builds.
These are warm-filesystem measurements. Source-pack acceleration remains Unix-only.
Every report records binary hashes and raw samples. Timed campaigns run without
local compilation or test suites in parallel.

## Changes and validation boundaries

- `67a54f4`: route line-based regex verification through a required literal, then
  verify the complete original line. The anchor is a necessary condition, never
  an accepted match. Nullable branches, alternatives, anchors, CRLF and Unicode
  retain the ordinary verifier's semantics. Counts share the same line routing.
- `86a47cb`: compact one term at a time, including token positions, and stream line
  maps. Input mappings and document remapping remain resident. Allocation scales
  with the largest term/file plus metadata, rather than all merged postings.
  This is not a process-wide RSS cap: resident mapped pages still contribute.
- `75bac78`: skip redundant file syncs only for hard links inherited from the
  current published generation. Copies, legacy-layout inputs and all new files
  still require syncing. All new directory entries and CURRENT retain their
  existing durability ordering. Segment inheritance still creates hard links.
- `6f2cda7`: prepare immutable document membership once per segment and validate
  runs of 32 one-byte posting deltas together. Full dictionary/payload checks,
  strict positive ordering, bounded sums, membership and frequencies remain.

Streaming compaction has a materialized-reference test with tombstones for both
profiles: gram/token/position files match byte for byte; line maps match
semantically. Corrupt input tests require failure without publication. Existing
source-pack, stateful update, generated query and CLI oracle suites still apply.
Posting validation is checked against a scalar reference, including every byte
lane through the new 32-byte boundary, malformed and overlong varints, sparse
membership and overflow. Publication tests distinguish inherited durable links,
new copies and legacy inputs, and exercise sync failure propagation.

## Regex line routing

[Initial query samples](queries-line-routing.json): 15 randomized pairs per
pattern, same lean compressed index, exact complete ripgrep path sets checked
on every sample. Baseline versus the first exact-anchor implementation:

| Regex | Before | After |
| --- | ---: | ---: |
| `return.*0` | 308.88 ms | 118.36 ms |
| `return.*[0-9]{12}` | 574.52 ms | 131.63 ms |
| `^static.*void` | 248.42 ms | 111.91 ms |
| `(?i)return.*0` | 337.27 ms | 350.85 ms |

Case-sensitive broad regexes improve by 55–77% in this campaign. Insensitive
matching did not benefit; its measured regression motivated a separate follow-up.
Literal, phrase and absent queries were essentially unchanged. The second regex
matches 28 files; it is not a zero-match query.

## Compaction

Three alternating samples per variant/profile, starting from private copies of
identical indexes built in 4,096-file chunks. The controlled Linux corpus has
65,284 files / 1,311,592,608 bytes, manifest
`adb3052a8f1cd3f0d7b49c7dff2cc5ad36b831da7a4c238c2f899db85ae57854`.
Source packs are disabled in this campaign. Preparation, copying and verification
are outside the timed interval; verification includes exact query results,
complete document counts and identical compacted gram/token/position/Bloom hashes.
Line-map record ordering may differ without changing semantics.

| Profile | Before | Streamed | Before peak RSS | Streamed peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Lean | 1.359 s | 0.987 s | 921.2 MiB | 388.2 MiB |
| Full | 6.723 s | 4.841 s | 3741.8 MiB | 887.5 MiB |

Lean compaction is 27.3% faster with 57.9% less peak RSS; full compaction is
28.0% faster with 76.3% less peak RSS. Output size is unchanged (136.02 MiB lean,
454.58 MiB full). This measures consolidating a full fragmented build, not
thousands of edits, packed compaction or foreground query contention.

Reproduce with `scripts/compare-compaction.py --corpus CORPUS --baseline BEFORE
--candidate AFTER --profile lean|full --repetitions 3 --output REPORT`.
[Lean samples](compaction-lean.json), [full samples](compaction-full.json).

## Durable publication

[Publication samples](publication.json): five alternating updates per variant at
1, 64 and 256 inherited segments. Each update adds one file to an identical
4,096-file synthetic full-profile index; packs are disabled. Fresh processes
verify exact old/new membership afterward. The baseline includes streamed
compaction and the first regex improvement, isolating the durability reuse change.

| Inherited segments | Before | After |
| --- | ---: | ---: |
| 1 | 40.34 ms | 37.78 ms |
| 64 | 211.83 ms | 198.03 ms |
| 256 | 757.71 ms | 687.91 ms |

Whole CLI update times improve by 6–9%. Logs retain phase traces. These numbers
are durable update time, not watched visibility latency. Inherited-tree work
still scales with segment count; stable segment references and independent live
update scheduling are not implemented by this change.

Reproduce with `scripts/compare-publication.py --baseline BEFORE --candidate AFTER
--repetitions 5 --output REPORT`.

## Startup validation

[Startup samples](startup-validation.json): 31 randomized pairs, exact ripgrep
path-set checks, same lean compressed index and previous code as control.

| Query | Before | After |
| --- | ---: | ---: |
| Absent symbol | 33.53 ms | 33.00 ms |
| Selective symbol | 35.20 ms | 34.47 ms |
| `struct file_operations` | 43.15 ms | 42.83 ms |
| `return` | 110.25 ms | 109.55 ms |

This is a modest constant-factor improvement, not a solution to validation cost
proportional to index bytes. No certificate shortcut, changed corruption policy,
or weakened validation is enabled.

## ASCII-fold line routing follow-up

`7f733f8` extends necessary line anchors to proven ASCII case-fold sequences.
Unicode classes with additional folds are declined or contribute only another
provably mandatory ASCII run. The full Unicode regex still verifies each candidate
line. Generated tests include mixed case, kelvin signs, long-s, optional branches,
anchors and CRLF.

Fifteen randomized pairs against `6f2cda7`, same indexes. Files-only and count
outputs were independently checked against ripgrep on every sample. Count output
uses live-source verification; enabling packs does not turn it into the packed
files-only path.

| Mode / regex | Before | After |
| --- | ---: | ---: |
| files: `(?i)return.*0` | 377.14 ms | 169.92 ms |
| files: `(?i)^static.*void` | 280.89 ms | 150.42 ms |
| files: `(?i)return.*[0-9]{12}` | 681.11 ms | 245.39 ms |
| files: `return.*0` | 120.68 ms | 120.54 ms |
| count: `(?i)return.*0` | 1148.00 ms | 769.02 ms |
| count: `(?i)^static.*void` | 949.75 ms | 675.32 ms |
| count: `(?i)return.*[0-9]{12}` | 1145.73 ms | 789.79 ms |
| count: `return.*0` | 707.04 ms | 716.95 ms |

[Files-only samples](fold-files.json), [count samples](fold-count.json).
The insensitive files-only cases improve by 46–64%; insensitive counts by 29–33%.
The already-optimized case-sensitive count control measured 1.4% slower. These
are sampled workload results, not a guarantee that all regexes become faster.

## Rejected parallel inheritance experiment

The experimental [patch](parallel-inheritance.patch), applicable to `6f0c9a7`,
overlaps inheritance across at most four batches of segment directories. It
passed the full local suite and Clippy, but was reverted after the paired
[publication benchmark](publication-parallel.json) showed no material gain.

| Inherited segments | Serial | Parallel experiment |
| --- | ---: | ---: |
| 1 | 36.14 ms | 34.76 ms |
| 64 | 117.29 ms | 116.47 ms |
| 256 | 354.89 ms | 352.93 ms |

Absolute times differ substantially from the first publication campaign; do not
compare those unpaired times as another improvement. The matched controls here
show under 1% improvement at 64/256 segments. The cause of the between-campaign
shift was not isolated. Stable storage references remain a separate design task.

The held-out, layout and competitor campaigns below used the binary containing
this experiment plus the x86 finder-layout fix (`6f0c9a7`). The rejected patch
only changes generation inheritance, not query execution. Binary hashes preserve
that exact provenance.

## Held-out Redis checks

[Redis samples](redis-held-out.json): 21 randomized pairs, previous-turn baseline
versus current query implementation, existing full/raw-pack index. Complete
files-only outputs matched ripgrep on every sample.

| Regex | Before | After |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 5.41 ms | 5.28 ms |
| `raxFind` | 6.00 ms | 5.95 ms |
| `return.*0` | 13.65 ms | 8.18 ms |
| `(?i)return.*0` | 13.49 ms | 8.65 ms |
| `^static.*void` | 9.58 ms | 7.21 ms |
| `return.*[0-9]{12}` | 21.88 ms | 8.79 ms |

The tested broad regexes improve by 25–60%; absent/selective queries remain close.
This is a second C-heavy corpus, not evidence for every language or repository.

## Consolidated packed-layout experiment

The current lean compressed index was copied and compacted with the new merger.
This tests an existing explicit maintenance operation; no automatic post-build
compaction policy was added. Both query variants use the identical executable.

The one compaction sample took 6.15 s, peaked at 468.5 MiB RSS, and produced 816.06 MiB, versus 897.75 MiB before. This is a single maintenance sample, not a timing distribution.

21 randomized query pairs, every complete path set checked against ripgrep:

| Query | Fragmented | Compacted |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 34.78 ms | 27.67 ms |
| `folio_wait_bit_common` | 37.41 ms | 30.72 ms |
| `struct file_operations` | 44.49 ms | 45.82 ms |
| `return` | 114.08 ms | 130.94 ms |
| `return.*0` | 124.50 ms | 141.42 ms |
| `(?i)return.*0` | 170.06 ms | 201.25 ms |
| `return.*[0-9]{12}` | 138.24 ms | 155.78 ms |

Absent/selective searches improved by 18–20%, but broad packed queries regressed
by 13–18%, and compaction adds maintenance cost. Posting layout, segment count
and source-pack layout all change together; this campaign does not isolate their
individual contributions. It does not justify unconditional compaction after
every build. The operation remains available as `fxi compact PATH`.

[Maintenance sample](layout-compaction.json), [query samples](compacted-layout-queries.json).
Reproduce by copying an index directory, running `FXI_INDEXES=COPY
FXI_SOURCE_PACK=1 FXI_SOURCE_PACK_COMPRESSION=1 fxi compact CORPUS`, then using
`compare-startup.py --indexes ORIGINAL --candidate-indexes COPY` with the same
executable for both variants.

## Fresh comparison with pinned search tools

[Complete raw comparison](competitors.json), 11 randomized measured repetitions
per query plus warm-up. The corpus manifest was independently rehashed before
and after the campaign, and every tool first passed a complete nonempty-file
coverage check (65,284 files). Every timed complete path set matched ripgrep.

These reuse existing indexes of the same source snapshot. `FXI full` uses the
full index with packed reads disabled; `FXI lean` disables packed reads on the
lean index; `FXI packed` enables its compressed source pack. Stored packs remain
on disk even when reads are disabled. This is a query comparison, not a new
build/storage comparison. The optional negative-routing experiment is disabled.

Tools are the pinned binaries already used by this repository, not newly fetched
latest releases. Every executable hash is recorded. tgrep is pinned to
`b1d0fc2f6245cc78f1943e5864ceeab812452404`; csearch/Zoekt build provenance is in
[round four](../performance-round4/provenance.json).

All entries are median milliseconds, direct CLI, warm filesystem, case-sensitive
regex, unlimited complete files-only output:

| Pattern | FXI full | FXI lean | FXI packed | tgrep | csearch | Zoekt | ripgrep |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `folio_wait_bit_common` | 38.28 | 37.43 | 38.09 | 20.10 | 16.78 | 61.59 | 2383.10 |
| `auditNonexistentSymbol94283` | 35.42 | 36.10 | 36.23 | 18.95 | 4.79 | 60.15 | 2408.01 |
| `struct file_operations` | 78.74 | 81.71 | 46.29 | 111.37 | 389.38 | 64.60 | 2324.09 |
| `return` | 652.67 | 646.64 | 118.77 | 2231.45 | 1556.39 | 155.30 | 2138.76 |
| `folio_wait_bit_common\|bpf_prog_select_runtime` | 38.42 | 39.53 | 39.12 | 28.23 | 36.34 | 63.25 | 2386.52 |
| `.*folio_wait_bit_common` | 38.10 | 38.06 | 38.24 | 20.94 | 17.62 | 65.75 | 2370.97 |
| `return.*0` | 654.77 | 658.98 | 128.25 | 2215.89 | 1732.76 | 313.70 | 2218.54 |
| `return.*[0-9]{12}` | 626.66 | 668.18 | 143.85 | 1820.14 | 2291.88 | 311.35 | 2205.56 |
| `^static.*void` | 540.53 | 523.81 | 116.97 | 2326.68 | 1464.51 | 1257.78 | 2296.05 |

Packed FXI leads all five broad/phrase cases in this suite. Its three broad regex
medians are 2.2–10.8 times faster than Zoekt, the next-fastest tool on those rows.
That does not make FXI the universal winner: csearch is about 7.6 times faster on
absence and 2.3 times faster on the selective symbol; tgrep also wins selective
and absent searches. Without packed reads, FXI loses several broad cases to
Zoekt. Strict startup validation remains a major obstacle.

No cold-storage, native Windows/Linux timing, multirepository service workload,
ranked/content-output comparison, new competitor build-time comparison or broad
language diversity claim follows from this table.

Reproduce with `scripts/compare-existing-indexers.py --snapshot SNAPSHOT.json
--fxi FXI --fxi-indexes LEAN_INDEXES --tgrep TGREP --repetitions 11 --output REPORT`;
pass the report's patterns through `--patterns` to include the broad-regex cases.

## Ownership validation follow-up

`19465e0` closes a line-map ownership gap found during compatibility review.
Lazy reads validate every loaded record against its segment's document set;
streamed compaction checks each record against original document ownership.
Foreign-segment records fail even when the original owner's map is missing.
Tests require repeated lazy calls to retain the error and compaction to preserve
CURRENT on failure. A separate mixed legacy fixture covers missing optional
positions and line maps through actual publication.

The complete local suite, Clippy with warnings denied, Rust 1.88 all-target checks
and locked fuzz-target compilation pass. An x86-specific Clippy failure in the
ASCII-fold change was fixed by boxing the cached finder (`6f0c9a7`). This changes
representation, not matching rules. Native CI covers Linux, macOS and Windows.

## Final shipping-build verification

The final production executable (`19465e0`, with parallel inheritance reverted)
was checked separately against the original baseline. Eleven paired samples on
the same lean compressed Linux index produced these medians:

| Query | Before | Shipping build |
| --- | ---: | ---: |
| Absent symbol | 32.79 ms | 31.96 ms |
| Selective symbol | 35.09 ms | 34.22 ms |
| `return.*0` | 270.19 ms | 118.81 ms |
| `(?i)return.*0` | 323.87 ms | 163.83 ms |
| `return.*[0-9]{12}` | 544.61 ms | 133.87 ms |

Every complete result set matched ripgrep; [raw samples](queries-shipping.json)
retain executable hashes. The final full-profile compaction recheck, including
the ownership fix, measured **6.668 s → 4.942 s** and **3,819.0 MiB → 883.8 MiB**
peak RSS: 25.9% faster and 76.9% less memory. Three alternating pairs retained
identical output component hashes and size. See [final compaction samples](compaction-full-final.json).

The independent [Redis coverage check](redis-coverage.json) verifies all 1,098
nonempty files, with manifest
`77ea9815ba86f433aa6f07590093282a9a6e104dca1851875d6180d900342928`.

Six sustained watcher trials (three per variant) each applied 440 edits at 25 ms
intervals, crossing the ten-second maximum durable age. All passed exact create,
edit, retained-match and deletion checks, plus graceful-shutdown persistence.
Each recorded a durable incremental publication before final visibility.
Median visibility after the final edit was 28.68 ms before and 25.91 ms after.
These are three samples per variant on a 256-file fixture, include CLI polling,
and do not establish tail latency or behavior during slow commits. Raw files are
`freshness-burst-{before,after}-{0,1,2}.json` in this directory.

The shipping code passed 964 local all-target test executions, Clippy, Rust 1.88
checks and locked fuzz-target compilation. All eight jobs in its
[remote CI run](https://github.com/marlboro-red/fxi/actions/runs/35335633824)
passed, including Linux, macOS and Windows tests. Fuzz compilation is not a claim
of an extended fuzzing campaign.

## Startup certificate review: not promoted

An independent architecture review examined using the existing epoch-2 negative
routing certificates to skip gram payload validation for positive searches.
Their recorded validation invariants are sufficient under a trusted issuer,
immutable published bytes and reliable filesystem identity/timestamps. They are
not equivalent to inspecting the payload on each new reader: the checksum does
not authenticate the issuer, and unstamped corruption can evade the metadata
checks. Existing tolerant posting decoders could then silently omit results.
No positive-search shortcut was implemented or enabled on that basis.

A future opt-in experiment would need a private evidence type, one pinned
generation, and certificate binding to the actual opened metadata, document,
path, dictionary, posting and Bloom handles before and after construction.
Pathname checks followed by reopening are insufficient. Document references,
dictionary structure and Bloom checks would remain validated; ineligible evidence
must retry strict opening, never return partial results. Public eager opening
and compaction must remain strict. Inheritance and garbage collection can both
change hardlink ctime; these must invalidate reuse rather than merely refreshing
recorded stamps. This also limits the shortcut's usefulness after updates.

The remaining structural work is a format with checked query-local dependencies,
stable segment references to avoid inherited-tree publication work, and update
scheduling that isolates live visibility from slow durable commits. None is
claimed solved by the constant-factor improvements measured here.
