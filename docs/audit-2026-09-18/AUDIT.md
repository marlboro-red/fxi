# FXI correctness, CLI UX, and improvement audit

Audited commit **`aaf0b14`**, 18 September 2026 (Australia/Sydney).

**Conclusion: correctness and command consistency need priority over more speed
optimization.** The latest freshness gains are real for the measured workload,
but green tests and fast files-only benchmarks do not establish that the whole
product is correct. This pass found reproducible daemon crashes, false/missing
matches, index-maintenance failures and everyday CLI surprises.

This is a comprehensive pass over the components listed below, **not a proof
that every defect or optimization has been found**. Production code was left
unchanged to keep the audit evidence attributable to one commit. Scripts use
isolated temporary roots/indexes/socket paths; the daemon crash probe targets
only its disposable process. No new performance ranking is claimed.

## Read this first: fixes in priority order

| Order | Finding and evidence | Required repair / acceptance condition |
|---|---|---|
| 1 | **Ordinary query input can kill the daemon.** A 20,003-byte nested query aborts the release process; an overflowing boost yields a JSON `null` score that typed clients cannot decode. [Q1–Q2](query-findings.md) | Fallible bounded parsing, finite numeric validation and traversal budgets. Invalid requests return errors; subsequent Ping/search and watching remain functional. |
| 2 | **Compaction can publish silently incomplete results.** Damaging one segment first causes a proper read error; compact then succeeds and changes twelve real matches into nine. [I1](index-findings.md) | Share strict format decoders; abort on missing required data; leave CURRENT unchanged. Never turn known corruption into a successful partial index. |
| 3 | **Temporary read failures become persistent missing results.** Restore file permissions, reindex, and the readable file remains excluded. [I2](index-findings.md) | Separate eligibility exclusions from transient I/O failures; retain/retry failed work and report incomplete scans. |
| 4 | **Boolean truth is confused with output rows.** NOT fabricates line 1; AND/OR duplicate lines, counts and ranking contributions. [Q3–Q4](query-findings.md) | Separate file predicates, unique matching lines and highlight spans. Check Boolean identities and parity across files/content/count/ranked modes. |
| 5 | **Query syntax can silently change meaning.** `foo-bar` becomes `foo AND NOT bar`; filters overwrite globally inside OR; regex delimiters, malformed groups, proximity and date/size boundaries are wrong or ambiguous. [Q5–Q10](query-findings.md) | Define a precise grammar and strict errors; use scoped predicates or reject unsupported scope. Preserve literal punctuation and valid escaping. |
| 6 | **Mutation commands and daemon state diverge.** Successful force rebuild leaves cached search stale; remove leaves the daemon serving a deleted index. Shutdown drops debounced/queued work and Unix accept remains blocked. [U3](cli-ux-findings.md), [I3–I4](index-findings.md) | Coordinate publication/reload/unload; stop producers, drain queues, flush and signal completed shutdown in a tested order. |
| 7 | **Connection timeouts fail to reclaim capacity.** After 64 idle clients exceed the timeout, client 65 is still rejected. Invalid/partial frames also lack a safe recovery policy. [transport evidence](architecture-and-integration.md) | Close unrecoverable connections, preserve framing boundaries and bound global work/response queues. |
| 8 | **The CLI's visible contract is inconsistent.** `-p subdir` searches outside it; adding `-e` can remove matches; context output repeats/reorders lines; TUI startup errors leave raw mode enabled. [CLI report](cli-ux-findings.md) | Separate root from scope, compose query ASTs consistently, merge output context, and use terminal cleanup guards. |
| 9 | **Corrupt/sparse metadata can trigger unsafe resource use.** Unchecked durable ID increments; dense remaps and claimed position counts can request multi-gigabyte allocations. Code-inspected; huge allocations deliberately not executed. [I5–I7](index-findings.md) | Checked arithmetic, remaining-payload bounds, fallible allocation and density-aware lookups. Require exact results or explicit corruption errors. |
| 10 | **Interactive clients have lifecycle/offset defects.** Pending connections can duplicate/resurrect after disposal; UTF-8 offsets are sliced as UTF-16; initial-query/editor/error paths are inconsistent. [integration report](architecture-and-integration.md) | Explicit connection ownership and disposal; unit-consistent spans; latest-query cancellation; actual terminal/webview interaction tests. |

The first three issues need prioritized, regression-backed fixes.
The Boolean/parser/CLI work needs a coherent contract rather than independent
patches that make one mode disagree with another. Proposed repair order is not a
claim that lower rows are harmless or that each row fits in one commit.

## What was actually checked

- **Current Rust suite:** `cargo test --all-targets` passed **774 test executions**.
  The library suite executes in both library and CLI targets; this is not 774
  distinct cases. These pre-existing tests did not detect the findings above.
- **Extension:** all **91 tests** passed; `npx tsc --noEmit` passed. Two additional
  mock-socket characterization tests reproduced the pending-connection defects;
  they are stored outside the permanent test suite because they intentionally
  assert current broken behavior.
