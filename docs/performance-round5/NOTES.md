# Round five: source verification and selective position evidence

This round starts at `a92ec0d` on the same M2 Max and controlled Linux fixture.
The CI repair is `82ddc17`. Performance binaries use the same local Rust 1.94
compiler; lint validation additionally uses Rust 1.98, matching current CI.
All measured search outputs are checked against ripgrep. Compilation and local
tests run outside timing windows. These are warm-filesystem experiments on one
machine, not cold-storage or concurrent-client claims.

## CI repair

GitHub run 35204301385 failed at new Rust 1.98 Clippy lints. The local default
compiler was 1.94. Replace fixed-size `chunks_exact` iteration with `as_chunks`,
drain-and-collect with `mem::take`, and a nested Windows conditional with a let
chain. The posting-codec example uses the same iteration fix. Rust 1.88 remains
supported. Run 35218564235 passed Linux, macOS, Windows, formatting and MSRV.
Benchmark harness regression tests are also being added to CI, including checks
that every timed subprocess receives the intended index and parallelism settings.

## Establish the bottleneck

`linux-stage-profile.json` separates planning, posting lookup and warm execution:

| Literal | Candidate files | Matching files | Candidate source bytes | Lookup ms | Engine ms |
|---|---:|---:|---:|---:|---:|
| `struct file_operations` | 3,788 | 1,240 | 217,812,338 | 0.968 | 7.404 |
| `folio_wait_bit_common` | 44 | 1 | 5,494,985 | 0.144 | 0.345 |
| `return` | 44,318 | 44,261 | 828,744,201 | 0.484 | 42.805 |

Broad `return` has almost perfect candidate precision. Further pruning cannot
remove most of its work; reading/verifying source and returning paths dominate.
The phrase has 3.05x candidate amplification, leaving useful pruning potential.

The metadata probe originally used ceiling division for Rayon minimum chunks.
That could produce two tasks instead of four when the file count was not
divisible by four. `linux-metadata-stage-ceiling-probe.json` retains that limited
probe; do not use its broad-query metadata timing as the production task policy.
Both the corrected profile and source-pack lab use the executor's floor-based
batch sizing. The corrected profile, with the rare-anchor snapshot prototype,
measures phrase metadata alone at 2.337 ms, lookup at 0.974 ms and engine at
4.671 ms; broad metadata at 27.064 ms and engine at 38.097 ms. These are separate
loops, not additive profiling spans. FXI validates editable source metadata;
Zoekt's immutable indexed-content search does not perform that same work.

## Rejected concurrency and reusable-buffer changes

Eleven paired direct samples with unchanged binaries:

| Read-task setting | Broad `return` ms | Phrase ms |
|---|---:|---:|
| Four, first comparison | 431.590 | 44.449 |
| Twelve | 702.716 | 38.238 |
| Four, second comparison | 434.748 | 44.576 |
| Two | 543.371 | 59.314 |

Keep the four-task default. More outstanding file reads hurt the broad workload.
The harness now records each binary's separate task setting and tests that it
reaches all warmup and measured subprocesses.

Reusing a worker-owned String for uncached verification passed all tests but was
effectively neutral over 21 paired samples: broad 449.228 -> 449.865 ms, phrase
43.852 -> 43.095 ms. It was reverted; `reusable-read-buffer.patch` and
`linux-scratch-direct.json` preserve the experiment.

## Lazy position evidence in immutable cached files

The candidate implementation keeps source text in a boxed string inside a shared
snapshot, avoiding the previous copy into inline `Arc<str>` storage. Each snapshot
can lazily retain complete byte positions for at most two trigrams, at most 32
positions each. Higher occurrence counts are cached as Overflow and use the
ordinary matcher. Small files and short literals bypass this mechanism.

For a proven exact, nonempty, line-local literal, select the gram with the lowest
summed indexed document frequency. Omitted stop-grams are excluded. Align each
known occurrence with that gram's query offset and verify the whole literal's
bytes at that location. This can reuse evidence across different queries; it
does not store whole-query answers. The content cache still validates metadata
before every reuse. An edited file gets a new snapshot and fresh evidence.

