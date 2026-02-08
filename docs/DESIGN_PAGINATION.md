# Design: Pagination for Daemon API

## Problem

The daemon returns all results in a single response. For large result sets (especially `ContentSearch` with context lines), this means:

- The client blocks until every match is found and serialized
- Large JSON payloads over the socket (potentially tens of MB)
- Clients that only need the first page of results still pay for computing all of them
- No way to request results 100-200 without receiving 1-200

## Approach

Add `offset` to requests and `total_matches` to responses. The daemon caches full result sets and serves slices from cache on paginated requests.

This is viable because the cache already stores full results for `Search`. `ContentSearch` needs a small fix (it currently caches post-truncation results — see "Cache fix" below).

## Protocol Changes

### Request

Add `offset` to both `Search` and `ContentSearch`. Use `#[serde(default)]` so existing clients are unaffected.

```rust
Request::Search {
    query: String,
    root_path: PathBuf,
    limit: usize,
    #[serde(default)]
    offset: usize,         // new — skip first N results
}

Request::ContentSearch {
    pattern: String,
    root_path: PathBuf,
    limit: usize,
    options: ContentSearchOptions,
    #[serde(default)]
    offset: usize,         // new — skip first N results
}
```

### Response

Add `total_matches` to both response types. Existing clients will see a new field but serde allows unknown fields by default in most languages.

```rust
pub struct SearchResponse {
    pub matches: Vec<SearchMatchData>,
    pub duration_ms: f64,
    pub cached: bool,
    pub total_matches: usize,      // new — total before offset/limit
}

pub struct ContentSearchResponse {
    pub matches: Vec<ContentMatch>,
    pub duration_ms: f64,
    pub files_with_matches: usize,
    pub total_matches: usize,      // new — total before offset/limit
}
```

### JSON Example

Request page 2 (results 100-199):

```json
{
  "type": "ContentSearch",
  "pattern": "TODO",
  "root_path": "/home/user/project",
  "limit": 100,
  "offset": 100,
  "options": { "context_before": 2, "context_after": 2, "case_insensitive": false, "files_only": false }
}
```

Response:

```json
{
  "type": "ContentSearch",
  "matches": [ "... results 100-199 ..." ],
  "duration_ms": 1.2,
  "files_with_matches": 47,
  "total_matches": 312
}
```

`duration_ms` will be low for cache-hit pages. `files_with_matches` is computed from the full result set, not the page.

## Implementation

### 1. Cache fix for ContentSearch (`daemon_unix.rs`, `daemon_windows.rs`)

**Current behavior (lines 949-967 in daemon_unix.rs):** `ContentSearch` applies `limit` before caching, and includes `limit` in the cache key. This means requesting `limit=50` and `limit=100` for the same query produces two separate cache entries with different, truncated results.

**Fix:** Cache the full result set (capped at `MAX_RESULTS_CAP`), then apply offset/limit when returning. Remove `limit` from the cache key.

Before:
```rust
// Cache key includes limit
let cache_key = format!(
    "{}\x00{}\x00{}\x00{}\x00{}\x00{}",
    pattern, options.context_before, options.context_after,
    options.case_insensitive, options.files_only, limit
);

// ... execute search ...

// Truncate, then cache the truncated result
let effective_limit = if limit == 0 { MAX_RESULTS_CAP } else { limit.min(MAX_RESULTS_CAP) };
let iter = matches.into_iter().take(effective_limit);
let match_data: Vec<ContentMatch> = iter.map(|m| /* ... */).collect();
cache.put(cache_key, (match_data.clone(), file_count));
```

After:
```rust
// Cache key does NOT include limit or offset
let cache_key = format!(
    "{}\x00{}\x00{}\x00{}\x00{}",
    pattern, options.context_before, options.context_after,
    options.case_insensitive, options.files_only
);

// ... execute search ...

// Cache full results (capped), then slice for response
let match_data: Vec<ContentMatch> = matches.into_iter()
    .take(MAX_RESULTS_CAP)
    .map(|m| /* ... */)
    .collect();
let total_matches = match_data.len();
cache.put(cache_key, (match_data.clone(), file_count));

// Apply offset + limit for the response
let page: Vec<ContentMatch> = match_data.into_iter()
    .skip(offset)
    .take(if limit == 0 { MAX_RESULTS_CAP } else { limit })
    .collect();
```

