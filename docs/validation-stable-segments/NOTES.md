# Stable segment lifecycle validation

The opt-in immutable-segment experiment needed evidence beyond short update
benchmarks. This campaign tests interrupted publication, live reader leases,
and repeated source changes against independent source scans. It found two
reclamation defects and a Windows reader-open failure during publication.
None was a demonstrated wrong search result.
The layout remains opt-in. Performance comparisons and its single-segment
regression remain in the [storage experiment](../performance-stable-segments/NOTES.md).

## Defects found

1. Killing a writer after creating its generation directory but before creating
   the lease left an empty directory that generation cleanup skipped forever.
   Its missing metadata then stopped object collection. Cleanup now removes only
   empty, recognized, real generation directories without a lease, under the
   existing root writer-lock contract. Nonrecursive removal preserves directories
   that acquire contents; unknown names, symlinks, and nonempty damaged trees are
   preserved. The latter can still stop collection intentionally.
2. A source-packed deletion-only delta retained an unused `CaptureWriter` staging
   lease through publication. Its incomplete staging generation blocked object
   collection, leaving objects retired by previous compaction. The packed history
   exposed this at step 66, immediately after automatic compaction. Consume or drop
   the capture before publication, including when there are no new documents.
   A later addition or compaction could reclaim the space, but a deletion-only
   history should not depend on that unrelated future operation.
   The [original failure report](lifecycle-packed-before-fix.json.gz) retains
   command samples and the failure; that harness revision did not retain the
   failing step's storage snapshot before raising. Subsequent runs do.
