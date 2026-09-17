# Round seven: save-to-search visibility

This round measures native watcher delivery through complete files-only CLI
results on macOS/M2 Max with warm storage. It is not a cold-storage, Windows,
concurrent-client or p99 claim. Public tgrep remains pinned to
`b1d0fc2f6245cc78f1943e5864ceeab812452404`; performance binaries use Rust 1.94.
The large fixture copies the same 65,287-file controlled Linux source corpus
used in the earlier watcher comparison, then adds 256 synthetic probe files.
This is distinct from round six's 65,284-file common three-indexer fixture.

## Results

Measured on one macOS/M2 Max machine. Times below are medians in milliseconds
from the last write completing to an exact CLI result. Sample counts are in
parentheses; these small samples do not establish tail-latency guarantees.
The main cohort uses the `8bc43d7` binary. The final compatibility fix preserves
its ordinary contiguous-document path; final-binary confirmation is recorded
separately below.

| Workload | Previous FXI | New FXI | tgrep | FXI speedup over tgrep |
|---|---:|---:|---:|---:|
| 256-file fixture | 151.8 (5) | 21.0 (7) | 53.5 (7) | 2.54× |
| 65k-file source tree | 402.3 (1) | 23.4 (3) | 49.5 (3) | 2.11× |
| Atomic replacement, 256 files | — | 22.6 (5) | 48.1 (5) | 2.13× |
| 41 KB source payload, 256 files | — | 22.0 (3) | 54.3 (3) | 2.46× |
| Final save after continuous writes, 65k files | — | 25.9 (2) | 31.1 (2) | 1.20× |

During continuous writes, the new file appeared after **24.1 ms**
with FXI versus **59.9 ms** with tgrep (two samples each).
FXI made zero durable delta publications before visibility in these final live
runs. Every FXI run passed the additional graceful-shutdown persistence check.
The final-save burst advantage is smaller than the ordinary-save advantage;
this workload should not be advertised as an overwhelming win.

The repeated latest large-tree tgrep controls measure about 50 ms, faster than
the earlier 60–66 ms controls. The table uses the latest three samples rather
than selecting the slower controls. The one fresh original-FXI large sample
agrees with the earlier 407–420 ms screening range, but is still only one sample.

On the large watched root, median RSS after visibility was
**289.0 MiB for FXI** versus
**484.4 MiB for tgrep**.
FXI's pre-edit RSS was about 288.9 MiB,
versus 277.3 MiB for the original FXI control.
Thus watched lookup preparation has a memory cost before edits, while the new
snapshot path avoids the original update's larger resident-set increase.
These are sampled RSS values, not peak-allocation measurements.

Warm query control, 21 interleaved CLI samples per query, with exact file-set
checks against ripgrep:

| Query | Previous FXI (ms) | New FXI (ms) |
|---|---:|---:|
| `folio_wait_bit_common` | 4.49 | 4.47 |
| `auditNonexistentSymbol94283` | 3.93 | 3.89 |
| `struct file_operations` | 10.50 | 10.52 |
| `return` | 53.89 | 53.08 |

There is no material warm-query regression in this control.

### Final binary confirmation

Commit `d5ad53f` adds a sparse/reordered legacy document lookup correction and
persistence-deadline regression tests. The shipped binary confirms **23.2 ms**
on the small fixture (three samples) and **21.3 ms** on the large tree (one
sample): see `small-shipped-fxi.json` and `linux-shipped-fxi.json`.
The final small-fixture burst check measures **17.3 ms versus 40.8 ms** after
the last save, and **23.8 ms versus 50.9 ms** for first new-file visibility
(three samples per tool). Burst completion is sensitive to notification timing:
earlier small tgrep burst samples were 10–22 ms after the last save. These short
runs support responsive updates during typing, not a stable universal ranking
for the exact final edit of every burst.

