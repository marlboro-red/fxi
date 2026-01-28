# Search and Indexing Audit Report

**Date**: 2026-01-28
**Scope**: Fine-grained analysis of search and indexing functionality
**Files Reviewed**: bloom.rs, trigram.rs, encoding.rs, reader.rs, writer.rs, executor.rs, planner.rs, scorer.rs, build.rs, types.rs, tokenizer.rs

---

## Critical Bugs

### 1. CRITICAL: mtime Unit Mismatch - Recency Scoring Broken

**Location**: `src/index/build.rs:277-280` vs `src/query/scorer.rs:66-68,128`

**Problem**: During indexing, file modification times are stored in **nanoseconds**:
```rust
// build.rs:277-280
let mtime = metadata
    .modified()
    .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64)
    .unwrap_or(0);
```

But the scorer uses `current_time` in **seconds**:
```rust
// scorer.rs:66-68
let current_time = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())  // SECONDS!
    .unwrap_or(0);
```

Then at line 128:
```rust
let age_secs = self.current_time.saturating_sub(mtime) as f32;
```

**Impact**: Since mtime (nanoseconds, ~10^18 for current dates) is vastly larger than current_time (seconds, ~10^9), `saturating_sub` returns 0. This means **ALL files appear to have just been modified**, receiving maximum recency bonus. Recency-based scoring is completely non-functional.

**Severity**: Critical - Core feature broken

---

### 2. CRITICAL: Unsafe Language Enum Transmute from Arbitrary u16

**Location**: `src/index/reader.rs:536`

**Problem**:
```rust
let language = unsafe { std::mem::transmute::<u16, Language>(lang_val) };
```

The `Language` enum has variants 0-31. If `lang_val > 31` (from a corrupted or malicious index file), this creates an **invalid enum value**, causing undefined behavior per Rust's safety guarantees.

**Impact**: A corrupted or maliciously crafted index file can trigger undefined behavior, potentially leading to crashes, memory corruption, or security vulnerabilities.

**Severity**: Critical - Undefined behavior

---

### 3. BUG: Empty Postings File Fallback mmaps Wrong File

**Location**: `src/index/reader.rs:90-92, 103`

**Problem**:
```rust
// Empty mmap for empty segment
unsafe { Mmap::map(&File::open(&index_path.join("meta.json"))?)? }
```

When a postings file (grams.postings or tokens.postings) doesn't exist, the code mmaps `meta.json` instead. If any code later reads from this mmap expecting binary delta-encoded postings data, it will interpret JSON text as binary data.

**Impact**: Potential crashes or incorrect search results for segments with missing postings files. The delta_decode function would interpret JSON bytes as varints, producing garbage doc_ids.

**Severity**: High

---

## Moderate Bugs

### 4. BUG: Token Query Strategy Mismatch

**Location**: `src/query/planner.rs:125-128` vs `src/utils/tokenizer.rs`

**Problem**: During indexing, `extract_tokens()` performs sophisticated tokenization:
- Splits camelCase: `getUserById` → ["get", "user", "by", "id"]
- Splits snake_case: `get_user_by` → ["get", "user", "by"]
- Lowercases all tokens

But during query planning for short queries (<3 chars, no trigrams):
```rust
// planner.rs:125-128
let tokens: Vec<_> = text
    .split_whitespace()
    .filter(|t| t.len() >= 2)
    .map(|t| t.to_lowercase())
    .collect();
```

This just splits on whitespace without camelCase/snake_case handling. So querying "getUserById" creates a lookup for "getuserbyid", which won't match indexed tokens ["get", "user", "by", "id"].

**Impact**: Short queries (< 3 chars, where token lookup is the only strategy) may miss matches for camelCase/snake_case identifiers.

**Severity**: Medium

---

### 5. BUG: saturating_add in Delta Decode Hides Corruption

**Location**: `src/utils/encoding.rs:92`

**Problem**:
```rust
prev = prev.saturating_add(delta);
```

If delta-encoded data is corrupted and contains invalid large values, `saturating_add` silently caps the result at `u32::MAX` instead of returning an error or panicking.

**Impact**: Silent index corruption. Multiple distinct documents could be mapped to `u32::MAX`, causing incorrect search results without any error indication.