This also fixes a latent bug: currently, two requests with different `limit` values for the same pattern waste cache space with duplicate entries.

### 2. Search handler — add offset (`daemon_unix.rs`, `daemon_windows.rs`)

`Search` already caches full results. The only change is slicing with both offset and limit:

Before (line 787-791):
```rust
let mut result_matches = match_data;
if limit > 0 {
    result_matches.truncate(limit);
}
```

After:
```rust
let total_matches = match_data.len();
let result_matches: Vec<SearchMatchData> = match_data.into_iter()
    .skip(offset)
    .take(if limit == 0 { usize::MAX } else { limit })
    .collect();
```

Same for the cache-hit path (lines 734-744): add `.skip(offset)` and return `total_matches` from the cached vec's length.

### 3. Protocol types (`protocol.rs`)

- Add `offset: usize` with `#[serde(default)]` to both request variants
- Add `total_matches: usize` to `SearchResponse` and `ContentSearchResponse`

### 4. Client (`client_unix.rs`, `client_windows.rs`)

Add `offset` parameter to `search()` and `content_search()` methods. Default to `0` for backward compat.

### 5. CLI (`main.rs` or wherever CLI args are parsed)

Add `--offset` flag (optional, default 0). Most CLI users won't use this, but it enables scripting.

## Files to Change

| File | Change |
|------|--------|
| `src/server/protocol.rs` | Add `offset` to requests, `total_matches` to responses |
| `src/server/daemon_unix.rs` | Cache fix for ContentSearch, offset/limit slicing, total_matches |
| `src/server/daemon_windows.rs` | Same changes as unix daemon |
| `src/server/client_unix.rs` | Add `offset` parameter to search methods |
| `src/server/client_windows.rs` | Same changes as unix client |
| CLI entry point | Add `--offset` flag |

## Backward Compatibility

Fully backward compatible in both directions:

- **Old client, new server:** `offset` defaults to `0` via `#[serde(default)]`. Client sees an extra `total_matches` field in responses, which is ignored by default in JSON parsers.
- **New client, old server:** Client sends `offset` field, old server ignores unknown fields during deserialization (serde default). Response won't have `total_matches` — client should treat missing field as "unknown total".

## Edge Cases

| Case | Behavior |
|------|----------|
| `offset` >= total results | Return empty `matches`, `total_matches` reflects the real count |
| `offset` = 0, `limit` = 0 | Same as today — all results up to `MAX_RESULTS_CAP` |
| `offset` = 0, `limit` > 0 | Same as today — first N results |
| Negative offset | Not possible — `usize` is unsigned, serde rejects negative values |
| Index changes between pages | Results may shift. `total_matches` may differ between pages. This is acceptable — the alternative (cursor-based pagination with snapshot isolation) is far more complex and unnecessary for a code search tool where the index is mostly stable. |

## What This Doesn't Solve

- **Progressive rendering during a slow first query:** The first request still blocks until the full result set is computed and cached. Subsequent pages are fast (cache hit). If the first-page latency becomes a problem, streaming is the right answer — but pagination eliminates the need for streaming in the common case where the executor is fast and the bottleneck is payload size.
- **Memory pressure from large cached result sets:** Full results are cached. For pathological queries matching millions of lines, this could use significant memory. The existing `MAX_RESULTS_CAP` (10M) bounds this, but a tighter per-query cap (e.g., 100k) could be added alongside pagination since clients can always page through results.

## Estimated Scope

Small change. Protocol additions are mechanical. The only logic change is the ContentSearch cache fix and adding `.skip(offset)` in four places. No executor changes needed.
