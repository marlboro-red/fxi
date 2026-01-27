# Specification Gaps Analysis

This document identifies gaps between the `spec.md` specification and the current implementation of fxi.

## Summary

| Category | Gap Count | Status |
|----------|-----------|--------|
| Query Language | 1 remaining | 3 implemented ✅ (mtime, near, boost) |
| Ranking/Scoring | 4 | High priority |
| TUI Features | 1 | Medium priority |
| Failure Handling | 4 | Medium priority |
| Build Process | 2 | Low priority |
| Configuration | 1 | Low priority |

**Recently Implemented:**
- `mtime:` filter (spec 9.4) ✅
- `near:` proximity search (spec 9.5) ✅
- `^` boost priority (spec 9.6) ✅
- Line filter application (was parsed but not applied) ✅

---

## Query Language Gaps

### 1. `mtime:` Filter (spec 9.4) ✅ IMPLEMENTED

**Spec says:**
> `mtime: ranges`

**Current state:** ✅ Implemented. Supports:
- `mtime:>TIMESTAMP` - filter files modified after timestamp
- `mtime:<TIMESTAMP` - filter files modified before timestamp
- `mtime:YYYY-MM-DD` - filter files modified on a specific date

**Implementation:** Added mtime_min/mtime_max fields to QueryFilters, parser supports all formats, executor applies filter.

---

### 2. `near:` Proximity Search (spec 9.5) ✅ IMPLEMENTED

**Spec says:**
> `near:foo,bar,20` - matches when foo and bar appear within 20 lines of each other

**Current state:** ✅ Implemented. The parser recognizes `near:term1,term2,distance` syntax, planner creates narrowing plan using trigrams from all terms, and executor verifies proximity constraints.

**Implementation:** Added `Near { terms, distance }` variant to QueryNode and VerificationStep, with line-based proximity checking in executor.

---

### 3. `^` Boost Priority (spec 9.6) ✅ IMPLEMENTED

**Spec says:**
> `^foo` - boost priority for term

**Current state:** ✅ Implemented. Supports:
- `^term` - boost with default 2.0x multiplier
- `^N:term` - boost with custom multiplier (e.g., `^3:important`)
- `^1.5:term` - boost with float multiplier

**Implementation:** Added `BoostedLiteral { text, boost }` variant to QueryNode, scorer applies boost multiplier to final score.

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

### 5. Score Factors (spec 10.4) ✅ IMPLEMENTED

**Spec says:**
> Score factors:
> - match count
> - proximity
> - filename match
> - directory depth
> - recency

**Current state:** ✅ Implemented in `src/query/scorer.rs`. Scoring includes:
- Match count (logarithmic scaling to prevent huge files from dominating)
- Filename match bonus (configurable)
- Directory depth penalty (configurable, with max cap)
- Recency bonus (exponential decay based on mtime)
- All weights configurable via `ScoringWeights` struct

**Implementation:** See `src/query/scorer.rs` and integration in `src/query/executor.rs`

**Note:** Proximity scoring for multiple term queries is not yet implemented (depends on Gap #2 `near:` query)

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
- ✅ **Result Scoring** (Gap #5, spec 10.4) - Implemented configurable scoring based on match count, filename match, directory depth, and recency
- ✅ **`mtime:` Filter** (Gap #1, spec 9.4) - Filter by modification time with timestamp and date formats
- ✅ **`near:` Proximity Search** (Gap #2, spec 9.5) - Find terms within N lines of each other
- ✅ **`^` Boost Priority** (Gap #3, spec 9.6) - Boost term priority with configurable multiplier
- ✅ **Line Filter Application** - The `line:` filter was parsed but not applied; now fully functional

---

## Priority Recommendations

**High Priority (implement next):**
1. ~~Result scoring (Gap #5) - Makes sort-by-score meaningful~~ ✅ DONE
2. ~~Proximity search `near:` (Gap #2) - Core code search feature~~ ✅ DONE
3. ~~`mtime:` filter (Gap #1) - Commonly requested feature~~ ✅ DONE
4. ~~`^` boost priority (Gap #3) - Fine-tune relevance~~ ✅ DONE
5. ~~Line filter application - Was parsed but not applied~~ ✅ DONE

**Medium Priority:**
6. Atomic writes (Gap #8) - Data safety
7. Early termination (Gap #6) - Performance optimization
8. `+field:` boost (Gap #4) - Filter-based boosting

**Low Priority (future work):**
9. External sort (Gap #12) - Only needed for huge repos
10. Other gaps - Nice to have
