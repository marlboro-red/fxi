# Checked dictionaries and query-local postings experiment

This follows `fad7919`. The default reader remains strict. `FXI_QUERY_LOCAL=1`
opts into an experimental storage and search policy; it is not a blanket speed
claim or a change to default corruption handling.

## Design and correctness boundary

The first `FXIGRAM1` prototype stored a whole-dictionary checksum and one 64-bit
XXH3 checksum per posting. The current `FXIGRAM2` format divides the sorted
vocabulary into pages of at most 512 entries. A checksummed root records the
complete entry count, posting-file length, and every page's first/last key and
checksums for its dictionary records and posting-hash slice. The root's ordered,
disjoint ranges prove outer-range/interpage absence. A key within a page requires
checking that entire page before lookup. Fixed partitions cover every record;
page endpoints, ordering and posting ranges are validated before use.

Publication creates evidence only after validating the staging segment with the
strict reader. Existing inherited evidence is validated, never refreshed to bless
changed bytes. All new files use the existing sync-before-CURRENT protocol.
Version 1 remains readable; inherited v1 segments keep the whole-dictionary cost.
A forced rebuild generates v2 pages for every segment.

Experimental readers validate the root at open. Optional `grams.bloom-check`
evidence binds that checked root digest to an XXH3 digest of a coverage-checked
Bloom filter's ordered words, probe count and length. Publication verifies every dictionary key is represented before
issuing this proof. Readers hash the actual root and loaded filter, without file
stamps; absent, damaged or mismatched proof disables Bloom pruning and falls back
to checked pages. This adds 32 bytes per segment and retains safe fallback for
legacy filters. Each
accessed page and posting is checksummed; postings are fully validated for count,
ordering and document membership before decoding, including before an intersection
can stop decoding early. Successful checks are cached within that immutable
reader using one byte per posting plus small page state; failures remain errors.
Missing sidecars use the legacy eager path; malformed roots fail at open and
malformed dependent pages fail on use. Public eager opening and compaction
validate every page and posting, including checksums when evidence exists.

An unrelated damaged posting need not fail a query that does not depend on it.
`fxi stats PATH` uses the eager reader and checks all gram postings and full-profile
token evidence; it is not a source-to-index completeness audit or a check of every
lazy optional artifact. Invalid evidence never becomes an empty result. Setting
both experimental flags disables timestamp-based `FXI_NEGATIVE_ROUTING` preflight,
so that it cannot bypass the checked reader.

Checksums detect accidental damage relative to publication bytes; they do not
provide authentication against coordinated rewriting. Ordinary reader document/path/metadata validation retains its existing structural
policy. The checked absence preflight described below additionally binds the
actual metadata, document and path bytes to publication hashes. Immutable published files
remain a reader-lifetime requirement. This experiment does not claim to discover
unindexed source changes.

The public Rust `get_trigram_docs` and `get_trigram_docs_with_bloom` APIs now return
`Result<RoaringBitmap>` so errors propagate through every gram execution path.
CLI syntax and output are unchanged. Library consumers must handle the result;
the repository examples have been updated.

## Usage

Build experimental evidence explicitly:

```sh
FXI_QUERY_LOCAL=1 fxi index --force --profile lean PATH
FXI_QUERY_LOCAL=1 fxi -l 're:/selective_symbol/' -p PATH
```

Use the flag on incremental indexing and compaction to generate evidence for new
segments. Mixed generations remain readable: a segment without a sidecar is
validated eagerly. Omit the flag for strict searches. Existing formats and
posting encodings are retained; the sidecar is additive.

`FXI_DEBUG=1` reports metadata/pinning, document/path loading, membership setup,
segment loading, and final setup, plus per-segment mapping/checksum and validation
times. Parallel segment times overlap and must not be added as elapsed time.

## Validation

Tests cover every single-byte mutation and every truncation of a small dictionary
and sidecar through opening or dependent lookup, empty/512/513-entry page boundaries,
partial final pages and interpage gaps, later-page dictionary/hash corruption, truncated posting files, structurally valid posting damage, damage
beyond filtered decoding's early exit, repeated errors, eager fallback without
evidence, independent queries against unrelated damage, and strict rejection.
CLI comparisons exercise full/lean multisegment indexes, positive/absent and
compound queries, case folding, Unicode, filters, files/counts/content output,
edits/deletions and compaction. Damaged inherited evidence prevents publication
and preserves CURRENT. Both experiment flags together cannot bypass corruption.

