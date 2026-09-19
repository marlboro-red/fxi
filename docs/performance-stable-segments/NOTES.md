# Stable segment objects: opt-in storage experiment

This experiment targets durable-update work proportional to inherited segment
files. It does not replace global document/path tables, remove full filesystem
reconciliation, or change search semantics. The control is the single-executable
`a7ff31e` implementation; neither layout is presumed faster before measurement.

## Storage contract

With `FXI_STABLE_SEGMENTS=1`, new publications store immutable segments in
`<container>/objects/<unique-generation-and-segment-name>`. Generation metadata
contains the complete segment-ID/object-name map. Format versions 4 (full) and
5 (lean) prevent previous executables from interpreting the layout as legacy.
Current-generation metadata, document/path tables, and reader leases stay in
`generations/`. A stable index keeps its layout on subsequent updates and
compaction even without the environment variable. A forced rebuild with
`FXI_STABLE_SEGMENTS=0` produces the existing layout.

Existing object references are reused; new local segments are synced and moved
into the object store. Migration preserves published legacy directories through
private staging links/copies, so already-open readers can lazily access them.
Object bytes, object-store directory entries and its container entry become
durable before the referencing generation and CURRENT are published. Existing
Windows directory-sync limitations remain; this does not claim stronger power-loss
guarantees than the underlying platform implementation provides.

Generation leases protect the complete object reference set. After CURRENT is
durable, cleanup removes unpinned retired generations, syncs their parent, then
marks objects referenced by every remaining generation before sweeping orphans.
Unreadable/incomplete manifests or unknown store entries stop object collection.
The `objects.check` digest binds exact published metadata bytes, so a valid-shaped
reference mutation cannot make GC discard an object used by an older pinned
reader. Missing/corrupt binding evidence or dangling references aborts the sweep.
This detects accidental corruption; it is not authentication against coordinated
rewriting of metadata and its digest. Cleanup failure preserves publication and
may retain extra space. `prune` validates the object layout and leases before
removing abandoned registrations; stats includes referenced object payloads.

The root writer lock remains the serialization boundary for publication and GC.
The public low-level writer/compaction APIs retain their existing caller-locking
requirement. No symlinks, platform-specific object links or mutable reference
counts are introduced by the new layout.

## Deliberate experimental boundary

The initial prototype rejected checked and negative-routing publication because
it could add sidecars inside inherited objects. The combined-mode follow-up below
removes this restriction by completing new segment proofs before export and
issuing generation-owned certificates afterward. Inherited objects stay read-only.
Ordinary strict search, full/lean profiles, source capture/packs, updates,
compaction, stats and prune are supported. The default layout is unchanged.

## Validation and measurement

The subsequent [lifecycle validation](../validation-stable-segments/NOTES.md)
adds actual process termination at publication/GC boundaries, overlapping readers,
and sustained CLI histories. It found and fixed an empty-generation orphan that
could prevent object reclamation after a crash. The format remains opt-in.

Coverage includes full/lean update and deletion, legacy migration, shared object
identity, pinned generation retention across compaction and later collection,
lazy old-reader token/line-map access, returned writer metadata, packed capture,
missing/traversing/aliased object references, malformed/incomplete manifests,
valid-shaped reference substitution to an existing object, failed-publication
orphans, unsupported routing policy and conservative prune. Crash-state tests
model failure before CURRENT; they are not a hardware power-loss campaign.
Generated CLI oracle tests exercise 128 cases per profile/layout across text,
JSON, counts, files, scopes, Unicode, word matching, regex and fixed-string modes.

The publication harness prepares each layout separately from the same source,
then alternates timed one-file updates from private copies of those prepared
indexes. Timings exclude fixture copying and initial layout migration. Exact
old/new result sets, document/segment counts and source manifests are checked.
A retained private Linux fixture permits a separate paired strict-search control.


## Initial measurements

Warm filesystem, the same M2 Max Mac, frozen release binaries, no compilation
or test processes, and the unrelated watch daemon paused/resumed in a finally
block. [Synthetic publication](publication-small.json) uses seven alternating
pairs; [Linux publication](publication-linux.json) uses eleven. All reported
publication timings include strict validation and durable publication.

