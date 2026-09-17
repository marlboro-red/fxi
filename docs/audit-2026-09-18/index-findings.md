# Index and freshness correctness audit — 2026-09-18

Audited baseline: `aaf0b14`. Read-only production review of index readers, builders, writers, compaction, generations, and daemon update lifecycle. Reproductions use the existing release binary and isolated temporary sources/indexes/sockets. No throughput benchmarks, compilation, or production changes were made by this audit. This is a risk-focused audit, not a proof that every implementation path is correct.

## Confirmed behavior bugs

### I1 — High: compaction converts detectable corruption into successful, incomplete results

Locations: `src/index/compact.rs:325`, `:361`, `:392`, `:445`, `:523`, `:565`; publication at `:182`.

Reproduction: [script](repro/index-corrupt.py), [output](repro/index-corrupt.txt). Build twelve matching source files in four chunks, remove the required gram/token files from one segment, then search. Before compaction, search correctly fails with `Cannot open segment 1; rebuild the index`. `fxi compact ROOT` then exits successfully, reports twelve documents merged, and publishes a generation returning only nine of the twelve matches. All twelve source files still exist with matching text.

Cause: compaction has independent, permissive parsers. Missing required dictionary/posting pairs are treated as empty; out-of-bounds posting ranges are skipped. Successful generation publication and garbage collection then remove the evidence of the missing data. Optional legacy positions/line maps need different treatment from required gram/token data.

Fix: validate all required input structures and referenced ranges before publishing; reuse validated segment readers or shared format decoders. Propagate malformed data errors rather than accepting partial postings. Preserve CURRENT on failure. Tests should cover missing files, truncated payloads, offsets outside files, invalid UTF-8 token dictionaries, overflowing offsets, and unchanged CURRENT after every failure.

### I2 — High: transient source read failures become permanent negative cache entries

Locations: `src/index/build.rs:1145–1175`, `:1223–1241`, `:1113–1118`.

Reproduction: [script](repro/index-rejected.py), [output](repro/index-rejected.txt). Build twelve files, add a valid text file containing a new marker with permissions `000`, run incremental indexing, restore permissions to `644`, run indexing again. FXI says `Index is up to date, no changes detected`, and searches miss the now-readable file. No artificial timestamp restoration is involved: chmod naturally leaves modification time unchanged.

Cause: `process_file_for_update` returns `None` for both permanent eligibility exclusions and temporary I/O errors. Every failure is saved in `rejected_files`, and subsequent full scans only compare mtime. A native chmod notification can force recovery if an active watcher receives it; explicit indexing and five-minute scans do not guarantee recovery.

Fix: use a typed outcome distinguishing indexed, deliberately excluded, unstable read, and I/O failure. Do not cache transient failures as permanent rejection; retain retry work and report skipped/error counts. Cache stable exclusions using size plus an appropriate source stamp or bounded revalidation. Tests should also cover formerly indexed files made temporarily unreadable, and disappearance/replacement while reading.

### I3 — Medium: graceful shutdown abandons already-received edits

Locations: `src/server/daemon_core.rs:224–227`, `:326–335`, `:1252–1254`, `:1280–1287`.

Reproduction: [script](repro/index-shutdown.py), [output](repro/index-shutdown.txt). Start an isolated watched foreground daemon using supported debounce settings (1 second quiet, 2 second maximum age), let startup settle, add a matching file, wait 100 ms, issue protocol Shutdown, and wait for normal process exit (with the accept wakeup described in I4). Subsequent direct disk search misses the new file.

Cause: watcher shutdown exits without flushing the local debouncer or draining received native events. Processor shutdown flushes only the `pending_changes` map, before draining its message channel. There is no barrier ensuring producers have delivered their final batches before the consumer's last flush. A busy index lock also makes the final flush return without persisting. The existing benchmark verifies persistence only after a change is already visible, so does not exercise this boundary.

Fix: coordinated shutdown: stop accepting new requests, stop notification producers, flush their final batches, drain the processor queue, persist captured pending work with a defined timeout/error result, then acknowledge completion. Add deterministic queue/debouncer and busy-lock tests, plus saves immediately before shutdown. Crash recovery on the next watched startup helps but is a separate contract from successful graceful persistence.

### I4 — Medium: Unix protocol Shutdown does not wake the blocking accept loop

Location: `src/server/daemon_unix.rs:74–77`; shutdown flag set in `src/server/daemon_core.rs:642`; CLI forced fallback `src/main.rs:324–325`.

Reproduction: same [shutdown script](repro/index-shutdown.py). A successful `ShuttingDown` response leaves the process alive, blocked in `listener.incoming()`. A second socket connection wakes accept and permits cleanup/normal exit. The script isolates this from I3 by explicitly waking accept and awaiting normal exit.

The ordinary CLI sleeps 500 ms after the graceful request and then invokes forced stop if still alive. Therefore this design commonly turns a nominal graceful stop into SIGTERM; a slow final persistence could be interrupted even though the user asked for an orderly stop.