Validation: **774 Rust test executions** across all targets, strict Clippy,
formatting, Rust 1.88 MSRV, and three Python benchmark-harness tests pass.
Coverage includes fresh-index differential query results, positions/phrases,
Boolean combinations, counts/content/ranking and limits, generation leases,
legacy aliases/reordered IDs, ignore cancellation, publication/rebuild failure,
pending-batch preservation and persistence deadlines. The count includes the
library suite executed in both binaries; it is not a count of distinct tests.

## Measurement corrections

The previous harness slept 100 ms between CLI probes. Five-millisecond polling
revealed that much of the older small-fixture 245 ms measurement was sampling
delay: the original binary actually measures around 150 ms here. CLI startup,
query execution, OS notification delivery and polling quantization remain part
of the measured time. This is an application visibility measurement, not an
isolated indexing kernel benchmark.

The final harness records time after the last write completes, first visibility
of a new file during unrelated continuous writes, binary/harness SHA-256, exact
create/edit/delete results, duplicate-path checks, daemon liveness, and FXI
fallback rejection. The final lookup runs also sample server RSS immediately
before writes and after visibility (outside latency timing). FXI also must make a newly visible marker survive graceful
shutdown and a fresh direct disk reader. Tests/compilation do not run during
measurements. The intermediate large-tool runs alternate tool order between repetitions;
the last lookup optimization uses three FXI samples followed by three fresh
tgrep controls.

Initial `small-baseline`, `small-zero-debounce`, `small-tgrep`, `linux-baseline`
and `linux-tgrep` files are screening records collected while the harness was
being refined; main comparisons use the `*-lookup-*` records against the final
tgrep and original-FXI controls. The earlier `*-final-fxi*` records measure
the intermediate snapshot implementation before the last path-lookup optimization. Early burst
screening counted attempted incremental updates, including memory previews;
the final harness counts actual `Wrote delta segment` publications instead.

## Findings and implementation

Every native notification previously became an empty rescan sentinel. FXI
waited for 100 ms of quiet and then rescanned the whole repository. Precise
file hints now use a root-based, ancestor-pruned walker: it retains the normal
nested ignore semantics while avoiding unrelated branches. Directory/ancestor
changes, ignore controls, ambiguous notifications, overflow and an external
CURRENT change require full reconciliation. Explicit hints force rereading
preserved-size/preserved-mtime edits. Small scopes use a serial walker and avoid
cloning/hashing all indexed paths into an update map. Watched readers now prime
a shared lazy path lookup at startup and durable reader swaps; ordinary
unwatched queries do not allocate it. Exact hinted documents are resolved by
path ID; descendant checks remain conservative for replacement/ancestor cases.

That alone was insufficient. A large-root screening with 10 ms debounce still
took 230 ms. Phase timing showed approximately 138 ms in publication. A finer
trace found 85 ms inheriting segment links, 6 ms loading metadata, 14 ms building
writer path lookup, 6 ms encoding metadata and 32 ms publishing. Opening the
new reader added another 14 ms. Skipping repeated syncs of unchanged inherited
files did not demonstrate a useful latency gain and was reverted; original
durability barriers remain intact.

Small saves now produce a fully indexed immutable memory delta before disk
publication. It contains actual gram/token postings, positions, line maps and
tombstones; query planning and verification are unchanged. Base segments and
durable paths are shared, while document metadata and appended paths belong to
the new snapshot. Readers retain the original generation lease for lazy files.
A separate durable reader prevents an uncommitted preview from becoming the
basis of a disk update or failed-publication retry.

The implementation is commit `8bc43d7`, built with Rust 1.94.0.

Every new preview combines the durable base with the entire pending path set.
It does not append another memory-only segment to the last preview. This keeps
repeated saves bounded and preserves edits across batching. Cancellation restores
the durable view, including an ignore rule hiding a newly previewed file.
Eligibility is bounded to 256 changed files and 8 MiB of accepted source bytes;
this is not a bound on total RSS. Larger batches/rebuilds use the durable path.