3. Windows CI's overlapping-reader test failed with `Access is denied` while
   opening a reader during publication. A delete-pending lease is a plausible
   cause: Windows may return that error instead of `NotFound`. Lease acquisition
   now retries other errors only when a successful re-resolution shows CURRENT
   moved to another generation. An unchanged or unresolvable CURRENT preserves
   the original error, retries remain bounded at eight, and content validation
   is unchanged. Deterministic tests cover transition, persistent denial, failed
   re-resolution, and the retry bound; native Windows concurrency is the
   integration check. The initial failing run is
   [35437823113](https://github.com/marlboro-red/fxi/actions/runs/35437823113).

## Interrupted publication and overlapping readers

[Crash matrix](crash-matrix.json): **82 actual child-process terminations**,
covering full and lean initial builds, updates, compaction, and rebuilds. The
parent waits for a synced marker at the selected boundary, kills the child, and
waits for exit. Destructors do not clean up the interrupted writer. Hooks exist
only in test builds.

Boundaries include generation creation, object installation and sync, manifest
completion, generation sync, CURRENT temporary-file sync, CURRENT rename and
directory sync, generation retirement, object marking, and partial object deletion.
Before rename, CURRENT must remain unchanged; after rename, it must select a
new complete generation. New publications are checked against the source **before**
recovery can hide a bad update. Recovery rebuilds and verifies exact search sets
and complete orphan reclamation. The matrix performs **580 source-oracle queries**.

A separate test runs two roots (full and lean), each with a writer and two reader
threads. Both readers must observe the original generation and each of eight
subsequent publications. Readers keep opening and searching while writers run;
acknowledgments gate progress and waits are bounded. Original readers remain
pinned across all publications; lazy token/line-map access and search still work.
Dropping those pins and publishing again must reclaim their generations and
objects. This provides 16 publication transitions with explicit observations,
rather than relying on two unsynchronized loops happening to overlap.

These tests cover **process termination, not power loss**. They do not simulate
lost/reordered filesystem writes, broken storage hardware, or every I/O error.

## Sustained CLI histories

The [harness](../../scripts/validate-stable-lifecycle.py) gives full and lean the
same seeded workload: edits, additions, deletions, and renames in balanced blocks.
Each starts with 64 text files and keeps the live corpus bounded. Paths include
spaces and Unicode; contents include CRLF, missing final newlines, punctuation,
case variation, and unique revision markers.

After every update, a fresh scan of all source files checks all matching content
rows and UTF-8 byte offsets for a universal marker and changed revision markers.
Periodic audits add absent, literal, case-insensitive, regex, files-only, and count
queries. The oracle uses Python substring/simple regex matching, not FXI internals.
Explicit compaction runs every 50 updates; automatic compaction remains enabled.
Every successful update checks reachability and live document counts. Final
compaction followed by a real publication must leave exactly the current manifest's
objects. The extra publication lets the compactor's own old-reader lease close.

The release containing both reclamation fixes passed **3,000 history updates, 60 explicit compactions, and
8,326 oracle queries**, comparing 1,740,678 matching rows. Four additional
reclamation updates followed final compaction. Both campaigns used the same
release binary (SHA-256 `af9861a2bc2502e27851fd6cebca1844e7e35169d0c2c7a95fa523b6ccf4ab9c`).
These recorded histories precede the subsequent Windows lease-retry fix.

| Campaign / profile | Updates | Peak logical bytes | Final logical bytes | Peak / final objects | Update median / p95 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| [Unpacked](lifecycle.json.gz), full | 1,000 | 110,255 | 41,611 | 15 / 2 | 34.92 / 60.63 |
| Unpacked, lean | 1,000 | 93,891 | 34,707 | 15 / 2 | 33.67 / 56.73 |
| [Packed](lifecycle-packed.json.gz), full | 500 | 168,012 | 68,340 | 15 / 2 | 40.65 / 67.85 |
| Packed, lean | 500 | 151,786 | 61,476 | 15 / 2 | 39.57 / 65.06 |

Every final object was referenced by CURRENT; no unreachable objects remained.
These observed bounds apply to this bounded corpus and compaction schedule,
not arbitrary histories or indefinitely pinned readers. All 19,881 existing
production registry entries were unchanged, and private campaign workspaces
were removed.

Reports retain binary/harness hashes, seed, raw command times, workload/source
hashes, and per-step storage. JSON is gzip-compressed to keep raw evidence small.
Private fixtures and copied binaries are removed on success or failure. These
are diagnostic subprocess timings, not paired performance benchmarks or tail
latency guarantees. No compilation ran during the recorded histories; the
unrelated local watch daemon was paused and resumed with a `finally` guard.

Reproduce after building an idle release binary:

```sh
python3 scripts/validate-stable-lifecycle.py --binary target/release/fxi \
  --steps 1000 --output /tmp/lifecycle.json
python3 scripts/validate-stable-lifecycle.py --binary target/release/fxi \
  --steps 500 --source-pack --output /tmp/lifecycle-packed.json
FXI_LIFECYCLE_REPORT=/tmp/crash-matrix.json \
  cargo test --lib index::lifecycle_tests
```

CI runs the crash/reader tests and a shorter 20-update history per profile on
Linux, macOS, and Windows, plus the packed history on Linux and macOS, retaining
lifecycle reports as artifacts. Harness
regressions exercise false-result detection, leaks, storage isolation, cleanup,
and seed reproducibility.

Before the Windows lease-retry follow-up, local checks passed: 1,125 all-target Rust test executions (the two
ignored entries are child-process helpers), Clippy with warnings denied, Rust
1.88 all-target checking, rustfmt, release build, and 17 Python harness tests.
Both source-capture reclamation regressions were also observed failing before
the fix and passing afterward.

## Remaining limits

The long histories are local macOS validation on a small synthetic corpus, not
large-corpus scaling evidence. Source packs are Unix-only and the packed campaign
does not enable compression. Concurrency checks use direct readers and serialized
per-root writers, not watcher event storms. Existing migration, integrity,
CLI-differential, and daemon tests remain necessary; this campaign does not replace
them. Default promotion still needs broader platform performance measurements,
watcher/concurrent workload stress, and resolution of the documented small-index
cost. The subsequent [combined-mode experiment](../performance-stable-segments/NOTES.md#combined-checked-search-and-stable-objects)
addresses the routing incompatibility and adds checked lifecycle coverage.