| Inherited segments / fixture | Control ms | Stable objects ms | Faster pairs |
| --- | ---: | ---: | ---: |
| 1 / 4,096 files | 34.95 | 43.02 | 1/7 |
| 64 / 4,096 files | 185.85 | 54.42 | 7/7 |
| 256 / 4,096 files | 670.15 | 102.61 | 7/7 |
| 32 / 65,284 Linux files | 308.54 | 288.55 | 9/11 |

The 64- and 256-segment fixtures improve 3.4× and 6.5× respectively, but the
single-segment fixture regresses 23%. Linux improves 6.5%. These small samples
are not tail-latency guarantees or cross-platform measurements. Trace medians
confirm inherited-tree work drops from 141.3/569.8 ms to 0.18/0.22 ms in the
fragmented controls, and from 32.2 to 0.23 ms on Linux. Whole-update latency
still includes scanning, opening/validating the base, rewriting metadata and GC.

[31-pair Linux query controls](queries-linux.json) are effectively flat:
absent 29.971 → 30.004 ms, selective 31.836 → 31.653 ms, broad regex
615.943 → 617.464 ms. The [normal-layout binary control](queries-legacy-control.json)
also remains broadly flat (29.610 → 29.798, 31.820 → 31.509,
616.526 → 614.282 ms). These are strict lean indexes without source packs;
they must not be compared as if they were the experimental checked/packed
configuration in earlier competitor tables.

The [compatibility probe](compatibility.json) confirms the previous executable
rejects the new version explicitly. Initial validation passed 1,110 all-target
test executions, Clippy with warnings denied, Rust 1.88, rustfmt, and 11 Python
harness tests. Production index registrations had no additions or removals.

An additional isolated native-daemon contract passed for stable storage: update
visibility, compaction/reload, removal, and persistence of a final save during
graceful watched shutdown.


## Follow-up: avoid syncing cleanup directories when nothing changed

Publication already syncs `generations/` before replacing CURRENT. Its second
sync is now conditional on an actual retirement attempt. The flag is set before
removal, even if removal fails after partial progress. A required sync failure
still prevents object collection. Object collection returns without another
object-store sync when its garbage set is empty. Tests inject removal and sync
failure and verify that referenced/orphan objects are not reclaimed prematurely.

A fresh complete campaign against the same original `a7ff31e` control produced:

| Inherited segments / fixture | Control ms | Final candidate ms | Faster pairs |
| --- | ---: | ---: | ---: |
| 1 / 4,096 files | 35.72 | 40.91 | 0/7 |
| 64 / 4,096 files | 192.00 | 51.84 | 7/7 |
| 256 / 4,096 files | 660.77 | 91.27 | 7/7 |
| 32 / 65,284 Linux files | 311.68 | 285.13 | 10/11 |

[Raw synthetic samples](publication-small-sync.json) and
[raw Linux samples](publication-linux-sync.json) include output and phase traces.
The final candidate is **3.7× faster at 64 segments, 7.2× faster at 256 segments,
and 8.5% faster on Linux** in this campaign. It remains **14.5% slower at one
segment**. These are whole-update measurements, not just the removed phase.
The two campaigns independently support the fragmentation benefit; differences
between their medians do not isolate the cleanup tweak's effect on their own.
The layout remains opt-in, rather than promoting a small-index regression.

Final [31-pair query controls](queries-linux-sync.json) measure absent
30.054 → 30.139 ms, selective 31.765 → 31.948 ms, and broad regex
652.881 → 623.975 ms. The [normal-layout binary control](queries-legacy-control-sync.json)
measures 29.778 → 29.809, 31.670 → 31.489, and 634.698 → 627.438 ms.
Given the earlier flat broad-query campaign and variation in both controls,
these results support no material search regression, not an established new
broad-query speedup. [Old-binary rejection](compatibility-sync.json) also passes.

This finishes the first storage experiment, not the proposed architecture work:
metadata still rewrites globally, strict readers still validate inherited posting
evidence, and GC scans generation references and object names. Incremental
metadata, checked-proof compatibility, bounded maintenance and query-local
validation remain separate experiments with their own correctness gates.

