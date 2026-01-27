# Specification Gaps Analysis

This document identifies gaps between the `spec.md` specification and the current implementation of fxi.

## Summary

| Category | Gap Count | Priority |
|----------|-----------|----------|
| Query Language | 4 | High |
| Ranking/Scoring | 5 | High |
| TUI Features | 1 | Medium |
| Failure Handling | 4 | Medium |
| Build Process | 2 | Low |
| Configuration | 1 | Low |

---

## Query Language Gaps

### 1. `mtime:` Filter (spec 9.4)

**Spec says:**
> `mtime: ranges`

**Current state:** Not implemented. The `QueryFilters` struct has no mtime field, and the parser doesn't recognize `mtime:`.

**Impact:** Medium - useful for finding recently modified files

**Implementation notes:**
- Add `mtime_min: Option<u64>` and `mtime_max: Option<u64>` to `QueryFilters`
- Parse `mtime:>TIMESTAMP`, `mtime:<TIMESTAMP`, `mtime:YYYY-MM-DD` formats
- Apply filter in `executor.rs` similar to size filter

---

### 2. `near:` Proximity Search (spec 9.5)

**Spec says:**
> `near:foo,bar,20` - matches when foo and bar appear within 20 lines of each other

**Current state:** Not implemented. The parser and executor have no proximity search logic.

**Impact:** High - very useful for code search to find related terms

**Implementation notes:**
- Add `NearQuery { terms: Vec<String>, distance: u32 }` to `QueryNode`
- In verification phase, track line numbers of matches and check proximity
- Consider both word-level and line-level proximity

---

### 3. `^` Boost Priority (spec 9.6)

**Spec says:**
> `^foo` - boost priority for term

**Current state:** Not implemented. All terms have equal weight.

**Impact:** Medium - allows fine-tuning search relevance

**Implementation notes:**
- Add boost field to literal nodes
- Apply multiplier to scores for boosted terms

---

### 4. `+field:` Field Boost (spec 9.6)

**Spec says:**
> `+path:src` - boost results matching this filter

**Current state:** Not implemented.

**Impact:** Low - useful but not critical

**Implementation notes:**
- Similar to boost priority, apply score multiplier for boosted filters

---

## Ranking/Scoring Gaps

### 5. Score Factors (spec 10.4)

**Spec says:**
> Score factors:
> - match count
> - proximity
> - filename match
> - directory depth
> - recency

**Current state:** All results have `score = 1.0` (hardcoded in `executor.rs:253`). Scoring is not calculated.

**Impact:** High - without proper scoring, sort-by-score is meaningless

**Implementation notes:**
- Calculate `match_count` during verification
- Add `proximity_score` for multiple term queries
- Boost score if match appears in filename
- Reduce score for deeply nested paths
- Factor in `mtime` for recency bonus
- Make scoring weights configurable (spec 17)

**Location:** `src/query/executor.rs:245-254`

---

### 6. Early Termination (spec 10.5)

**Spec says:**
> Stop when:
> - top-K satisfied
> - score drops below threshold

**Current state:** Not implemented. All candidates are processed regardless of limit.

**Impact:** Medium - affects performance on large result sets

**Implementation notes:**
- Track minimum score in top-K heap
- Skip verification for candidates unlikely to make top-K
- Requires scoring to be implemented first

---

## TUI Gaps

### 7. Toggle Regex/Literal Mode (spec 11)

**Spec says:**
> Keybindings:
> - toggle regex/literal

**Current state:** No keybinding to toggle between regex and literal search mode.

**Impact:** Low - users can manually type `re:/pattern/`

**Implementation notes:**
- Add `search_mode: SearchMode` to `App` state
- Add keybinding (e.g., `Ctrl+R`) to toggle
- Auto-wrap query in `re:/` or strip it based on mode

---

## Failure Handling Gaps (spec 15)

### 8. Atomic Segment Writes

**Spec says:**
> - atomic segment writes

**Current state:** Writes directly to target paths without atomic operations.

**Impact:** Medium - corruption risk if interrupted during write

**Implementation notes:**
- Write to temporary file first
- Use `std::fs::rename` for atomic move
- Implement for all index files (meta.json, docs.bin, etc.)

**Location:** `src/index/writer.rs:131-162`

---

### 9. Checksum Validation

**Spec says:**
> - checksum validation

**Current state:** No checksums stored or validated.

**Impact:** Medium - cannot detect silent corruption

**Implementation notes:**
- Add CRC32 or xxhash to segment files
- Validate on read
- Store checksums in meta.json

---

### 10. Auto Rebuild on Corruption

**Spec says:**
> - auto rebuild on corruption

**Current state:** Errors out on corruption with no recovery.

**Impact:** Medium - requires manual intervention

**Implementation notes:**
- Catch corruption errors in `IndexReader::open`
- Prompt user or automatically trigger rebuild
- Keep backup of previous good index

---

### 11. Versioned Meta Validation

**Spec says:**
> - versioned meta

**Current state:** Has version field but no validation or migration logic.

**Impact:** Low - will matter when format changes

**Implementation notes:**
- Check version on load
- Implement migration for old formats
- Error on incompatible future versions

---

## Build Process Gaps

### 12. External Sort for Large Builds (spec 7.3)

**Spec says:**
> External Sort:
> - Sort by key then doc_id
> - Chunked disk sort
> - Merge phase

**Current state:** Accumulates all trigram/token postings in memory using `BTreeMap`.

**Impact:** Low-Medium - limits max indexable codebase size by RAM

**Implementation notes:**
- Emit (trigram, doc_id) pairs to temp files
- External merge sort
- Stream merge into final postings
- Critical for "millions of files" goal in spec

**Location:** `src/index/writer.rs:19-25` (in-memory maps)

---

### 13. FST Path Store (spec 6.2)

**Spec says:**
> Path Store (paths.fst):
> - Minimal prefix compressed structure (FST or trie)

**Current state:** Uses simple length-prefixed format (`paths.bin`), not FST.

**Impact:** Low - affects index size but not functionality

**Implementation notes:**
- Use `fst` crate (already in dependencies)
- Build FST from sorted paths
- Enables efficient prefix/wildcard queries

**Location:** `src/index/writer.rs:187-204`

---

## Configuration Gaps

### 14. Ranking Weights Configuration (spec 17)

**Spec says:**
> User configurable:
> - ranking weights

**Current state:** `IndexConfig` exists but has no ranking weight fields.

**Impact:** Low - depends on scoring implementation

**Implementation notes:**
- Add weight fields to `IndexConfig`
- Load from config file
- Apply in scoring calculation

---

## Completed

- ✅ **Match Highlighting in Results** (spec 11: "highlighted matches") - Implemented in this PR

---

## Priority Recommendations

**High Priority (implement next):**
1. Result scoring (Gap #5) - Makes sort-by-score meaningful
2. Proximity search `near:` (Gap #2) - Core code search feature

**Medium Priority:**
3. Atomic writes (Gap #8) - Data safety
4. `mtime:` filter (Gap #1) - Commonly requested feature
5. Early termination (Gap #6) - Performance optimization

**Low Priority (future work):**
6. External sort (Gap #12) - Only needed for huge repos
7. Other gaps - Nice to have
