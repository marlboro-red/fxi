# Weakness follow-up

This tracks the six remaining weaknesses from the correctness/performance review.
It separates completed changes from research results and work still needed.

| Area | This round | Remaining acceptance criteria |
|---|---|---|
| Concurrent updates | Added controlled overlap regressions for new edits during preview/durable publication and removal during publication. All pass without a production change. | Exercise sustained multi-root updates and failures with concurrent searches; measure visibility tails. The single publisher can still delay unrelated roots. |
| Resource control | Process-wide daemon search admission; explicit overload; bounded Unix response queue; permits held through delivery; CLI respects overload. | Per-query allocation/result budgets, cancellation and deadlines. Admission does not bound total RSS or independently launched direct searches. |
| Index size | Identified approximately 373 MiB of token data in the existing 33-segment fixture; no new format shipped. | Measure lossless dictionary compression and build/read costs. Removing tokens requires an explicit capability/API contract; silently breaking public token APIs is unacceptable. |
| One-shot startup | Existing strict batched validation remains in place; no new shortcut shipped. | Profile remaining startup work. Any reusable validation proof must bind the opened immutable generation, all relevant dependencies and the validation contract. Preserve corruption detection. |
| Phrase search | Longer-gram precision lab: `struct file_operations` candidates fall from 3,787 to 1,240 with eight-byte constraints, preserving all matching files. | Measure query cost plus general-index storage/build overhead. Production constraints must originate from the same content as primary indexing, and preserve memory previews, deltas and compaction. |
| CLI/search usability | Help now explicitly describes global matching-line/file limits and matching-line counts; overload errors no longer silently launch another search. | Multiple spans per line, inversion and multiline need explicit semantics and differential tests before implementation. |

## New regression coverage

`src/server/lifecycle_tests.rs` blocks the live-reader swap after batch capture,
then delivers a repeated-path edit and a new-path edit. Both preview and durable
publication must retain that subsequent batch, and a final flush must agree with
live and reopened disk readers. A second test overlaps removal with publication,
checks that removal cannot acknowledge early, and verifies that late notifications
do not recreate the index. These controlled interleavings supplement the existing
sequential generated update oracle; they are not an exhaustive concurrency proof.

Admission tests cover saturation across connections, correlated overload responses
for both search APIs, status/ping availability, capacity recovery after handler
errors, and release of writing/queued permits after slow readers disconnect.
A fake-daemon CLI regression proves that an overload response cannot trigger direct
search fallback. Windows uses the same admission primitive and holds its permit
through bounded response writing; native platform CI remains essential.

## Research evidence

See [longer-gram precision](performance-phrase-grams/NOTES.md). The initial lab is
an offline upper-bound experiment on candidate selectivity, not a shipped speedup.
Selective persisted postings must distinguish a gram not stored from a gram absent
in every document. Cached live-source bytes are not a safe substitute for extracting
constraints during primary indexing.

The index-size research agent was stopped by automatic review with a possible
cybersecurity-risk flag before making format changes. No compression improvement
or index-size reduction is claimed from this round.