Fix: actively wake/close the listener, or use a nonblocking accept/event loop with a shutdown signal. Make final completion observable to the client, and distinguish a timeout/forced shutdown from a completed flush.

## Bounds and malformed-data risks confirmed by code inspection

### I5 — Medium: disk writer ID arithmetic is less safe than the memory path

`src/index/writer.rs:1043–1048` computes maximum existing document ID plus one without checking overflow; `:1097` and `:1107` increment path/document IDs unchecked. Debug builds panic and optimized builds can wrap. Memory delta creation uses checked IDs, but its fallback is the unchecked durable writer. Validate capacity before constructing a generation or adding any files, return actionable errors, and compact/rebuild through an explicitly safe path. Test sparse maximum-ID fixtures without enormous document populations.

### I6 — Medium: malformed sparse IDs/counts can request huge allocations

`src/index/compact.rs:201–202` allocates a dense remapping vector by maximum stored document ID, regardless of document count; one `u32::MAX` record requests roughly 16 GiB. Reader `DocumentLookup` already uses a density-aware strategy; compaction should share it or validate density/capacity.

`src/utils/encoding.rs:271` and `:329` allocate positions from an untrusted varint count before checking remaining payload bytes. A six-byte document/count prefix can ask for roughly 16 GiB. `src/index/compact.rs:496–500` similarly allocates a claimed line-map encoded length before verifying available file bytes. These are reachable from damaged index files. No huge allocation was executed during the audit. Bound counts by remaining bytes, use fallible allocation, and return structured corruption errors.

### I7 — Medium: validation remains inconsistent across index components

`read_documents_version` validates byte bounds but does not reject duplicate/zero IDs, invalid path references, unknown flags, or references to missing segment IDs. `read_paths` uses lossy UTF-8 decoding, permits unsafe absolute/parent paths, and ignores trailing bytes. Compaction silently skips invalid path references (`compact.rs:221`) rather than rejecting damaged metadata. Posting decoders stop at malformed varints or saturate arithmetic, yielding apparently valid partial lists. A damaged index can therefore pass structural opening checks while silently omitting candidates.

Improve with a shared invariant validator and checked decoders, plus optional `fxi check` diagnostics. Keep startup performance in view: validate once per immutable generation and use a verified manifest/checksum strategy rather than repeatedly re-parsing large postings on every query. Differential tests should mutate format boundaries and require either exact results or a clear error, never a successful partial answer.

### I8 — Platform-specific path representation limitation

`writer.rs:915`, `:1319`, `reader.rs:1870`, and `utils/app_data.rs:60–78` use lossy Unicode conversion for stored paths/root hashing. On Unix filesystems permitting non-UTF-8 names, distinct paths can collapse to the same serialized spelling/index identity, and reconstructed source paths can be unreadable. This is code-evident but was **not reproduced here**: the macOS test filesystem rejected creation of a non-UTF-8 filename. Add Linux coverage and choose reversible OS-native path encoding or explicit rejection with useful diagnostics. Protocol JSON path representation must be considered alongside the disk format.

## Performance and design improvement priorities

1. Revision-aware background persistence/compaction. Disk publication currently blocks the single update processor and therefore all roots. Preserve durable/live separation and protect newer snapshots from older completion. Benchmark tail latency during long publication and sustained edits, not only a quiet single save.
2. Explicit, shared corruption/eligibility contracts. Separate parsers have drifted; central validation is both a maintainability improvement and necessary to fix I1/I2/I5–I7.
3. Resource budgets for event queues, rejected metadata, live snapshot documents, decode buffers, and compaction. Current per-preview file/source-byte caps do not cap all RSS; immutable document vectors are copied, and pending/native mpsc queues are unbounded under a slow consumer.
4. Compaction memory and amplification: current all-segment merge materializes all grams, tokens, positions and line maps in memory. Explore streaming sorted merges/tiered compaction with a real memory budget and measure publication/search tail effects.
5. Precise-path indexing still has avoidable full-population work: unknown temporary/new paths may scan all valid paths for descendants, whole rejected-file maps are cloned for each event, and memory snapshots rebuild document lookup/valid IDs. Measure rename/create storms and large rejected populations before choosing a prefix index/persistent metadata representation.
6. Watcher failure health and recoverability. Errors are logged, but request/status should expose unhealthy watching, last successful reconciliation, pending visible-versus-durable work, and retry reason. Detect and retry missed setup/runtime failures without requiring a new query.
7. Recovery testing should include queue/debounce shutdown boundaries, read errors, corrupt compaction inputs, generation switch failures, and concurrently changing files; steady-state search equality alone does not cover these contracts.

## Existing design strengths retained

The separate durable/live readers avoid replaying uncommitted metadata as if persisted; native events retain file paths with conservative full-scan fallbacks; memory deltas build real positional postings rather than bypassing query planning; immutable generations plus leases prevent deleting files still used by readers; publication uses one CURRENT switch after durable writes. These are sound foundations. Improvements should preserve those invariants rather than trade correctness for isolated latency results.