## Measurements

Measurements use the existing controlled Linux-source corpus on the Apple M2 Max,
64 GiB, macOS, warm filesystem. Builds alternate three times, use lean compressed
source packs, and verify corpus manifests before/after plus exact ripgrep result
sets. Query campaigns compare every complete result against ripgrep outside the
timed process interval. No compilation runs during timed measurements.

Raw build results: [builds.json](builds.json). Raw diagnostic trace:
[initial-trace.json](initial-trace.json). The trace was collected with test work
running and is diagnostic only, not a speed comparison.

The first whole-dictionary prototype measured 34.71 → 21.67 ms for absence,
36.38 → 23.55 ms for a selective symbol, and 44.97 → 32.28 ms for a phrase
(21 paired samples each). Broad query controls also improved. See
[whole-dictionary query samples](queries-checked-dictionary.json).

Build medians were 3.578 → 3.735 s; the first candidate sample was 7.381 s,
followed by 3.735 and 3.645 s. That outlier is retained, not attributed to a
specific cause. Packed index size increased from 941,356,724 to 975,095,412 bytes
(+33,738,688 bytes, about 3.6%). These results motivate paging the dictionary
rather than treating the remaining whole-dictionary scan as solved.


## Legacy Bloom checksum correction

Review found that the existing rotating-XOR Bloom checksum permits swaps of
words 64 positions apart without changing the checksum. Such a swap can remove
a required probe bit. Strict readers now corroborate Bloom negatives against
the fully validated dictionary before excluding a segment. Experimental proofs
use an independent ordered-content XXH3 digest, so a reordered filter disables
pruning and falls back to checked pages. A regression deliberately preserves the
legacy checksum while removing a match's probe bit and checks both reader modes.
The legacy on-disk Bloom format remains readable.


## Checked absence and mapped paths

The next iteration adds a generation-owned `query-routing.bin`. Publication
issues it only after strict validation of every segment and core table. It binds
the ordered, complete segment list, metadata/document/path hashes, checked gram
roots and strong Bloom digests. The files-only CLI preflight hashes actual content
before accepting absence; timestamps alone are insufficient. Missing, damaged,
unsupported or inconclusive proof falls back to the ordinary checked reader.

Eligibility is deliberately narrow: a case-sensitive complete regex literal of
at least three bytes, without filters, line breaks or extra assertions, and with
at least one non-stop trigram. Every segment must reject the literal. Positive,
compound and unsupported queries retain the normal execution path. This avoids
allocating document membership and path objects for proven absence; it still
reads and hashes the evidence and core tables. Unrelated posting payloads are
outside this proof's dependency set, as under query-local validation generally.

Both strict and experimental readers now validate the mapped path table upfront
but allocate `PathBuf` objects only for requested paths. Bounds, UTF-8, safe
relative components and trailing-byte checks remain eager. Shared snapshots use
thread-safe once-only initialization. Regression tests cover complete segment
coverage, changed core bytes, altered roots, malformed/truncated evidence,
stop-gram-only and empty generations, CLI fallback, and concurrent path access.

## Test storage review

An external review correctly identified that some library integration tests
and benchmark fixtures inherited the real app-data directory. `cfg(test)` alone
would not fix this: integration tests link the ordinary library.

The inspected local store contained 15,324 containers, 15,302 readable registered
roots and 14,489 missing source roots. These are local observations, not the
review's exact counts, and do not prove which historical run created each entry.
No existing user indexes were deleted.

Unit tests now automatically select private process storage. Integration library
fixtures and Criterion fixtures explicitly initialize it; CLI fixtures pass
private index/runtime settings. Initialization does not mutate global environment
variables, concurrent callers share one directory, and normal process exit cleans
only that process's owned directory. Aborts can leave temporary storage. Explicit
`FXI_INDEXES` retains precedence, including non-UTF-8 paths.

A full local `cargo test --all-targets` run passed 990 test executions with zero
added/removed containers or changed container mtimes in real app data; see
[test-storage-sentinel.json](test-storage-sentinel.json). This was a container
metadata check, not a full content audit. Subsequent targeted regressions also
passed. CI now snapshots recursive real app-data metadata around the full suite
on all three operating systems, with `FXI_INDEXES` unset. The guard does not follow
symlinks and has five subprocess regression tests of its own. Its first remote
run caught an additional clean-machine side effect: reading watcher configuration
created the otherwise absent app-data directory. Path resolution now separates
reading from directory creation, and CLI fixtures explicitly override
`FXI_APP_DATA` so they do not inherit user configuration either.

