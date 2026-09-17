# How fxi searches and where optimization belongs

This is an implementation guide, not a speed comparison. The executable contract
is in [SEMANTICS.md](SEMANTICS.md), the protocol in [DAEMON_API.md](DAEMON_API.md),
and remaining experiments in the [audit inventory](audit-2026-09-18/AUDIT.md).
Historical measurements live in `performance-round*/`; their binary hashes,
corpora and workloads matter. A result for warm files-only search says nothing
by itself about build time, content output, cold startup or update tail latency.

## Index and publication

The index combines byte trigrams with token postings and positions. Trigrams
narrow substring/regex candidates; tokens and positions support plans that can
use them safely. Fresh indexes retain common grams. Missing/omitted constraints
must broaden candidates, never prove that a valid substring is absent.

Builds discover eligible files, extract data in parallel and write immutable
segments. Incremental updates append segments and tombstones. A generation
manifest selects the published snapshot; readers lease generations so cleanup
cannot delete files they still need. Writers use a per-root advisory lock.
Compaction validates its inputs before publication and remaps live document IDs.
Source read failures preserve the previous published index rather than becoming
permanent exclusions.

Core metadata, paths, document membership and gram payloads are validated when
readers open them. Token postings/positions can be loaded and validated lazily.
This deliberately has a startup cost. Future validation shortcuts must establish
that the same immutable bytes were already checked; accepting a partial decoder
result would exchange correctness for speed.

## Query pipeline

1. Parse a bounded query, reject malformed syntax, and separate global filters
   from its Boolean expression.
2. Build conservative candidate constraints. Regex planning uses the Rust regex
   syntax tree: mandatory literals, alternatives, bounded small classes and
   required repetitions can supply grams. Complex/short/Unicode cases fall back
   to broader sets when a narrow proof is unavailable.
3. Apply file/subtree and query filters, combine postings and verify candidates
   against source content.
4. Collect the requested output: ordered paths, unique matching-line counts,
   content/context, or ranked matches. File predicates such as NOT do not invent
   matching source text. Ranked relevance is a heuristic, not BM25.
5. Render text/JSON or serialize a bounded protocol frame.

Whole-word mode transforms matchers while retaining each one's case semantics.
Bare query terms remain case-insensitive; phrases, fixed-string mode and regex
mode are case-sensitive unless requested otherwise. This is why benchmarks must
compare equivalent expressions and expected result sets, not merely equal strings.

## Warm readers and caches

A daemon avoids reopening its index on every request. It does not memoize whole
query answers: editable source can change while a generation remains unchanged.
Source snapshots are shared through bounded, sharded caches and checked against
metadata before reuse. Non-Unix validation also compares the current source bytes.
Concurrent queries may retain snapshots beyond cache admission limits, so the
cache budget is not a process-RSS limit.

Small positional witnesses can accelerate compatible literal verification on
unchanged cached content. Parallel candidate verification is bounded by configured
worker policies. Both optimizations retain ordinary verification fallbacks.
See [SEMANTICS.md](SEMANTICS.md#candidate-planning-and-verification) for exact
cache budgets, overrides and platform differences.

## Live versus durable updates

Native watcher events are hints. Precise paths use scoped reconciliation;
directory/ignore changes, ambiguous events and periodic repair use wider scans.
Eligible small changes first publish an in-memory index for search visibility.
Persistence later publishes the durable generation. These are separate milestones:
a daemon can see a save before a fresh direct reader can see it.

Small previews are rebuilt against the durable base and the whole pending path
set. Source budgets bound accepted preview input, not all allocations. Publication
and compaction still occupy the update processor, which can delay subsequent
notifications. Graceful shutdown stops producers and persists final work before
acknowledging success; forced termination provides no such guarantee.

## Optional experiments

- **Source packs** (`FXI_SOURCE_PACK=1` when building) store source copies in
  segments. Eligible Unix direct files-only scans can verify these bytes after
  checking source identity/stamps and pack integrity. They add disk/build cost.
- **Certified negative routing** (`FXI_NEGATIVE_ROUTING=1` for build and query)
  can establish absence for a narrow class of exact case-sensitive literals
  without ordinary startup. Certificates are tied to generation dependencies;
  changed or missing evidence falls back to full opening. Hardlink ctime changes
  can invalidate an otherwise useful certificate.

Neither experiment is a universal default improvement. Measure build, disk,
startup, warm queries and update behavior separately before enabling one.

## Next experiments and acceptance gates

| Area | Candidate improvement | Required evidence |
|---|---|---|
| Index opening | Reusable validated immutable manifests | Corruption failures preserved; startup/RSS measured |
| Update tails | Revision-safe background publication | No stale publisher overwrites newer visibility; p95/p99 and durability |
| Full builds | Byte-budgeted pipeline and less duplication | Throughput, peak RSS, skewed/large files and failure propagation |
| Compaction | Streaming merges and tiered policies | Exact before/after results, write amplification and peak RSS |
| Content/count | Shared prepared matchers and streaming collection | Unique-line semantics, allocation and broad-query latency |
| Ranking | Bounded exact top-k | Identical ordered results including late high scores |
| Server load | Global admission, cancellation and bounded queues | Slow clients, concurrent roots, bounded memory and recovery |
| Pagination | Revision-aware cursors | Defined source consistency and explicit expiration |

For each experiment, retain a binary baseline, hash the corpus and artifacts,
interleave repeated trials, and check exact expected answers outside timing.
Record raw samples and rejected ideas. Do not time while compilers/tests contend
for the same machine. New techniques earn adoption through correctness and
end-to-end gains, not an isolated inner-loop result.