- **Actual CLI:** **54** observations covering help/version, first search,
  explicit paths, flags, limits, filters, counts/context, piped stdout, filenames,
  errors, daemon startup/reload/force rebuild/remove/stop and index statistics.
- **Query behavior:** **54** further CLI cases, **5** isolated wire probes,
  **5** standalone parser probes, and **82 independent ripgrep comparisons** on
  **80 deterministic randomized files**. All 82 regex/Unicode candidate-recall
  comparisons passed; Boolean/output/parser failures were tested separately.
- **Storage/lifecycle:** reproducible corrupt compaction, restored permissions,
  and save-before-shutdown cases; no deliberate multi-gigabyte allocations.
- **Terminal:** a PTY verified that failed TUI initialization left echo and
  canonical input disabled and never left the alternate screen.
- **Transport:** an isolated 64-client test verified that 30-second idle timeouts
  leave slots occupied after 32 seconds.
- **Source review:** parser, planner, executors/scoring, mutable-source verification,
  readers/encoders/writers/builders, generation publication/leases, compaction,
  watcher/debounce/pending state, Unix/Windows transports, CLI/output, TUI,
  extension, configuration, diagnostics, packaging metadata and benchmark scope.

[Validation summary](validation.json), [CLI raw observations](cli-observations.json),
[query raw probes](query-probes.json), [parser probes](parser-probes.json),
[PTY evidence](terminal-observations.json), [idle connection evidence](unix-idle-result.json).
Each detailed report links its reproduction scripts and exact source locations.

## Improvement inventory by subsystem

This table includes design/resource opportunities as well as defects. Expected
speed gains are hypotheses unless explicitly tied to earlier recorded evidence.

| Area | Improvements | How to establish that it is better |
|---|---|---|
| Query language | Fallible parsing; depth/node/byte limits; valid finite numbers; strict complete consumption; escaped delimiters; interior punctuation; scoped filters; validated dates and bounds; explicit literal/regex/query modes | Parser round trips, malformed input subprocess tests, exact expected ASTs, literal preservation and independently evaluated Boolean/property tests |
| Match/result model | Separate file truth from rows/spans; NOT without invented lines; unique line counts; multiple highlight ranges; order-independent proximity; explicit ranked/CLI/wire limit precedence | Idempotence/permutation tests; content-line/count/file-set parity; later high-scoring results; zero/unlimited and explicit top/limit combinations |
| Query execution | Prepare matchers once; avoid repeated per-leaf scans/string cloning; stream counts/content; bounded exact top-k storage; share source snapshot through context rendering | Broad Boolean/Unicode/phrase/context benchmarks, source mutation tests, allocation profiles and exact result equivalence |
| Candidate planning | Cost-aware path/ext/language bitmap filtering; safe phrase/position anchors; conservative short/Unicode regex fallback; cheaper certified negative routing | Independent regex oracle plus holdout corpora; report candidate bytes, decoded postings, false-positive rate and planner overhead |
| Full builds | Byte/posting budgets across discovery/extraction/write stages; reduce duplicate representations; skew-aware chunks; real error reporting and retry classification | Heterogeneous generated/large-file corpora, peak RSS/FD count, throughput, incomplete-scan correctness and queue bounds |
| Live updates | Revision-safe background publisher; per-root scheduling; extraction reuse by source revision; shared base document metadata; faster path-prefix/descendant checks; bounded event queues | Long bursts crossing persistence deadlines, multiple roots, checkout/rename/ignore storms, slow commits and p95/p99 with exact visibility checks |
| Durable writer | Batch tombstones/latest-live-ID lookup; checked ID capacity; fewer repeated path/metadata rebuilds | Vary changed-file count independently of corpus size; sparse/max-ID tests; write amplification and allocation measurements |
| Generation storage | Stable immutable segments referenced by small manifests; shared validated formats; integrity/check diagnostics; safe leased garbage collection | Crash/fault injection at each publication step, active-reader tests, missing/truncated components and fsync/metadata operation profiles |
| Compaction | Strict input validation first; streaming k-way/tiered merges; density-aware ID remaps; byte budgets; avoid full corpus source recapture where possible | Repeated updates/merges, damaged input without publication, exact before/after results, peak memory and cumulative bytes written |
| Freshness/recovery | Distinguish visible/durable revisions; shutdown producer/consumer barrier; retry transient reads; watcher health and recovery; coherent external generation handling | Saves at every shutdown boundary, contended locks, daemon restarts, interrupted writes, lost notifications and restored permissions |
| Server resources | Global admission, bounded response channels, cancellation/backpressure, frame-byte/record-limit agreement, idle/invalid connection cleanup, root eviction | Mixed cheap/expensive requests and slow clients; 1/8/32/128-client load; bounded threads/RSS; subsequent clients recover after errors |
| CLI | Accurate help/version; consistent modes and -e/-w; real file/subtree scope; operational failure codes; stable piped output; JSON/NUL; quiet broken pipe; merged context | Executable help examples and end-to-end journeys in TTY/non-TTY, exact stdout/stderr/status, reserved words and odd filenames |
| Operations/diagnostics | Readiness/completion handshake; visible versus durable pending work; real watched mode; live/tombstone counts; honest cache/RSS metrics; `check`/`explain` facilities | Reproduce actual failure states and ensure diagnostics identify root, incomplete work, reason and recovery action |
| TUI | RAII terminal cleanup; async index/preview tasks; latest-query coalescing/cancellation; bounded reads before preview allocation; surfaced editor errors; safe editor argument handling | PTY errors, tiny terminals, rapid edits/searches, slow I/O, failed editor launch and clean normal/error exits |
| Extension | Pending socket ownership, stale event fencing, disposal registration, UTF-8→UTF-16 conversion, explicit effective limits, real webview interaction coverage | Dispose/reconnect races, Unicode spans, delayed responses, activation cycles and displayed/opened locations |
| Portability/files | Reversible native path encoding or explicit rejection; consistent source eligibility; permission/transient I/O handling; Windows console/pipe and Linux watcher behavior | Native platform tests including non-UTF-8 names where supported, rename/permissions, invalid UTF-8 content and filesystem differences |
| Package/release | Replace placeholder `github.com/user/fxi` in Cargo/extension metadata; version output; test installation/upgrade and daemon binary-version mismatch | Clean-machine install smoke tests, executable identity, protocol/version diagnostics and package contents |
| Benchmarking | All output modes and result counts; independent expected answers; cold/warm/direct/API separation; representative edits; concurrent load and long-lived memory; noise/uncertainty | Pinned builds/corpora, randomized/interleaved controls, raw samples, exact completeness checks, p50/p95/p99/CPU/RSS/disk together |
| Test architecture | Assertions against independent semantics, not two paths sharing the same bug; parser/index/protocol fuzz corpora; fault injection; CLI/PTY/provider tests | Turn each reproduced defect into a failing regression before fixes; enforce bounded failure and exact-or-error results |

