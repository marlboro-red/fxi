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

Stable publication currently rejects `FXI_QUERY_LOCAL=1`,
`FXI_GENERATION_ROUTING=1`, and enabled negative routing. Existing checked-proof
publication can add sidecars in inherited segment directories, which would
violate object immutability. Supporting checked publication requires a separate
proof-publication design; this experiment must not silently mutate shared objects.
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