The review's broader history claim—optimization preceded correctness—is not
established by commit-prefix counts. The defects and fixes are documented in the
[audit](../audit-2026-09-18/AUDIT.md); test volume or a successful audit cannot prove
all search behavior correct. The stale blanket speed claims in `CLAUDE.md` were
removed, and the [evidence guide](../README.md) distinguishes current summaries
from retained historical reports. The quoted production-code/unwrap/unsafe/CI
counts were not independently recounted as part of this storage investigation.

The repo had no index at verification time. An intentional `fxi index .` now
creates its source index. Both the installed CLI and current release binary
returned the same 12 `RoaringBitmap` files as independent ripgrep. This is one
explicit repository index, separate from test fixtures; stale registrations were
left untouched.


## Measurement provenance

The `*-routing.json` campaign is the latest measured implementation: checked
paged dictionaries, strong Bloom coverage proofs, checked absence preflight and
lazy path materialization. It uses 65,284 files (1,311,592,608 source bytes), with
manifest `adb3052a8f1cd3f0d7b49c7dff2cc5ad36b831da7a4c238c2f899db85ae57854`.
Raw reports retain binary and harness hashes. These are pinned existing tool
builds, not a survey of the latest releases of every indexing product.

`*-paged.json`, `search-modes.json`, and `default-regression.json` retain the
intermediate paged-only experiment. Files named `*-final.json` are an intermediate
strong-Bloom-proof campaign, despite their filenames. During that campaign's
competitor comparison, the test-pollution investigation read real app-data
metadata; its broad-query samples may have interference. Retain them as history,
but use the subsequent `competitors-routing.json` campaign for comparisons.
Absolute medians across separate campaigns are not controlled causal comparisons.

The experiment exchanges additional publication work and storage for lower
standalone query validation cost. Resident API timings measure requests against
already loaded readers; they cannot be compared directly to another tool's
standalone process. Small sample counts do not establish tail-latency guarantees,
and this single warm corpus does not establish a universal ranking.


## Latest paired results

Standalone files-only medians in milliseconds (21 paired samples):

| Pattern | Previous strict binary | Current experimental |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 32.26 | 10.48 |
| `folio_wait_bit_common` | 34.14 | 17.29 |
| `struct file_operations` | 41.90 | 25.40 |
| `return` | 108.40 | 91.81 |
| `return.*0` | 116.85 | 100.71 |
| `(?i)return.*0` | 162.54 | 146.67 |
| `^static.*void` | 110.95 | 94.57 |

[Raw query samples](queries-routing.json). Comparison below uses 11 randomized
samples per tool/query, identical corpus coverage and exact ripgrep result sets.
All entries include complete process time; source packs are enabled for FXI.

| Pattern | FXI experimental packed | tgrep | csearch | Zoekt | ripgrep |
| --- | ---: | ---: | ---: | ---: | ---: |
| `auditNonexistentSymbol94283` | 10.86 | 17.82 | 4.47 | 56.69 | 3186.04 |
| `folio_wait_bit_common` | 17.40 | 19.06 | 16.08 | 57.52 | 3161.89 |
| `return.*0` | 105.17 | 2183.16 | 1789.34 | 291.14 | 3231.39 |

[Full seven-mode comparison and samples](competitors-routing.json), including
FXI full legacy and experimental lean without source-pack verification. csearch
retains the standalone absent/selective lead. FXI wins the measured broad query
with packs; that is a workload-specific result, not universal superiority.

Three paired build medians: 3.539 → 3.940 s. Index bytes: 941,356,724 → 975,298,224. Median peak RSS: 154.4 → 173.1 MiB.
[Raw build samples](builds-routing.json) retain all samples.

Durable one-file incremental publication medians (five pairs, 4,096 source files,
source packs disabled):

| Inherited segments | Previous (ms) | Experimental (ms) |
| --- | ---: | ---: |
| 1 | 38.86 | 41.24 |
| 64 | 240.27 | 256.23 |
| 256 | 857.25 | 930.54 |

[Raw publication samples](publication-routing.json). Additional generation-wide
evidence issuance remains an update cost; this experiment does not solve it.

Repeated requests (31 randomized samples per mode):