Final follow-up validation passed 1,115 all-target test executions, Clippy,
Rust 1.88, rustfmt and all 11 Python harness tests. The first storage commit
(`703e981`) passed all eight CI jobs, including Windows, macOS and Linux.
Both benchmark campaigns resumed the existing watcher. Private retained
benchmark corpora/indexes were removed after their query controls completed.


## Rejected experiment: write final metadata before object flushing

An additional prototype moved all new objects and wrote final metadata/check
before flushing any object tree. All object, directory and CURRENT durability
barriers remained. Injected object-sync failure tests confirmed that old CURRENT
remained unchanged and abandoned objects were reclaimable. The goal was to let
the filesystem combine more unpublished writes into one flush wave.

The [11-pair screen](publication-small-batched.json) measured 32.274 → 43.335 ms
at one segment, 192.517 → 54.302 ms at 64, and 692.786 → 94.842 ms at 256,
against the same original control. It did **not** remove the small-index penalty
or show a compelling benefit beyond the retained implementation. Cross-campaign
variation prevents attributing the differences versus the earlier candidate
solely to write ordering. We reverted this experiment and did not spend another
large-corpus campaign on it. The [patch](rejected-batched-writes.patch), applied
to `7784a15`, records the exact code and regression test behind the frozen binary
whose checksum appears in the report. Final production code remains `7784a15`.