All positions include overlapping occurrences. Checked subtraction and bounds
checks handle file edges; overflow never masquerades as a complete prefix.
Evidence is owned by the source snapshot: weak references to inline `Arc<str>`
would pin its entire allocation after eviction, which this design avoids.
Retained text keeps the existing byte budget; at most 256 bytes of position
payload plus bounded entry/allocation metadata are added per snapshot. In-flight
snapshots can outlive cache admission, as before. The public Rust
`FileContent::Cached` payload now wraps `SourceSnapshot`; dereferencing still
returns the source string. There is no on-disk format change.

Twenty-one paired warm samples (`linux-rare-position-warm.json`):

| Literal | Before ms | Candidate ms |
|---|---:|---:|
| `struct file_operations` | 13.901 | 11.633 |
| `const struct file_operations` | 13.658 | 11.470 |
| `file_operations` | 13.731 | 11.423 |
| `static const struct` | 51.674 | 45.794 |
| `unsigned long flags` | 21.439 | 18.409 |
| `folio_wait_bit_common` | 4.119 | 4.148 |
| `return` | 62.825 | 61.104 |

The preceding fixed-hash anchor prototype is recorded separately in
`linux-position-cache-warm.json`. Its phrase improvement was smaller, about 14%.
Neither version claims a universal gain or new indexing theory. Regression tests
cover overlap, overflow, bounded replacement, concurrency, Unicode and file
boundaries, plus real cached queries after same-size edits, invalid UTF-8 and
deletions. Existing result-limit, line-filter and invalid-regex checks remain.

The final 21-sample Linux repeat (`linux-position-cache-final-warm.json`)
confirms phrase **14.128 -> 11.850 ms** (16% lower latency) and `unsigned long
flags` **21.618 -> 18.810 ms** (13% lower). The direct-query control is effectively
unchanged: broad 429.539 -> 431.325 ms, phrase 44.809 -> 44.726 ms, selective
12.995 -> 13.060 ms, absent 11.246 -> 11.318 ms. No one-shot improvement is claimed.
Redis's four warm controls are essentially unchanged; CPython's `static int`
improves 5.890 -> 5.698 ms, while `return` regresses 7.421 -> 7.587 ms and the other
two controls are essentially unchanged. These are modest, workload-specific gains.
The final implementation passes 697 all-target tests, strict Clippy on Rust 1.98,
Rust 1.88 checks, formatting and the benchmark harness tests.

## Offline selective byte positions and residue masks

`examples/position_lab.rs` streams each source file and simulates query-independent
gram selection and capped exact offset lists. Missing, overflowed or invalidated
evidence is Unknown, never a proof of absence. It evaluates twelve fixed literals,
not just the phrase that motivated the experiment. Every actual matching file
must survive every policy. Existing token ordinal positions cannot safely replace
these byte positions under substring semantics.

The initial 12 policies are in `linux-position-lab.json`. The extended 24-policy
probe in `linux-position-residue-lab.json` adds 64/128/256-bit positional summaries
for overflowing lists. Bit `p mod M` records an occurrence at byte offset `p`.
Rotate each mask by its query offset and intersect: a real phrase start must
remain in the intersection. Empty intersections safely reject; collisions only
retain extra candidates. Saturated masks are omitted as Unknown. Complete exact
anchors can further test absolute starts against the residue intersection.

Selected space/phrase-filter observations (extra MiB, not total index size):

| Policy | Extra MiB | Phrase survivors, all-known | True matches |
|---|---:|---:|---:|
| Hash 1/8, exact cap 16 | 113.2 | 3,471 | 1,240 |
| Hash 1/4, exact cap 16 | 232.0 | 2,031 | 1,240 |
| Hash 1/4, cap 4 + 64-bit residues | 241.5 | 1,805 | 1,240 |
| Hash 1/4, cap 4 + 128-bit residues | 308.0 | 1,558 | 1,240 |
| All grams, exact cap 16 | 915.7 | 1,480 | 1,240 |