| Pattern | Current standalone CLI | Current resident CLI | Current resident API | Previous resident API |
| --- | ---: | ---: | ---: | ---: |
| `auditNonexistentSymbol94283` | 9.720 | 4.395 | 0.372 | 0.358 |
| `folio_wait_bit_common` | 16.790 | 4.958 | 0.776 | 0.729 |
| `struct file_operations` | 25.785 | 9.596 | 4.799 | 4.759 |

[All modes and first-request observations](search-modes-routing.json). Startup
avoidance is the main gain; the experiment does not demonstrate a material
improvement to already resident query execution.

Default strict reader on the same legacy index (31 pairs):

| Pattern | Previous (ms) | Current (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 32.72 | 30.50 |
| `folio_wait_bit_common` | 34.42 | 32.50 |
| `return.*0` | 118.20 | 116.86 |

[Default-mode samples](default-routing.json). The experimental headline numbers
require rebuilding and searching with `FXI_QUERY_LOCAL=1`; they are not the
default product performance.


Final storage verification at `5dbf46e`: **996 test executions passed**, with the
recursive metadata of **303,769 real app-data entries unchanged**. See
[test-storage-recursive.json](test-storage-recursive.json). The existing watch
daemon was paused for this check and resumed afterward with its loaded state
preserved. An earlier unpaused check correctly detected that daemon updating the
new repository index; it was not counted as a clean isolation result. The remote
Linux/macOS tests also pass the clean-machine storage guard.


## Allocation-light checked routing

The next controlled experiment preserves the existing checked absence policy and
on-disk format. Its root validator borrows the root bytes without creating any
per-posting validation cache or page objects. Bloom checks use a mapped view,
retain the legacy checksum check, and stream the canonical strong digest directly
from little-endian file bytes instead of decoding and copying all words. Tests
compare mapped/owned checksums and strong digests across every supported probe
count; existing changed-root/Bloom/truncation and CLI fallback regressions pass.

Both binaries use `FXI_QUERY_LOCAL=1` on the same immutable checked index in
[31 paired samples](queries-mapped-routing.json): absence **9.519 → 6.677 ms**,
selective **17.253 → 17.175 ms**, broad `return.*0` **100.624 → 100.758 ms**.
These are direct paired results, not subtraction from an earlier campaign.
The gain is confined to the intended negative preflight. No format rebuild or
additional storage is required. This still reads all segment routing dependencies;
it does not eliminate the structural startup cost.


## Generation-wide routing experiment

`FXI_GENERATION_ROUTING=1` additionally opts into a checked, authoritative
`generation-routing.bin`; it requires `FXI_QUERY_LOCAL=1` both when publishing
and searching:

```sh
FXI_QUERY_LOCAL=1 FXI_GENERATION_ROUTING=1 fxi index --force --profile lean PATH
FXI_QUERY_LOCAL=1 FXI_GENERATION_ROUTING=1 fxi -l 're:/literal_symbol/' -p PATH
```

A publisher derives a complete sorted trigram-to-segment-mask
table from strictly validated segment dictionaries. Masks support more than 64
segments. The root binds actual metadata/document/path bytes, ordered complete
segment IDs, and fixed 512-record pages. Each accessed page is checked for hash,
key order/endpoints, nonzero masks and unused high bits before lookup or pruning.

An exact literal can match only in a segment containing every non-stop trigram.
Intersecting the masks can therefore prove absence without opening individual
segments. This remains a necessary condition: nonempty intersection does not
prove a match and falls back to normal search. Stop-gram-only, case-insensitive,
filtered, compound and nonliteral expressions also fall back. Tombstones and
empty postings can only make this summary more conservative.

This mode explicitly changes the query's corruption dependency set. A successful
proof depends on the authoritative router and actual core tables; it does not
inspect original segment dictionaries, gram checks, Bloom filters or postings.
Damage there may coexist with a correct empty answer. Missing/invalid router
proof never establishes absence: normal checked search remains the fallback.
Strict reader opening additionally validates every routing page and the original
segment evidence. Published mappings must remain immutable; hashes do not
provide authentication or detect unindexed source changes.

The new router is rebuilt from independently validated inputs every enabled
publication, rather than trusting an older router. Generation creation inherits
only segment files, so disabling the option for a later publication cannot leave
a stale generation-wide router attached to that new generation.

The controlled corpus has 4,217,171 gram entries across 33 segments, but 323,183
unique grams: a 13.05× overlap. Its router occupies 3,888,454 bytes. A global
presence bitmap alone would not reject `auditNonexistentSymbol94283`: every one
of its trigrams exists somewhere. Segment-mask intersection is empty. Conversely,
every gram of `folio_wait_bit_common` and `struct file_operations` occurs in all
33 segments; segment routing alone cannot improve those positive workloads.

Publication performs a hash-table update per gram entry, sorts unique keys, and
rewrites/fsyncs the summary. One flat mask arena avoids one allocation per key.
This is a measurable build/update/storage tradeoff, not a free accelerator.

## Reusing certified path validation

Query-local readers can reuse publication's UTF-8 and relative-path validation
when the actual path-table bytes match the checked generation manifest. They
still check table lengths/bounds while building path ranges; individual paths
remain lazily materialized. Missing, damaged or mismatched evidence falls back
to complete ordinary validation. Default strict readers retain all checks.

Epoch 1 does not encode the publisher's platform-specific path semantics.
Windows therefore declines this reuse and retains its own component checks:
Unix can accept a filename that Windows interprets as a drive-prefixed path.
Certificate tests mutate every path byte and cover damaged/missing/unsupported
proof, and both policies reject unsafe paths without usable evidence.

The integrated implementation passed 1,017 local test executions, Clippy and
MSRV 1.88 checking before measurement. CLI regressions compare both profiles,
positive/absent and unsupported queries, updates/deletion, compaction and damaged
or missing optional routing files. Unit routing tests cover mask boundaries,
empty generations, checked-page boundaries, malformed counts, metadata/core
changes and independence from damaged unused segment evidence.

## Process startup calibration

[31-sample calibration](startup-floor.json) measured `fxi --version` at 3.629 ms
and empty-index checked absence at 4.054 ms, including process launch/reaping.
These are observed floors for this binary/environment, not physical lower bounds.
They motivated inspecting macOS executable linkage: the native FSEvents watcher
pulls CoreFoundation/CoreServices into the same executable used for one-shot
search. A separate helper experiment will test whether moving daemon execution
out of the CLI improves that cost while retaining native watching.


## Generation/path measurements before the helper experiment

31 paired standalone samples, both binaries using checked query-local validation:

| Pattern | Mapped-routing baseline (ms) | Certified paths only (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 6.479 | 6.600 |
| `folio_wait_bit_common` | 16.479 | 12.631 |
| `struct file_operations` | 25.023 | 21.245 |
| `return.*0` | 101.711 | 98.927 |

[Raw path-isolation comparison](queries-certified-paths.json). Both use the same
existing index with generation routing disabled. The absence difference is small;
this optimization targets ordinary reader initialization.

On fresh paired indexes, additionally enabling generation routing:

| Pattern | Mapped-routing baseline (ms) | Router + certified paths (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 6.390 | 4.215 |
| `folio_wait_bit_common` | 16.331 | 12.773 |
| `struct file_operations` | 24.728 | 20.958 |
| `return.*0` | 98.355 | 95.287 |

[Raw generation comparison](queries-generation.json). The analogous mixed-tool
[11-sample campaign](competitors-generation.json) still leaves csearch ahead on
absence (4.10 versus FXI packed 5.38 ms), while FXI wins the selective symbol
(13.56 versus 15.85 ms) and packed broad query (99.40 versus 286.07 ms for Zoekt).
Do not infer the reason for the difference in absolute latency between campaigns.
A pre-existing watch daemon remained active during these intermediate campaigns;
the final helper comparison will pause it and avoid repository mutations during
timing. These are retained intermediate evidence, not the final controlled result.

Three paired builds: 3.740 → 3.956 s; median peak RSS 154.3 → 170.9 MiB. Index bytes increase by 3,888,454 (about 0.4% of the existing checked packed index).
[All build samples](builds-generation.json) retain the slower third pair for both
binaries. These costs are incremental over the checked format, not over the original
strict format.

Synthetic publication medians (five paired samples, 4,096 files):

| Inherited segments | Checked baseline (ms) | + generation routing (ms) |
| --- | ---: | ---: |
| 1 | 41.493 | 47.457 |
| 64 | 249.013 | 250.983 |
| 256 | 900.446 | 897.749 |

[Publication samples](publication-generation.json). This small, repetitive corpus
does not establish publication cost on the 1.3 GB Linux corpus.


## macOS daemon-helper experiment

The CLI now keeps daemon implementation and native FSEvents frameworks in a
sibling `fxid` executable. `fxi daemon` remains the public interface; foreground
execution uses `exec` to preserve PID/signals, background execution retains the
existing double-fork and readiness handshake. Helper resolution uses the sibling
of the actual CLI executable, never arbitrary PATH fallback. Linux/Windows CLI
daemon execution remains monolithic in this iteration.

The integrated CLI links libiconv/libSystem; the helper retains
CoreFoundation/CoreServices. Both executables must be installed together for
macOS daemon commands. Cargo build/install includes both; clean `cargo run --
daemon ...` needs `cargo build --bins` first. Missing/mismatched helpers fail with
an actionable error. The combined package is larger than the prior single binary
(about 8 MB in the initial split build, versus about 4.3 MB).

The integrated full suite passed 964 executions, plus Clippy and MSRV 1.88. The
count falls because macOS CLI compilation no longer duplicates daemon unit tests;
the library retains its daemon tests. New process tests cover relocation, spaces
in paths, missing helper/no PATH fallback, foreground PID/signals and exit codes,
native watching, final-save persistence and background startup. CI also installs
both binaries and exercises an installed watched daemon. All eight CI jobs passed
for `0683a00`.

For this campaign, the pre-existing watch daemon was paused and resumed in a
`finally` block. No compilation ran during timed measurements; pausing the daemon prevented
background indexing of repository changes.
Both binaries use identical index bytes and policy flags in each paired test.
[31-pair startup calibration](startup-helper.json):

| Case | Monolithic CLI (ms) | Split CLI (ms) |
| --- | ---: | ---: |
| version | 3.477 | 2.564 |
| empty_checked_absence | 3.780 | 2.837 |

The split saves about a millisecond, not the whole startup floor. On the checked
generation-routing index, [31 query pairs](queries-helper.json) show:

| Pattern | Monolithic CLI (ms) | Split CLI (ms) |
| --- | ---: | ---: |
| `auditNonexistentSymbol94283` | 4.286 | 3.297 |
| `folio_wait_bit_common` | 13.092 | 11.815 |
| `struct file_operations` | 21.627 | 20.236 |
| `return.*0` | 97.045 | 95.469 |

[Default strict-mode control](default-helper.json) and the
[held-out Redis control](redis-helper.json) use legacy indexes with experimental
validation/routing disabled. Redis absence improved 5.240 → 4.187 ms and `raxFind`
5.669 → 4.630 ms; the roughly one-millisecond gain therefore extends beyond the
experimental format and Linux corpus.

Full process comparison, 11 randomized samples and exact result-set checks:

| Pattern | FXI experimental packed | tgrep | csearch | Zoekt | ripgrep |
| --- | ---: | ---: | ---: | ---: | ---: |
| `auditNonexistentSymbol94283` | 4.01 | 17.82 | 4.30 | 57.05 | 2657.91 |
| `folio_wait_bit_common` | 12.53 | 19.37 | 16.24 | 57.55 | 2671.87 |
| `return.*0` | 101.21 | 2169.29 | 1813.54 | 291.49 | 2904.60 |

[Full seven-mode samples](competitors-helper.json). Absence is a near tie with
csearch, not a decisive win. The selective and packed broad workloads show larger
leads. These are pinned tools on one warm corpus/platform, not universal rankings.

## Full-corpus publication cost

The extended publication harness copies source bytes into a private corpus (never
hardlinks them, which would change the original source ctimes), adds one probe file,
and verifies the original manifest after removing it. Each timed variant receives
a private copy of the same prepared index; build/copy/oracle work is outside timing.
A 32-file smoke run checked this harness path before the full experiment.

The copied Linux corpus retains the exact 65,284-file, 1,311,592,608-byte manifest.
With lean indexes, source packs disabled and 32 inherited segments, three paired
one-file durable updates measured **568.81 → 649.51 ms** from the checked mapped
baseline to the current helper + generation router: **+14.2%**.
[All samples and traces](publication-linux-generation.json) retain the complete
commands and publication breakdown. This update regression is real; the router
currently rebuilds its whole summary for one added file. Reusing an unchanged
summary safely is a remaining architecture task, not an achieved optimization.


## Rejected linker experiment

A macOS CLI-only `-Wl,-dead_strip_dylibs` build removed the unused libiconv
dependency while retaining libSystem. The helper binary was unchanged, and
process smoke checks passed. However, [31 paired samples](startup-linker.json)
measured `--version` at 2.517 → 2.475 ms and checked empty absence at
2.774 → 2.760 ms. The sample ranges overlap substantially; 19/31 pairs favored
the candidate in each case. This does not justify another platform-specific
linker setting, so the flag was discarded.
