# Architectural critique of FXI

Reviewed source: `c13b7b7`, 18 September 2026. This is a fresh source review informed
by the repository's measured experiments. It is not a new benchmark, complete
correctness proof, or a claim that old audit defects remain unfixed. Findings
below distinguish code observations, measured costs and proposed designs.

Subsequent changes: [implementation progress](IMPLEMENTATION.md) covers the
line-map correctness fix, opt-in lean profile and single-capture source packs.
[Capture measurements](../performance-single-capture/NOTES.md) document the
source-pack build improvements and remaining query tradeoffs.
[Further implementation measurements](../performance-architecture-next/NOTES.md)
cover regex line routing, streamed compaction, publication durability reuse and
startup validation. The numbered findings below describe the reviewed baseline. The new
[lean-profile measurements](../performance-lean-profile/NOTES.md) replace the
estimated savings below with measured results for that implementation.

## Assessment

FXI has strong foundations: conservative candidate generation, owned source
snapshots, immutable generations, reader leases, fallible parsing and tested
fallbacks. Its biggest problem is the mismatch between the evidence it builds
and the evidence ordinary searches can use. The resulting costs appear in
storage, cold-process startup, source verification and update publication.

A complete rewrite is not justified. More isolated fast paths, however, will
not resolve these structural costs. The next stage should establish explicit
file revisions, index capabilities and query evidence requirements, then remove
work that those contracts make unnecessary.

## 1. The default index buys capabilities the primary workload does not use

**Code observation:** `process_file_content_with` in
[`build.rs`](../../src/index/build.rs) extracts grams, tokens, token positions and
line offsets. The writer persists them. In
[`planner.rs`](../../src/query/planner.rs), `TokenLookup`, `TokenOrTrigram` and
`PositionalPhrase` are reserved/unemitted for normal parsed searches. Comments
correctly explain why token boundaries cannot safely narrow arbitrary
case-insensitive substring searches. Ordinary literal/phrase planning uses
conservative gram constraints followed by source verification.

The latest measured corpus spends approximately:

| Component | MiB |
| --- | ---: |
| Token positions | 246.54 |
| Token dictionary | 51.11 |
| Token postings | 23.37 |
| Line maps | 35.58 |
| Gram dictionary + postings | 208.97 |

The first three alone are about **321 MiB, or 56% of the 575 MiB unpacked index**.
Line maps are additional storage; ordinary executor paths derive lines from
verified source rather than using the public stored-line-map methods.
[Current component measurements](../performance-source-compression/builds.json).

This is not an argument to break the public token APIs. It is an argument to
stop making every CLI user pay for all library capabilities. Compressing token
metadata was useful, but it did not justify generating unused positions.

**Proposed direction:** explicit index capabilities, with a lean substring/regex
profile and optional token/position data. Readers must expose absence as a
capability condition, with a documented correct fallback or explicit error.
Updates and compaction must preserve capabilities, including mixed old indexes.
Do not silently return empty token answers for an index lacking token data.

**Acceptance:** measure full build time, disk, RSS and maintenance cost with and
without each capability; run every supported CLI mode against an independent
oracle. The 321 MiB is potential removable data on this fixture, not a measured
lean-index result or promised speedup.

## 2. A file revision is not yet the unit that binds all search evidence

**Code observation:** `ProcessedFile` retains extracted structures, not its
original source bytes. Source packs reread files later in
[`source_pack.rs`](../../src/index/source_pack.rs) and
[`compressed.rs`](../../src/index/source_pack/compressed.rs). Search candidate
selection uses indexed evidence; verification may read current live text or a
metadata-validated cache/pack. Token positions describe tokenization-time text.
The cached byte-position evidence in
[`source_positions.rs`](../../src/index/source_positions.rs) belongs to yet
another, explicitly owned source snapshot.

These are legitimate mechanisms individually. Their different provenance means
we cannot freely combine them. In particular, a position from an older index
cannot be assumed to address a later packed/live snapshot. The compressed block
filters correctly derive from their own captured bytes; that is essential.