The selected default quiet debounce is 1 ms, with a 100 ms maximum event age.
Persistence is scheduled after 250 ms quiet or ten seconds of continuous edits;
explicit nonzero FXI_DELTA_FLUSH_SECS retains its first-event scheduling role.
Graceful shutdown attempts to persist pending work. Interrupted work is repaired
by watched startup reconciliation. Direct disk readers can lag the daemon.

This uses established near-real-time indexing principles, not a claim of a new
indexing theory. Lucene separates near-real-time readers from durable commits
([IndexWriter documentation](https://lucene.apache.org/core/10_3_1/core/org/apache/lucene/index/IndexWriter.html));
[pinned tgrep](https://github.com/microsoft/tgrep/blob/b1d0fc2f6245cc78f1943e5864ceeab812452404/tgrep-cli/src/serve.rs)
also maintains a live overlay. FXI's implementation reuses its existing query
engine and immutable segment semantics to avoid a second approximate evaluator.

## Remaining limits

Durable publication and compaction still use the update processor. Saves that
arrive during that work can wait behind it; this is not a universal latency
bound. Large branch switches, ignored-directory changes, writer contention,
notification loss and recovery scans are different workloads from a small save.
Native notification delivery imposes latency outside the indexer itself.
A future independently scheduled publisher needs per-path revisions and atomic
rebase/retirement so a save arriving during a commit cannot disappear.

## Next experiments

The highest-value next change is an independently scheduled publisher, not
another debounce reduction. Assign each changed path a monotonically increasing
revision, capture a publication watermark, and retire only revisions actually
included in the committed generation. A new edit arriving during publication
must remain in the query snapshot. Test failed commits, overlapping saves,
compaction and external CURRENT replacement before enabling that design. Measure
latency throughout bursts longer than ten seconds and injected slow publication,
including worst samples rather than just medians.

Snapshot construction still copies document metadata and scans numeric document
IDs. A shared base document table plus a bounded replacement map could make
small-save work closer to the size of the change. That requires preserving every
existing iterator, valid-ID mask, sparse-ID lookup and generation lease; it is
not safe to bypass planner intersections by appending dirty candidates.

For stronger read-after-save guarantees, an editor/client revision barrier could
explicitly request visibility of a known save. Native OS notifications alone
cannot promise an arbitrarily low end-to-end bound. Bulk branch changes,
concurrent writers, Linux/Windows latency and long-run memory remain separate
benchmark requirements before making any universal-best claim.

## Reproducing the comparison

The freshness harness requires Python 3.11+ (standard-library TOML parsing).
It rejects nonempty user watcher configuration and clears inherited watcher
tuning variables. The measured environment had no watcher-file overrides and
no inherited merge/rebuild tuning; the final guard smoke run records this.

Build FXI with `cargo +1.94.0 build --release`; use the pinned tgrep checkout
above. Run each command sequentially, without compilation or other benchmarks:

```sh
python3 scripts/benchmark-freshness.py --binary target/release/fxi --tool fxi --repetitions 7 --output /tmp/fxi-small.json
python3 scripts/benchmark-freshness.py --binary /path/to/tgrep --tool tgrep --repetitions 7 --output /tmp/tgrep-small.json
```

For a large-tree run, add `--source /path/to/controlled-corpus`. Add
`--burst-edits 120` for approximately three seconds of continuous saves,
`--atomic-save` for editor-style replacement, or
`--edit-source /path/to/source-file.c` to save that real UTF-8 payload followed
by the unique probe marker. The default probe files are short synthetic text;
large corpus size alone does not imply a large edited-file payload.

The JSON includes raw samples and server logs. `after_last_edit_seconds` is the
headline save-to-search measure; `visibility_seconds` includes the entire burst.
`new_file_first_visible_seconds` tests whether unrelated continuous saves delay
new-file discovery. RSS is a point-in-time resident-set sample, not peak memory.
Fixture paths are temporary and may be cleaned after results/logs are retained.
