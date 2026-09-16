# Watcher correctness and update latency

This round follows `3414af1`. The objective remains a broad, substantial
advantage, not a claim based on selected query medians.

## Remove the extra publication delay

`acc428d` changes the default additional delta-flush delay from 60 seconds to
zero. Saves are still debounced. Without a live overlay, delaying publication
also delays visibility of new files and new matching substrings. Users can
still configure a longer interval to trade freshness for fewer writes.

`scripts/benchmark-freshness.py` builds a 256-file synthetic fixture, starts a
real native watcher and daemon, edits an existing file and creates a new one,
and polls the CLI until both exact matches appear. It also checks deletion and
replacement remove stale results. This measures notification, debounce and
publication together. Poll resolution is 100 ms; this is not a sub-millisecond
latency benchmark or representative large-corpus indexing measurement.

- Previous binary (`d6298ca`): **59.747 seconds**, one sample. The configured
  extra delay dominates, so this is a reproduction rather than a precise ratio.
- New binary (`acc428d`): **0.620–0.634 seconds**, five independent roots.
- Pinned tgrep `b1d0fc2`: **0.139–0.146 seconds**, five independent roots.

FXI's minute-long defect is removed, but tgrep remains faster in this fixture.
The tradeoff is more frequent index publication. A live overlay and faster
reconciliation are separate work. Raw samples and server logs are embedded in
`freshness-before.json`, `freshness-after.json`, and `freshness-tgrep.json`.

## Do not lose or block ordinary watcher batches

`a5f5834` attempts the writer lock without blocking the shared processor. A busy
root keeps its pending batch while other roots can update. Genuine lock errors
and failed publication retain work with a one-second retry backoff. Mutation
helpers receive the held lock, including rebuild recovery, and return errors.

Regression tests cover contention versus lock errors, progress on another root,
retaining a failed update while its root is temporarily unavailable, retry
without another notification, and the existing recovery deadlock test. Full
Rust tests, strict Clippy, release builds and Rust 1.88 checks pass for both
commits. The reserved explicit rebuild-message path remains separate; these
changes concern the ordinary batches emitted by the native watcher.

## Reconcile real changes and reuse immutable readers

`e1fb733` fixes a test-discovered rebuild loop: a startup/reconciliation sentinel
was counted as one changed file before any scan. On a one-file root that looked
like a 100% change; rebuilding restarted the watcher, which emitted another
sentinel. Rebuild thresholds now use the actual filesystem diff, including the
configured watcher threshold.

Reconciliation reuses a loaded reader only when its canonical root and immutable
generation still match CURRENT. An unchanged generation does not cause another
reader load/swap, while externally published generations reload and due
compaction still runs. Tests cover all three cases, alongside actual updates,
ignore-rule changes and failure recovery. Full Rust tests, strict Clippy and
Rust 1.88 checks pass.

Seven interleaved first-query measurements on the existing Linux fixture,
including initial reconciliation and watcher registration:
**177.919 → 146.376 ms median** (`linux-watched-startup.json`). Fresh daemon
processes are used for every sample; process startup is outside the timed region.
Every matching-file set equals ripgrep. The first candidate sample was slower
(194.290 ms) and remains in the raw data. This is a watched-root initialization
measurement, not steady-state query latency.


## Watcher continuity and scheduling

`4d19bae` keeps native watchers registered through atomic generation rebuilds.
Duplicate registration is checked under the watcher-handle lock, preventing
competing loads from replacing a running watcher. A lifecycle regression test
verifies that rebuild and duplicate registration preserve the existing worker,
and that normal shutdown still stops it.

The scheduling change lowers the default quiet debounce from 500 to 100 ms,
uses the actual debounce/max-age deadline for wakeups, and drains up to 1,024
queued watcher messages before reconciling their combined work. The two-second
maximum event age remains. Cancelling the last pending create resets its batch
clock, so the next unrelated event does not inherit an expired deadline.
Tests cover maximum-age wakeups, cancelled batches and lossless bounded backlog
draining; full tests, strict Clippy and Rust 1.88 checks pass.

Five fresh single-save fixtures: **0.245–0.248 seconds** until both new and edited
files are visible (`freshness-scheduled.json`), versus 0.620–0.634 before scheduling.
Three bursts of 120 rewrites, 25 ms apart: only **two incremental publications**
per FXI run; final-save visibility 0.248–0.250 s. tgrep final-save visibility
ranged 0.017–0.139 s in the same burst setup. The burst duration includes actual
write/sleep overhead and is recorded separately; it is not a search latency.

A larger screening fixture copies the controlled Linux corpus, adds the same
256 probe files, then builds each tool's own index. One run each measured FXI
**0.456 s** and tgrep **0.143 s** (`freshness-linux-*.json`). This establishes
that large-root reconciliation remains a gap, not a precise speed ratio.
All probes check exact new/edited/removal results through real daemons. Raw
records include binary and harness hashes. These are warm-storage measurements.

## Tombstones and retained-match validation

`b170d71` fixes a query correctness bug: gram/token candidate plans could retain
old document IDs after incremental updates. Verifying old IDs against the current
path duplicated matches when an edit retained the searched text. Deleted paths
recreated with ignored content could also be resurrected through old postings.
Every candidate plan now intersects the immutable live-document bitmap before
verification. Regression coverage exercises three successive updates, five query
forms, files/counts/content/ranked output and ignored-path recreation.

The native-watcher probe now preserves an existing match while adding a second
marker. It waits until the marker is searchable, then verifies that the retained
match has no duplicate paths. Three fresh fixtures pass
(`freshness-live-docs-validation.json`). Full tests, strict Clippy, release and
Rust 1.88 checks pass.

## Bulk metadata experiment: not adopted

`examples/metadata_lab.rs` compares parallel per-file metadata with macOS
`getattrlistbulk` directory batches. It verifies every regular-file stamp against
standard metadata outside timing, including device/inode, length and timestamp
fields. Native tests span multiple buffers, Unicode names, symlinks and short
directory records; malformed records are checked for panics. The API and layout
come from Apple's [manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/getattrlistbulk.2)
and [attribute definitions](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/attr.h).

Eleven interleaved Linux-fixture samples: **41.616 ms standard versus 40.460 ms
bulk** (`linux-metadata-batching.json`), with 20.002 ms of reusable grouping work
measured separately. This is effectively neutral and offers no demonstrated
whole-query gain. The prototype remains an offline experiment; production source
reads and reconciliation do not use it.