## Where performance work should resume

1. **Update tail latency and concurrency:** decouple durable publication from live
   indexing with per-path revisions and safe retirement/rebase. Fix shutdown and
   mutation coherence first, or concurrency will make those bugs harder to reason
   about. Avoid a stale publisher overwriting a newer query snapshot.
2. **Phrase and one-shot startup gaps:** the previous comparable round-six API
   phrase result still favored Zoekt, and one-shot absence favored csearch. Those
   historical results are motivation, not current remeasured rankings. Build a
   fresh common comparison after correcting semantics.
3. **Memory/disk/build/compaction:** streaming merges, bounded pipeline stages,
   compact metadata and optional source-pack compression can improve dimensions
   that single-query latency hides. Default and experimental packed indexes need
   separate cost reporting.
4. **Content/count/ranked workloads:** files-only speed does not imply cheap
   full results. A better row/span collector can fix correctness and reduce
   allocation/verification work together. Ranking optimizations must remain exact
   or explicitly declare approximation.
5. **Cold and sustained operational workloads:** multiple roots/clients, long
   bursts, slow storage and index growth need measurements before any claim that
   FXI is comprehensively superior.

The [architecture report](architecture-and-integration.md) gives code locations,
previous measured gaps and concrete experiments for each optimization. No claim
of a novel indexing technique or a new speedup follows from this audit.

## Suggested implementation sequence

- **Safety and completeness:** bound query complexity/numerics; reject corrupt
  compaction; distinguish transient read failure; reclaim bad/idle connections.
- **Search contract:** separate predicates/lines/spans; fix parser/filter/bounds
  semantics and ranked limit precedence; property-test all modes.
- **Lifecycle:** explicit generation reload/unload and coordinated shutdown;
  assert CLI completion corresponds to the daemon's resulting state.
- **CLI and clients:** scope/modes/help/status/output, terminal guards and editor
  errors, connection/Unicode integration fixes. Document intentional compatibility
  changes rather than silently changing scripts.
- **Then optimize:** choose one measured bottleneck, preserve correctness gates,
  benchmark against current controls, and retain or reject the experiment based
  on end-to-end benefits and resource tradeoffs.

## Limits and retained strengths

No real VS Code visual session, Linux non-UTF-8 filename execution, Windows
interactive-console audit, full power-loss campaign or new speed comparison was
performed. The local macOS filesystem rejected the invalid filename used by the
path-byte probe; that finding remains code-inspected. Huge-allocation malformed
indexes were not executed. Source-context version mixing is a reviewed risk,
not a deterministic reproduced race. A daemon-only initial-query TUI path is a
latent API issue; the current CLI does not expose an initial-query argument.

The current conservative regex planner passed this audit's independent grid.
Immutable generation leases, source-cache validation and separate live/durable
readers remain good foundations. These findings do not invalidate the recorded
save-to-search improvement; they show why that improvement is insufficient to
claim overall correctness or product superiority.
