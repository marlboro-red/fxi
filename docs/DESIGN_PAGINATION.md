# Pagination: unimplemented design work

The daemon currently returns one bounded JSON response per request. It has no
`offset`, continuation token or `total_matches` pagination contract. See the
[current API](DAEMON_API.md) for the fields actually supported.

The previous proposal assumed a whole-query result cache. That assumption no
longer holds: sources can change without the index generation changing, so a
cached answer keyed only by generation can be stale.

A future implementation needs to choose and document a consistency model:

- **Snapshot cursor:** retain verified results or pinned source snapshots for a
  bounded lifetime. Later pages are stable, but consume memory and need eviction,
  cancellation and explicit cursor-expired errors.
- **Recomputed pages:** re-run against current sources. This uses less retained
  memory but edits can cause skipped/duplicate results between pages; callers
  must be told that pages do not form one snapshot.

Either design needs deterministic ordering, query/scope binding, byte and result
budgets, root-removal handling, and tests covering edits between pages. Ranked
search additionally needs exact global ordering before selecting a page, unless
an approximate contract is explicitly introduced.

Do not add `offset` alone and imply stable paging. Until a design is implemented,
clients should narrow the query or set an appropriate result limit; oversized
responses return an error rather than an unreadable frame.