The retained code commit `7784a15` also passed all eight GitHub CI jobs, including
Windows, macOS and Linux ([run](https://github.com/marlboro-red/fxi/actions/runs/35417456112)).


## Combined checked search and stable objects

Publication now has two proof phases. First, strictly validate staging data and
inherited objects against the new generation's document membership. Create gram
and Bloom proof files only inside new, private segment directories. Next, export
and sync new objects and finalize the version 4/5 reference manifest. Finally,
issue query-routing and generation-routing certificates against the final
metadata/docs/paths. Negative-routing certificates are also generation-owned and
issued after export. The existing durability barriers and reader leases remain.

No inherited object is modified to add missing proofs. Enabling checked mode on
an existing strict stable index leaves old proof-less objects intact; posting
validation falls back until compaction that actually rewrites segments, or a
forced rebuild, creates new objects with proofs. A no-op compaction adds nothing. Generation-wide absence evidence can still be issued independently.
Malformed mandatory evidence prevents publication; invalid optional certificates
remain ineligible rather than being rewritten inside inherited objects. Steady-state
stable references avoid hard-link count changes that invalidate Unix stamp-based
negative certificates. Migration can retain legacy hard links temporarily: retiring
that legacy generation can change object ctime once and cause safe negative-proof
fallback. Byte-hash checked/generation certificates are unaffected.

To build and search with the combined experiment on Unix, keep these options in
the environment for indexing, updates, compaction, and searches:

```sh
export FXI_STABLE_SEGMENTS=1
export FXI_QUERY_LOCAL=1
export FXI_GENERATION_ROUTING=1
export FXI_SOURCE_PACK=1
export FXI_SOURCE_PACK_COMPRESSION=1
fxi index --force --profile lean PATH
fxi -l --regex 'pattern' PATH
```

The defaults and format versions are unchanged. Source packs remain Unix-only;
checked stable publication itself is portable. Query-local and generation-routing
validation retain their documented integrity boundaries: checks can skip evidence
irrelevant to a query, unlike a full strict validation pass. Publication still
validates inherited postings and rebuilds global routing/document/path metadata;
this change does not make durable updates proportional only to changed bytes.

Validation adds full/lean checked lifecycle and migration tests, byte-for-byte
inherited-object snapshots plus Unix mtime/ctime checks, pinned-reader retention,
missing-proof fallback, corruption rejection with unchanged CURRENT, compressed
packs and negative routing. Certificates are checked against final metadata hashes
so an invalid certificate followed by a correct slow fallback cannot mask an
issuance-order bug. Generated CLI tests add 128 cases per checked stable profile.
The process-kill matrix now covers strict and checked publication: 164 scenarios.


### Combined-mode measurements

The same frozen candidate executable is used for both layouts, isolating storage
layout from binary changes (SHA-256
`4052b0b9f1a02d5cd9f07d176de90377606e71363ec1b31444e483894179745a`).
Each arm builds its own checked index with generation routing enabled. The
publication harness asserts legacy versions 2/3 versus stable versions 4/5 before
measurement; candidate flags must not accidentally configure the legacy control.
Source-pack settings are explicit in both arms and regression-tested.

Eleven alternating update pairs per fixture, warm filesystem on the same Mac,
no compilation/tests during timing, and the unrelated watch daemon paused with a
`finally` resume guard. Every update verifies exact old/new matching-file sets
against source expectations, document/segment counts, and the corpus manifest.

| Fixture | Checked legacy ms | Checked stable ms | Stable faster pairs |
| --- | ---: | ---: | ---: |
| 1 segment / 4,096 files, full, no packs | 37.37 | 43.18 | 0/11 |
| 64 segments / 4,096 files, full, no packs | 249.80 | 61.79 | 11/11 |
| 256 segments / 4,096 files, full, no packs | 913.28 | 137.85 | 11/11 |
| 32 segments / 65,287 Linux files, lean, compressed packs | 504.64 | 466.21 | 9/11 |

[Raw synthetic samples](checked-publication-small.json) and
[raw Linux samples](checked-publication-linux.json). The fragmented cases improve
4.0x and 6.6x; Linux improves 7.6%. The one-segment case regresses 15.5%, so the
combined experiment still does not justify default promotion on performance alone.
The Linux fixture contains all 65,287 source files, not the 65,284-file common
subset used in earlier competitor tables. These are FXI layout comparisons,
not a fresh competitor ranking or cross-platform performance evidence.

[31-pair search controls](checked-queries-linux.json), complete case-sensitive
regex files-only results, checked against ripgrep on every sample:

| Query | Checked packed legacy ms | Checked packed stable ms |
| --- | ---: | ---: |
| Absent identifier | 4.256 | 4.228 |
| Selective identifier | 11.026 | 10.795 |
| `return.*0` | 97.421 | 96.959 |

The query medians are effectively unchanged: the combined layout retains the
fast search path while reducing fragmented publication work. Small differences
are not established search speedups. The corpus manifest was rechecked before
and after the query campaign, and private retained indexes/source copies were
removed after measurement. Eleven update samples do not establish p99 latency.


### Combined-mode correctness validation

The final frozen candidate passed the [checked, compressed-packed lifecycle
campaign](checked-lifecycle.json.gz): 1,000 updates per profile, 40 explicit
compactions total, and 5,786 independent source-oracle queries comparing 1,166,220
matching rows. Each profile peaked at 15 segment objects and ended with exactly
the two objects referenced by CURRENT, with zero unreachable objects. The two
final reclamation updates are additional to the 2,000 history updates. This is
small-corpus sustained validation, not a large-corpus throughput benchmark.

The [process-kill report](checked-crash-matrix.json.gz) records 164 scenarios and
1,160 source-oracle checks across strict/checked full/lean publication. These are
process terminations, not power-loss simulations. Local validation passed 1,141
all-target Rust test executions, Clippy with warnings denied, Rust 1.88, rustfmt,
and 21 Python harness tests. Real production registry entries remained unchanged
at 19,881, and the unrelated watcher was resumed after measurement.

Reproduce the layout comparison using the same frozen executable for both arms:

```sh
python3 scripts/compare-publication.py --baseline /path/to/fxi --candidate /path/to/fxi \
  --baseline-query-local --candidate-query-local \
  --baseline-generation-routing --candidate-generation-routing \
  --candidate-stable-segments --repetitions 11 --output /tmp/checked-small.json
```

Add `--corpus /path/to/linux-source --source-pack --keep-fixture` for the large
compressed-packed run. The report records the retained baseline/candidate indexes;
pass those to `scripts/compare-startup.py` with both checked and generation-routing
flags, the same binary, 31 repetitions, and patterns `auditNonexistentSymbol94283`,
`folio_wait_bit_common`, and `return.*0`. Enable `FXI_SOURCE_PACK=1`, use private
application data/socket settings, verify the source manifest again after querying,
and remove only the experiment's retained fixture afterward.

The combined mode is useful and remains opt-in. It resolves compatibility between
these experiments; it does not establish default-mode superiority, newest-version
competitor rankings, native cross-platform speed, or bounded update tail latency.