These are exact size calculations for a specified hypothetical tagged VByte
encoding, excluding integrity metadata, alignment and skip structures. They are
not serialized production indexes or query-latency measurements. All-known
filtering can require many additional posting lookups; fixed-pair results are
also retained and are generally less selective. Tests cover wraparound, aliases,
overlapping occurrences, Unicode, overflow, saturation, invalidation and encoding.
This is an experiment with approximate positional summaries, not a novelty claim.

Across one execution of each of the twelve literals, perfect filtering could
remove only 10.91% of candidate-file visits. The 241.5 MiB hybrid removes 5.32%
of those visits and 8.85% of candidate bytes; the 952 MiB all-gram/64-bit hybrid
removes 10.12% of visits and 17.87% of bytes. These workload aggregates are not
latency predictions. They argue against shipping a large default sidecar based
on the phrase alone. A further encoding experiment could retain exact lists
whenever their VByte payload is smaller than a fixed residue mask.

## Packed source: large verification gain, large storage cost

`examples/source_pack_lab.rs` creates a private immutable uncompressed source pack.
Each candidate still checks current metadata, falling back to ordinary source
reads after a changed stamp. Both paths validate complete UTF-8. Eleven paired
verification-stage samples, with exact ripgrep result parity:

| Literal | Separate files ms | Packed source ms | Ratio |
|---|---:|---:|---:|
| `return` | 407.298 | 38.291 | 10.6x |
| `struct file_operations` | 30.967 | 6.906 | 4.5x |
| `folio_wait_bit_common` | 0.501 | 0.198 | 2.5x |

The pack adds **1,251.2 MiB** of source bytes, plus metadata not represented by a
serialized format here. Copying took 2.046 seconds, but did not `sync_all`; that
is not a durable production build cost. The freshly copied pack and source files
are warm. Index open/planning, path sorting, CLI startup and serialization are
outside these measurements. The pack is a private tempfile, not a deployed index
with checksums, versioning, incremental updates, compaction or recovery.

The result supports investigating an optional packed/compressed source store for
broad one-shot search. It does **not** establish an end-to-end win over Zoekt.
Production integration must price integrity validation, build/update cost, disk
footprint, cold storage and the existing point-in-time source-race semantics.
The probe includes edit, deletion and invalid-UTF-8 fallback tests.

## Reproduction

Use the existing controlled corpora/indexes from rounds two and four. Public
sources remain pinned shallow clones. `lab-provenance.json` records example and
binary hashes. Build examples before measurements, then run through
`scripts/quiet-benchmark.py`; capture the child program's JSON separately from
the wrapper's informational output. For example:

```sh
cargo build --release --example position_lab --example source_pack_lab
FXI_INDEXES=/path/to/indexes target/release/examples/position_lab /path/to/corpus
FXI_INDEXES=/path/to/indexes target/release/examples/source_pack_lab /path/to/corpus
```

The raw paired search files also record binary/harness hashes and source/index
paths. No unmeasured claims are made about livegrep, indexed ugrep, OpenGrok,
distributed services, other hardware, or simultaneous search/update workloads.


## Windows CI regression and correction

Run 35220348323 exposed a stale-cache result after a rapid same-size source
rewrite on Windows. The six-byte query did not use positional evidence. The
existing non-Unix stamp (size, modification time, creation time) could compare
equal despite different content. Cache hits now reread complete UTF-8 bytes
outside the shard mutex on non-Unix platforms. Identical bytes preserve the
snapshot and its evidence; changed or unreadable sources discard that snapshot
without evicting a concurrent replacement. Unix cache hits retain their existing
identity/change-time validation and fast path.

The daemon regression now restores the original modification time after each
same-size rewrite. A platform-independent test exercises byte revalidation,
unchanged snapshot identity, invalid UTF-8 and deletion. The offline source-pack
lab falls back to ordinary reads on non-Unix platforms. Its reported speedups
remain Unix measurements; no Windows packed-source acceleration is claimed.
