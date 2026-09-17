# Architecture, resource use, and integration audit

Review of `aaf0b14`, 18 September 2026. This is a source review plus isolated
correctness probes, not a fresh performance benchmark. Reported older timings
below are measurements recorded by the repository, not independently rerun here.

## Confirmed integration defects

1. **Pending VS Code socket ownership race** (`vscode-extension/src/daemon/client.ts:38`).
   `this.socket` is assigned only inside the asynchronous `connect` event, so two
   calls to `connect()` create two sockets. `dispose()` cannot destroy a pending
   socket; its eventual connect event changes a disposed client to connected.
   A disconnect event from an older socket can also act on the newer global
   socket. Two isolated mock-socket tests reproduce the first two consequences.
   Fix by owning the pending socket immediately and making event handlers check
   socket identity/disposal. Tests should cover retry, dispose-before-connect,
   overlapping attempts, and stale close/error events.

   Reproduction (the artifact deliberately asserts broken behavior and is not a
   permanent regression test): copy `extension-connection-race.test.ts.txt` to
   `vscode-extension/src/daemon/audit-connection-race.test.ts`, run
   `cd vscode-extension && npm test -- src/daemon/audit-connection-race.test.ts`,
   then remove the temporary test. Both characterization tests passed.

2. **Unicode highlighting offsets use different units**
   (`vscode-extension/src/webview/getWebviewContent.ts:444`). Rust regex offsets
   and `ContentMatch.match_start/match_end` are UTF-8 byte offsets. JavaScript
   `String.slice` expects UTF-16 code units. For `é needle`, byte range `3..9`
   produces `eedle` with the current slicing. Astral characters introduce another
   mismatch. Convert byte offsets at the rendering boundary and test accented,
   CJK, emoji, combining, and zero-width matches. An isolated Node
   expression reproduced that exact mismatch; a full rendered webview was not exercised.

3. **TUI startup errors skip terminal restoration** (`src/tui/mod.rs:57-76`).
   Raw mode and the alternate screen are enabled before fallible terminal/App
   setup. `App::new(path)?` (e.g. a nonexistent path) exits before the cleanup
   block. Use an RAII terminal guard, including early setup failure and panic
   cleanup. A PTY regression should verify terminal flags after the error.

4. **Initial TUI query never starts with a connected daemon**
   (`src/tui/mod.rs:79`, `src/tui/app.rs:119`, `src/tui/app.rs:210`). Initial query
   is deferred to background index-load completion, but daemon startup sets load
   state Ready and never takes that completion branch. Execute immediately when
   ready; otherwise retain deferred execution. Cover both launch modes.

5. **Editor launch silently fails for common EDITOR values**
   (`src/tui/app.rs:525`). `EDITOR='code --wait'` is treated as a single executable
   name; every editor receives vi's `+line` argument, and errors are ignored.
   Parse executable/arguments without shell interpolation, support documented
   line-location conventions or a safe template, and surface launch errors.

6. **Panel lifecycle cleanup is incomplete** (`vscode-extension/src/extension.ts:23`).
   The webview registration is subscribed, but the `SearchPanelProvider` instance
   is not added to subscriptions. Its explicit dispose routine removes client
   and configuration listeners; registration disposal does not establish that
   those listeners are cleaned up. Register provider disposal explicitly and
   test activation/deactivation cycles.

## Resource/correctness boundaries requiring regression coverage

- **Daemon framing and idle connections** (`src/server/daemon_unix.rs:156`,
  `src/server/protocol.rs:282`): every non-EOF read error is answered and then
  parsing resumes. Timeout after a partial length/body loses frame state;
  rejecting an oversized frame leaves its body unread and misinterprets it as
  new lengths. Idle read timeouts also retain connection slots. Fatal transport
  or framing errors should close the connection; recover only errors known to
  follow an entirely consumed frame. See `probe-unix-idle.py` and its JSON for
  the isolated 64-idle-connection probe. **Reproduced:** after 32 seconds,
  an idle client received `Resource temporarily unavailable (os error 35)` as a
  JSON error, but a 65th client still got BrokenPipe and the daemon logged
  `too many connections, rejecting`. All probe processes/sockets were cleaned up.
