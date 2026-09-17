# Audit follow-up: fixes and remaining work

The original reports and captured outputs describe the pre-fix executable. They
are retained as historical evidence. This document tracks the implementation
follow-up; current user contracts are in [the README](../../README.md),
[semantics](../SEMANTICS.md) and [daemon API](../DAEMON_API.md).

## Implemented

| Finding | Change | Main regression coverage |
|---|---|---|
| Q1–Q2 | Bounded fallible parsing; finite boosts/scores; invalid requests leave daemon alive | Parser tests; executable malformed-query/Ping sequence |
| Q3–Q4 | File truth separated from positive lines; unique Boolean lines/counts | Executor Boolean/negative regressions |
| Q5–Q8 | Literal punctuation/escaping, strict complete parsing, explicit global filter scope | Parser expected AST/error cases; CLI mode/case tests |
| Q9–Q10 | Shared order-independent proximity window; strict validated numeric/calendar bounds | Proximity permutations; boundary/date cases |
| Q11 | Wire limits override implicit parser defaults and intersect explicit `top:N` | Ranked API regression with 180 matches |
| I1–I2 | Compaction validates before publication; read/traversal failures preserve generations and retry | Damaged components, unreadable source/subtree, permission restoration |
| I3–I4 | Producer shutdown barrier, final reconciliation, bounded persistence retries, completion reply, wakeable accept | Contended writers, final producer messages, immediate-save subprocess shutdown |
| I5–I7 | Checked IDs, sparse compaction remap, bounded decoding, strict metadata/path/posting validation | Sparse maximum IDs, malformed varints/counts/references, unchanged publication |
| I8 | Explicit rejection of unsupported non-UTF-8 paths | Reader/path/root validation tests |
| U1–U2 | File/subtree scope, explicit query/fixed/regex modes, case-preserving OR/word flags | CLI expected-result tests and independent ripgrep parity |
| U3 | CLI build/compact reload; daemon-aware remove; root lifecycle serialization | Non-watched mutations, removal/late hints, reload preview/failure tests |
| U4–U5 | Ordered merged context; RAII terminal restoration | CLI adjacent context; real controlling-PTY failure test |
| Context consistency risk | Context uses the same source snapshot as its verified matches | Rewrite/delete/truncate between verification and rendering, cached/uncached |
| Extension integration | Pending-socket ownership, late-event fencing, disposal, UTF-16 highlights, direct command launch and task filtering | Socket/provider/command tests; packaged VSIX asset inspection |
| Resource/operation defects | Bounded outgoing frames, bad/idle connection closure, bounded Windows writes, meaningful failure codes/status | Protocol budget, native transport and CLI lifecycle tests |

Additional fixes include isolated PID files with `FXI_SOCKET`, rejection of
nonpositive PIDs, live/tombstone statistics, version/help output, JSON/NUL paths,
quiet broken pipes, bounded/coalesced TUI search and preview work, surfaced editor
errors, and correct extension package metadata/assets.

The README, API, semantics, library examples, optimization guide and pagination
proposal have been rewritten or updated. Old unsupported speed multipliers and
whole-query-cache assumptions are removed from current guidance.

## Deliberate limits and work still open

- A result line currently exposes one matching span. Duplicate rows/counts are
  fixed; displaying every occurrence requires a multi-span result model.
- Filters are global. Ambiguous grouped/negated/OR filter syntax is rejected;
  arbitrary Boolean filter expressions are not implemented. Whole-word mode
  with boosted/proximity queries returns an explicit error.
- Search is not a transactional snapshot of the entire tree. Watcher delivery
  and indexing take time; searchable and durable updates remain distinct.
- Query queues/results and concurrent retained snapshots are not all bounded by
  one process-wide memory budget. The frame limit prevents unreadable responses,
  but does not prevent all expensive work before serialization. Context snapshot
  retention can increase peak memory for broad searches.
- Global admission/cancellation, revision-safe background publication, streaming
  compaction, allocation reduction, revision-aware pagination, and richer
  integrity/explain diagnostics remain experiments/design work.
- Native Windows pipe behavior and Linux filesystem behavior need native CI;
  a cross-compile alone is not runtime evidence. Power-loss fault injection and
  a real VS Code visual session are not claimed.
- No universal-best search-tool claim follows from these repairs. Historical
  rankings are not automatically current after correctness changes. In
  particular, strict posting validation adds reader-opening work and must be
  measured rather than hidden.

## Validation and performance

Final validation commands, counts and controlled before/after measurements are
recorded in [VALIDATION.md](VALIDATION.md). Performance results apply only to the
recorded corpus, platform, output mode and binary hashes.