**Product consequence:** the documented search contract prunes stale positives
but can miss new matches excluded by old candidate postings. A negative result
is not necessarily an exhaustive answer about the current filesystem. Direct
readers can also lag the watched daemon's visible snapshot.
[Current semantics](../SEMANTICS.md#freshness).

**Proposed direction:** capture each accepted file revision once and derive its
postings, optional byte/block evidence and optional packed bytes from that
capture. A bounded pipeline or temporary spool is needed; retaining an entire
repository's source in RAM would be a poor implementation. Associate evidence
with a revision and its provenance, not just document IDs and compatible sizes.

Choose explicit user contracts for indexed-snapshot searches and watched/live
searches. A watched mode needs complete dirty-file candidate inclusion after
its visibility barrier, rather than hoping live verification repairs stale
candidate omissions. Where filesystem notification loss requires reconciliation,
that limitation must remain visible.

A content hash identifies captured bytes but does not cheaply prove the current
filesystem still contains them. File identity/change tracking and platform
coherence remain separate requirements. Single capture also does not create a
transactional snapshot of the whole source tree.

## 3. The index usually tells us which files, not where to look

**Code observation:** normal gram postings narrow to document IDs. The executor
then checks source text. Existing rarest-first gram intersections, segment Bloom
filters and segment-local Boolean execution are worthwhile; this is not an
absence of planning optimization. But candidate verification still often costs
work proportional to candidate file bytes, not matching regions.

The new block-filtered pack and the daemon's bounded cached byte positions are
both responses to that limitation. They improve particular paths but introduce
two independently managed sources of finer-grained evidence.

**Proposed direction:** evaluate revision-bound byte anchors or block candidates
as an optional second stage of the primary search index. Keep document-level
postings for cheap broad pruning, then obtain candidate regions only where the
estimated verification savings exceed the evidence cost. Large files and
selective phrases are plausible targets; indexing every gram position could
make storage much worse.

Word positions are not interchangeable with byte positions. Unicode-insensitive
matching, punctuation and token boundaries require explicit recall proofs.
Every shortcut must either return a verified witness, prove absence for its
covered revision, or request more evidence.

**Acceptance:** measure candidate files, candidate bytes/blocks, bytes actually
read/decoded, evidence size and query time on held-out corpora. A faster Bloom
lookup alone is not enough. Our compression results show that avoiding decoding
can matter more than choosing a denser codec.

## 4. One-shot startup validates much more data than many queries need

**Code observation:** `SegmentReader::open` in
[`reader.rs`](../../src/index/reader.rs) validates every gram posting payload and
its document membership before exposing the segment. Token loading is already
lazy. Thus a query that could be rejected quickly still pays substantial gram
initialization cost. Faster varint validation helps the constant factor but
retains this relationship to total index size.

**Measured consequence:** earlier comparisons show selective/absent one-shot
losses to tgrep/csearch, while warm APIs avoid repeatedly opening the index.
The experimental negative-routing certificate is an existing attempt to reuse
validated evidence, not an absent feature.

**Proposed direction:** make validation boundaries part of the format design.
Amortize validation in resident readers now. For a future query-local reader,
validate a trustworthy routing directory up front and validate the posting
blocks the query depends on. Proving absence requires directory completeness;
checking only the bytes that happened to be found is unsound. Checksums detect
changes but do not themselves prove semantic correctness or unchanged live data.

Define whether an unrelated damaged component should prevent every query or be
reported by an explicit full integrity check. Lazy tokens already make this a
dependency-sensitive policy. Preserve exact-or-error behavior for every piece
of evidence used by the query. Do not trade corruption detection for a flattering
startup number.

## 5. Generation publication has costs proportional to inherited structure

**Code observation:** [`generation.rs`](../../src/index/generation.rs) recursively
hard-links inherited segment files into each new generation, falling back to
copies when linking fails. Publication recursively syncs the new generation,
including inherited files, then atomically replaces CURRENT. This is robust and
simple, but a tiny delta still touches a potentially large existing file tree.
Creating hard links also affects inode change times, complicating stamp-based
validation reuse.

**Proposed direction:** immutable segments with stable storage locations and a
small generation manifest referencing them. Publish and sync new data plus the
manifest; retain reader leases and durable reference-aware garbage collection.
Do not merely delete existing fsync calls. Crash ordering, orphan cleanup,
manifest integrity and Windows mapped-file lifetimes need explicit tests.

This is a storage-layout change with migration cost. It is justified only if
metadata-operation and publication traces show that inherited-tree work remains
material under representative segment counts and update histories.

## 6. Fast previews still share a scheduling bottleneck with persistence

**Code observation:** `run_watcher_processor` and `flush_expired_changes` in
[`daemon_core.rs`](../../src/server/daemon_core.rs) consume events and perform
flush work through one processor. Lock contention has a retry path, but actual
publication/compaction work can occupy that processor. Preview construction in
`IndexReader::with_memory_delta` clones the document vector and scans it for
superseded paths. Small input batches do not imply work proportional only to
the number of changed files.

This explains why fast isolated-save medians do not establish sustained-update
or multiple-root tail latency. An expensive root can delay processing for other
roots even though their source changes are small.

**Proposed direction:** per-root live update scheduling, immutable base metadata
plus a bounded replacement overlay, and independently scheduled durable work.
Use per-path revisions and publication watermarks: a publisher must retire only
the revisions it actually committed. Newer edits must survive rebasing after
publication, failure and compaction. This is a correctness protocol before it
is a concurrency optimization.

**Acceptance:** edits throughout deliberately slow commits, bursts crossing the
persistence deadline, large checkouts, multiple roots, failed publication and
restart. Record p95/p99, oldest pending edit age and durable lag, not only quiet
save medians.

## 7. Compaction materializes the corpus's merged evidence

**Code observation:** `merge_all_segments` in
[`compact.rs`](../../src/index/compact.rs) accumulates all merged gram/token
postings, line maps and token positions in maps/vectors, then sorts/deduplicates
and writes them. Document IDs are globally remapped. This is substantially more
than a streaming merge of a few bounded input buffers. Optional packs can also
require source recapture.

**Proposed direction:** sorted per-term iterators, bounded k-way merging and a
merge policy based on bytes and rewrite amplification. Evaluate stable external
file identity with segment-local IDs to avoid coupling every merge to a global
ID rewrite, but retain current ordering/liveness contracts during migration.

**Acceptance:** cumulative bytes written, peak RSS and foreground latency over
thousands of realistic updates. A single freshly built index cannot establish
maintenance quality. Strict validation and failure-without-publication stay.

## 8. Output modes have diverging execution and cost models

**Code observation:** [`executor.rs`](../../src/query/executor.rs) has separate
files-only, count, ranked and content paths. Source-pack literal/streaming-regex
acceleration lives in files-only execution. Ranked execution verifies the whole
candidate set, constructs results, sorts and finally truncates. This is correct
for exact ranking without score bounds, but expensive for small requested top-k.
Content rendering may retain source snapshots and creates line vectors.

**Proposed direction:** a shared prepared verifier with explicit capabilities
(existence, line counts, spans and retained source), feeding specialized result
collectors. Keep efficient mode-specific collectors; do not force all modes to
materialize a universal heavyweight result. Use a bounded exact top-k collector
to reduce result storage; early termination additionally requires valid score
bounds and cannot be inferred from a heap alone.

Make source revision and matching evidence common across collectors so each
optimization does not need to rediscover freshness, Unicode and boundary rules.
Regression coverage must compare output semantics, not only matching file sets.

## 9. Resource limits do not yet express the actual resource being consumed

**Code observation:** [`admission.rs`](../../src/server/admission.rs) bounds active
searches through execution and delivery, and transport response queues are
bounded. These are real improvements. But one selective request and one broad
content/ranked request consume the same admission unit. The shared text cache's
budget excludes other live allocations and snapshots retained by active
queries. Watcher/event channels use unbounded `mpsc::channel`; draining a bounded
number per iteration does not bound queue growth. Loaded-root metadata is held
in maps without a general resource-based eviction policy.

**Proposed direction:** cancellation/deadline checkpoints, bounded or coalesced
watcher queues that degrade safely to a reconciliation marker, and budgets for
active results, source snapshots and maintenance work. Track work or bytes as
well as request count. Bound query expansion before large result allocation.
Cancellation must release resources without abandoning a partially published
transaction; maintenance and search need different cancellation boundaries.

**Acceptance:** many cheap requests mixed with broad content queries, slow or
abandoned clients, multiple roots and prolonged event storms. Measure useful
throughput, tail latency, peak RSS and recovery after overload.

## 10. Public API failures still sometimes look like valid answers

**Code-confirmed example, not a newly reproduced CLI failure:**
`SegmentReader::get_line_map` uses
`read_line_maps(...).unwrap_or_default()`. The public `offset_to_line` returns 1
when no line map is found. A read/format error can therefore become a plausible
line number, and the failed lazy load is retained as an empty map. Ordinary CLI
verification currently derives lines from source, limiting the immediate blast
radius; the library API still has misleading error semantics.

**Proposed direction:** distinguish absent capability, unavailable accelerator,
corrupt required evidence and actual empty results in the type/API design.
Optional source-pack failure can legitimately request a live fallback; a public
line-number operation must either compute a correct fallback or return an error.
Add a focused malformed-line-map regression before changing this interface.

## 11. The client contract conceals useful consistency information

**Code observation:** search responses in
[`protocol.rs`](../../src/server/protocol.rs) expose results, timing and resolved
root, but not a visible revision or durable revision. Watch status exposes a
pending-change count. Clients cannot use those fields to ask whether a particular
save has become searchable or to bind subsequent pagination to one view.

**Proposed direction:** optional revision-bearing responses, an explicit wait
for a known accepted update, and revision-aware pagination if pagination is
implemented. A raw editor save requires an ingestion/barrier protocol; a daemon
counter alone does not prove the OS has delivered all preceding notifications.
Do not label a watcher counter a filesystem transaction boundary.

Query defaults also impose avoidable learning cost: bare terms are insensitive
substrings with whitespace AND, whereas fixed-string mode preserves spaces and
is case-sensitive unless requested otherwise. Documentation and explicit flags
now make this defensible, but future features should normalize once into a
single typed query plan rather than accumulating more mode-specific exceptions.

## 12. Performance policy has outgrown isolated thresholds and environment flags

There is already selectivity ordering, but source-pack choice still uses a
candidate-count threshold; filters follow textual narrowing in the planner;
several runtime choices depend on separate environment settings. Candidate
count alone cannot distinguish 128 tiny files from 128 multi-megabyte files.
The current experimental switches are useful for research, but should not become
a permanent substitute for explicit capabilities and observable decisions.

Add an `explain` facility reporting evidence requirements, estimated/actual
candidate bytes, rejected blocks, fallback reasons, validation time, source I/O,
result materialization and revision lag. Introduce a cost model only after those
measurements exist. Index cheap selective metadata where justified, then choose
filter order and verification strategy from measured costs. Avoid fitting all
thresholds to the same six Linux-source queries.

## Recommended order

1. Define revision/evidence/capability contracts and phase-level instrumentation.
   Repair the misleading line-map API as a bounded correctness change.
2. Build a lean index profile with explicit capability compatibility. This attacks
   a large measured storage cost without weakening substring semantics.
3. Prototype single-capture revision-bound source evidence and selective block/
   byte anchors. Compare them with current packs/cache, preserving the existing
   engine as a control.
4. Move to stable segment references and revision-safe background publication;
   measure slow-persistence and multiple-root workloads before replacing defaults.
5. Stream compaction and result collection, then add resource-aware scheduling
   and cancellation. Each step needs its own correctness and performance gates.
6. Address one-shot validation with a format-level proof of routing completeness,
   rather than disabling validation. Measure fresh-process and genuinely cold
   storage separately from warm resident readers.

The key architectural objective is to make ordinary cost proportional to the
query's required evidence and the update's changed revisions. FXI currently
achieves this in some paths, while other paths still pay for much of the corpus.
A defensible claim of overall leadership requires measuring those latter paths,
not extrapolating from the best files-only median.