- **Unchecked durable IDs** (`src/index/writer.rs:1044,1096,1104`): max DocId+1,
  next_doc_id++, and next_path_id++ can wrap in release builds. Memory previews
  reject overflow but then fall back to this durable path. Use checked capacity
  at allocation and return an actionable compact/rebuild error. Construct tiny
  sparse/high-ID fixtures instead of allocating billions of documents.
- **Sparse compaction allocation** (`src/index/compact.rs:202`): an index with a
  handful of valid documents but a high DocId allocates max_id+1 remap entries
  (~16 GiB for u32::MAX). A dense fast path needs a sparse fallback or a validated
  density/capacity bound. Reader legacy sparse support makes the assumption
  especially worth testing. No huge allocation was deliberately attempted.
- **Result-size cap is not a wire-byte cap** (`src/server/daemon_core.rs:41`,
  `src/server/protocol.rs:250`): up to ten million records are built and serialized
  while readers reject frames over 100 MiB. Large lines/context can reach the
  limit far below ten million records. Add byte-budgeted pagination/streaming,
  explicit truncation metadata, and matching client/server bounds.

## Ranked improvement opportunities

| Priority | Opportunity and code | Why it matters / validation experiment |
|---|---|---|
| P1 | Independent revision-safe publisher; `daemon_core.rs:221,319,349` | One watcher processor serializes preview extraction, publication, rebuilds and compaction across roots. Measure mixed roots, injected slow writes, and bursts exceeding 10 seconds. Use per-path revisions and publication watermarks; test overlapping saves, failures, external CURRENT replacement and shutdown. |
| P1 | Delta metadata structural sharing; `reader.rs:1047,1088,1136` | Each preview scans maximum DocId, clones all documents, scans tombstones/valid docs and resets valid/path-order caches. Share a base table plus bounded replacements with sound iterator/ID semantics. Scale one-file saves from 65k to 1m documents and measure both update and next-query cost. |
| P1 | Reuse extracted pending changes; `build.rs:1212-1263` | Every preview reparses the full union of pending paths, clones extracted postings for the memory delta, then persistence rereads/reextracts. A revisioned per-path extraction cache can avoid repeated work during multi-file bursts. Must ensure changed source bytes invalidate cached extraction and preserve eligibility/ignore changes. |
| P1 | Bounded global request admission; `daemon_unix.rs:24,142,188` | 64 connections ×32 handler threads permits roughly 2,048 simultaneous handlers plus connection/writer threads; response channel is unbounded. Global Rayon contention, response allocation and slow-client queues can dominate. Test mixed fast/slow queries at 1/8/32/128 clients, throughput and p95/p99, bounded memory, cancellation. |
| P1 | Phrase verification precision | Round six's comparable paths-only API median remains 5.894ms FXI vs1.611ms Zoekt. Safe positional/interior-token or byte-anchor filtering must retain substring and Unicode semantics. Prior interior-token experiment improved two phrases but regressed other queries and was rejected. Use a broader phrase mix, cold/warm dictionaries, per-query costs and holdout corpora before enabling. |
| P1 | One-shot startup/absence | Round six reports csearch4.227ms vs FXI11.736ms absent; optional certified mapped routing reduces FXI11.710→5.818ms in a separate cohort. Persist compact routing summaries tied to immutable segment identity; avoid mandatory full-dictionary validation on eligible negative queries without weakening corruption detection. Benchmark new common controls, not ratios of these separate cohorts. |
| P2 | Stable immutable segment store; `generation.rs:52` | Every commit traverses/hardlinks all inherited segment files; round seven profiled85ms here. Reference-counted/leased stable segments and small generation manifests can remove metadata churn, but GC/crash durability need redesign. Merely skipping inherited fsync was tested, showed no win and was reverted. |
| P2 | Streaming tiered compaction; `compact.rs:93,260` | Current full merge materializes every gram/token/position map, sorts vectors, remaps all docs and recaptures source packs. K-way streaming merges and selective tiers can bound memory/write amplification. Track cumulative bytes written, peak RSS and query amplification over hours of edits; preserve stop-gram and position compatibility. |
| P2 | Batch durable tombstones; `writer.rs:1081` | Each changed path performs a reverse document scan, O(changes×documents), after reopening and cloning all paths. Build/reuse a numeric latest-live-document lookup, batch mask updates, and measure branch-switch scale, not just one file. |
| P2 | Ranked/content/count paths | Files-only received most source-pack/scheduler work. `executor.rs:258,346,1336` still materializes full matches/line strings and clones candidate paths; scored queries must consider all candidates to preserve exact ranking. Stream scoring into bounded top-k storage, use justified score upper bounds only, and extend source-pack verification to content/count with parity tests. |
| P2 | Adaptive filtering and postings | `planner.rs:136` always applies metadata filters after content narrowing; selective ext/path filters and Boolean intersections could benefit from cheap column bitmaps/cardinality-based ordering. Measure dictionary probes, decoded postings, false-positive candidate bytes and filtering cost, including Unicode/short regexes; ensure the cost model itself does not tax selective queries. |
| P2 | Compact index/build footprint | Round six packed index1886.6MiB vs csearch73.0MiB, build RSS354.2 vs291.9MiB; packs add~1.23GiB. Compression/block directories, optional/lazy positions and binary metadata deserve experiments. Compare default/packed configurations distinctly; report build throughput, peak memory, bytes per source byte, cold faults and warm latency together. |
| P2 | Stream build stages; `build.rs:233,350,413` | Full path discovery, metadata weighting, extraction vectors and queued segment writes retain multiple representations. Byte-budgeted producer/consumer stages can bound worst-case expansion, especially huge generated files. Validate skewed size/token distributions and file descriptor/queue limits. |
| P2 | Reader/root cache lifecycle; `daemon_core.rs:51,1094`, `reader.rs:633` | Loaded roots/watchers remain indefinitely; last_used is updated but not used for eviction. Content has a shared1GiB text budget, but reader metadata/mappings/token maps/watched-path dictionaries and derived position structures need accounting. Add root eviction with pending-flush/lease safety, a documented total budget and long-session tests. |
| P2 | Honest diagnostics; `daemon_core.rs:944` | Status memory is doc_count×100+1MiB/root, missing actual mapped/cache/position/snapshot usage. Cache misses increment while no cache-hit increment exists, so displayed hit rate cannot describe real caching. Expose separately measured source-cache hits/bytes, mapped bytes, pending visible/durable revisions and update phases. |
| P2 | TUI bounded responsive work; `app.rs:375,421,549,678` | Repeated searches create uncancelled threads; daemon searches serialize behind one client mutex. Reindex and previews block UI; the1MiB preview cap is applied after reading the entire file. Use latest-request coalescing/cancellation, bounded preview reads and asynchronous rebuild progress. |

## Evidence needed before a universal-best claim

Freshness round seven is a small-sample, warm-storage, one-machine macOS result.
Its very short synthetic saves and three-second bursts do not cover compaction
or the ten-second publication deadline. Broaden to real edit payloads, large
renames/deletes/branch switches, changed ignore rules, notification loss,
concurrent writers, slow/network filesystems, cold page cache, Linux/Windows,
long-running memory/disk growth and multi-root/multi-client contention.

Query comparisons need all three output modes (files, lines/context, counts),
ranked top-k, Unicode/case-folding, regex/short literals, filename/path filters,
negative/Boolean/proximity queries, limit handling and exact coverage checks.
Separate one-shot startup from warm API execution and client serialization.
Report p50/p95/p99, uncertainty, throughput, CPU, peak RSS and physical index
size; randomize/interleave samples, pin versions/build flags, and retain raw
results. No new speedup is claimed by this audit.