**Severity**: Medium

---

### 6. BUG: Literal Matching Only Finds First Match Per Line

**Location**: `src/query/executor.rs:632-639`

**Problem**:
```rust
if let Some(pos) = finder.find(search_bytes) {
    matches.push((...));
}
```

Uses `find()` instead of `find_iter()`, so if a line contains `"foo foo foo"`, only the first "foo" is recorded.

**Impact**: The `match_count` used for scoring is actually "number of lines with matches" rather than "total match count". This affects scoring accuracy - a file with 10 occurrences of a term on 1 line scores the same as a file with 1 occurrence.

**Severity**: Low-Medium (affects ranking quality, not correctness)

---

### 7. BUG: Bloom Filter Merge Doesn't Verify num_hashes

**Location**: `src/utils/bloom.rs:164-168`

**Problem**:
```rust
pub fn merge(&mut self, other: &BloomFilter) {
    debug_assert_eq!(self.bits.len(), other.bits.len());
    // No check that num_hashes matches!
    for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
        *a |= *b;
    }
}
```

Merging bloom filters with different `num_hashes` would produce incorrect membership testing. The merged filter would use `self.num_hashes` for queries, but `other` was built with different parameters.

**Impact**: Potential false negatives after merge. Currently the codebase doesn't appear to merge filters across segments, so this is latent.

**Severity**: Low (currently latent)

---

## Potential Issues

### 8. Unsafe UTF-8 Conversion in Tokenizer

**Location**: `src/utils/tokenizer.rs:34, 49-50, 70`

**Problem**:
```rust
// SAFETY: we only process ASCII bytes
let s = unsafe { std::str::from_utf8_unchecked(slice) };
```

The code filters for ASCII bytes (`< 128`), but slicing could theoretically include bytes from before the filter logic ran if there's a subtle logic error.

**Impact**: Potential undefined behavior with edge-case inputs. Low risk given the current filtering logic.

**Severity**: Low

---

### 9. Integer Overflow in mtime Conversion

**Location**: `src/index/build.rs:279`

**Problem**:
```rust
.as_nanos() as u64
```

`as_nanos()` returns `u128`. Casting to `u64` silently truncates for dates after ~2554 CE. More immediately, this is part of the mtime unit bug (#1).

**Severity**: Very Low (year 2554 problem, subsumed by bug #1)

---

## Summary

| ID | Severity | Component | Brief Description |
|----|----------|-----------|-------------------|
| 1 | **Critical** | scorer/build | mtime nanoseconds vs seconds - recency scoring broken |
| 2 | **Critical** | reader | Unsafe transmute allows UB from corrupted index |
| 3 | High | reader | Empty segment mmaps wrong file |
| 4 | Medium | planner | Token query doesn't match indexing tokenization |
| 5 | Medium | encoding | saturating_add hides data corruption |
| 6 | Low-Medium | executor | Only first match per line counted |
| 7 | Low | bloom | merge() doesn't verify num_hashes |
| 8 | Low | tokenizer | Unsafe UTF-8 with filtering assumptions |

---

## Recommended Fixes

### For Bug #1 (mtime units):
Change `build.rs:279` from `.as_nanos()` to `.as_secs()`:
```rust
.map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
```

### For Bug #2 (unsafe transmute):
Replace transmute with safe conversion:
```rust
let language = Language::try_from(lang_val).unwrap_or(Language::Unknown);
```
And implement `TryFrom<u16>` for `Language`.

### For Bug #3 (empty mmap):
Create a proper empty mmap or use a zero-length Vec:
```rust
let trigram_postings = if postings_path.exists() && postings_path.metadata()?.len() > 0 {
    let file = File::open(&postings_path)?;
    unsafe { Mmap::map(&file)? }
} else {
    // Return empty Vec wrapped appropriately
};
```

### For Bug #4 (token mismatch):
Use the same `extract_tokens()` function in the planner:
```rust
let tokens: Vec<_> = crate::utils::extract_tokens(text)
    .into_iter()
    .collect();
```

### For Bug #5 (saturating_add):
Use checked_add and return an error on overflow:
```rust
prev = prev.checked_add(delta).ok_or(DecodeError::Overflow)?;
```
