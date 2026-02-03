use crate::index::types::{bytes_to_trigram, Trigram};
use ahash::AHashSet;

/// Fixed-size bitset for all possible trigrams (2^24 = 16M trigrams)
/// Uses 2MB of memory but has O(1) insert/lookup with no hashing
struct FullTrigramBitset {
    bits: Vec<u64>, // 2^24 / 64 = 262144 u64s = 2MB
}

impl FullTrigramBitset {
    fn new() -> Self {
        Self {
            bits: vec![0u64; 262144], // 2^24 / 64
        }
    }

    #[inline]
    fn set(&mut self, trigram: Trigram) {
        let idx = (trigram >> 6) as usize;
        let bit = 1u64 << (trigram & 63);
        self.bits[idx] |= bit;
    }

    fn collect(self) -> Vec<Trigram> {
        let mut result = Vec::with_capacity(100_000);
        for (idx, word) in self.bits.into_iter().enumerate() {
            if word != 0 {
                let base = (idx as u32) << 6;
                let mut w = word;
                while w != 0 {
                    let bit_pos = w.trailing_zeros();
                    result.push(base | bit_pos);
                    w &= w - 1;
                }
            }
        }
        result
    }
}

/// Sparse bitset for tracking which trigrams have been seen.
/// Only allocates memory for 64-trigram blocks that are actually touched.
/// Much faster than the old 2MB full bitset for typical source files.
struct SparseTrigramBitset {
    /// Map from block index to 64-bit word
    blocks: ahash::AHashMap<u32, u64>,
}

impl SparseTrigramBitset {
    #[inline]
    fn new() -> Self {
        Self {
            // Typical source files touch ~500-2000 unique 64-trigram blocks
            blocks: ahash::AHashMap::with_capacity(512),
        }
    }

    /// Check if trigram is set and set it. Returns true if it was already set.
    #[inline]
    fn test_and_set(&mut self, trigram: Trigram) -> bool {
        let block_idx = trigram >> 6; // divide by 64
        let bit = 1u64 << (trigram & 63); // mod 64
        let word = self.blocks.entry(block_idx).or_insert(0);
        let was_set = (*word & bit) != 0;
        *word |= bit;
        was_set
    }

    /// Collect all set trigrams into a vector
    fn collect(self) -> Vec<Trigram> {
        let mut result = Vec::with_capacity(self.blocks.len() * 32);
        for (block_idx, word) in self.blocks {
            let base = block_idx << 6;
            let mut w = word;
            while w != 0 {
                let bit_pos = w.trailing_zeros();
                result.push(base | bit_pos);
                w &= w - 1;
            }
        }
        result
    }
}

/// Extract unique trigrams from content.
/// Uses different strategies based on file size for optimal performance.
pub fn extract_trigrams(content: &[u8]) -> Vec<Trigram> {
    if content.len() < 3 {
        return Vec::new();
    }

    // For small files, use simple sort+dedup (more cache-friendly)
    if content.len() < 4096 {
        let mut trigrams: Vec<Trigram> = content
            .windows(3)
            .map(|w| bytes_to_trigram(w[0], w[1], w[2]))
            .collect();
        trigrams.sort_unstable();
        trigrams.dedup();
        return trigrams;
    }

    // For medium files, use HashSet (tuned capacity for typical source files)
    if content.len() < 100_000 {
        // Tuned based on benchmarking - avoid over-allocation
        let capacity = (content.len() / 8).min(8000);
        let mut seen: AHashSet<Trigram> = AHashSet::with_capacity(capacity);
        for window in content.windows(3) {
            let trigram = bytes_to_trigram(window[0], window[1], window[2]);
            seen.insert(trigram);
        }
        return seen.into_iter().collect();
    }

    // For large files (<1MB), use sparse bitset
    if content.len() < 1_000_000 {
        let mut bitset = SparseTrigramBitset::new();
        for window in content.windows(3) {
            let trigram = bytes_to_trigram(window[0], window[1], window[2]);
            bitset.test_and_set(trigram);
        }
        return bitset.collect();
    }

    // For very large files (>1MB), use full bitset - O(1) per trigram, no hashing
    let mut bitset = FullTrigramBitset::new();
    for window in content.windows(3) {
        let trigram = bytes_to_trigram(window[0], window[1], window[2]);
        bitset.set(trigram);
    }
    bitset.collect()
}

/// Alias for extract_trigrams for backward compatibility
#[deprecated(note = "Use extract_trigrams instead, which now returns Vec<Trigram>")]
#[allow(dead_code)]
pub fn extract_trigrams_vec(content: &[u8]) -> Vec<Trigram> {
    extract_trigrams(content)
}

/// Extract trigrams from a query string for searching
pub fn query_trigrams(query: &str) -> Vec<Trigram> {
    let bytes = query.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }

    let mut trigrams = Vec::new();
    for window in bytes.windows(3) {
        let trigram = bytes_to_trigram(window[0], window[1], window[2]);
        trigrams.push(trigram);
    }
    trigrams.sort_unstable();
    trigrams.dedup();
    trigrams
}

/// Extract trigrams with their positions for phrase matching
#[allow(dead_code)]
pub fn extract_trigrams_with_positions(content: &[u8]) -> Vec<(Trigram, usize)> {
    let mut results = Vec::new();

    if content.len() < 3 {
        return results;
    }

    for (pos, window) in content.windows(3).enumerate() {
        let trigram = bytes_to_trigram(window[0], window[1], window[2]);
        results.push((trigram, pos));
    }

    results
}

/// Check if content is likely binary
pub fn is_binary(content: &[u8]) -> bool {
    let sample_size = content.len().min(8192);
    let sample = &content[..sample_size];

    // Check for null bytes
    let null_count = sample.iter().filter(|&&b| b == 0).count();
    if null_count > sample_size / 10 {
        return true;
    }

    // Check for high proportion of non-text bytes
    let non_text_count = sample
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();

    non_text_count > sample_size / 8
}

/// Check if content appears to be minified (very long lines)
pub fn is_minified(content: &[u8]) -> bool {
    let mut line_length = 0;
    let mut max_line_length = 0;
    let mut line_count = 0;

    for &byte in content.iter().take(65536) {
        if byte == b'\n' {
            max_line_length = max_line_length.max(line_length);
            line_length = 0;
            line_count += 1;
        } else {
            line_length += 1;
        }
    }

    // If average line is very long and max line is extremely long
    if line_count > 0 {
        let avg_line = content.len().min(65536) / (line_count + 1);
        return max_line_length > 1000 && avg_line > 500;
    }

    // Single line file longer than 10KB
    content.len() > 10240
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_trigrams() {
        let content = b"hello";
        let trigrams = extract_trigrams(content);
        assert_eq!(trigrams.len(), 3); // "hel", "ell", "llo"
    }

    #[test]
    fn test_extract_trigrams_large() {
        // Test the bitset-optimized path with larger input
        let content = b"abcdefghijklmnopqrstuvwxyz";
        let trigrams = extract_trigrams(content);
        assert_eq!(trigrams.len(), 24); // 26 - 2 = 24 unique trigrams
    }

    #[test]
    fn test_extract_trigrams_small() {
        // Edge cases
        assert_eq!(extract_trigrams(b"").len(), 0);
        assert_eq!(extract_trigrams(b"a").len(), 0);
        assert_eq!(extract_trigrams(b"ab").len(), 0);
        assert_eq!(extract_trigrams(b"abc").len(), 1);
    }

    #[test]
    fn test_extract_trigrams_returns_vec() {
        let content = b"hello";
        let trigrams = extract_trigrams(content);
        // Verify it returns a Vec (can call Vec-specific methods)
        assert_eq!(trigrams.len(), 3);
        assert!(!trigrams.is_empty());
    }

    #[test]
    fn test_extract_trigrams_large_bitset_path() {
        // Test the bitset path for larger content
        let content: Vec<u8> = (0..2000).map(|i| (i % 256) as u8).collect();
        let trigrams = extract_trigrams(&content);
        // Should have deduplicated trigrams
        assert!(trigrams.len() < content.len());
    }

    #[test]
    fn test_query_trigrams() {
        let query = "hello";
        let trigrams = query_trigrams(query);
        assert_eq!(trigrams.len(), 3);
    }

    #[test]
    fn test_is_binary() {
        assert!(!is_binary(b"hello world\n"));
        assert!(is_binary(b"\x00\x00\x00\x00\x00\x00\x00\x00"));
    }

    #[test]
    fn test_trigram_sparse_bitset() {
        let mut bitset = SparseTrigramBitset::new();

        // First insert should return false (wasn't set)
        assert!(!bitset.test_and_set(0x616263));

        // Second insert should return true (was already set)
        assert!(bitset.test_and_set(0x616263));

        // Different trigram should return false
        assert!(!bitset.test_and_set(0x626364));

        // Collect should have both
        let collected = bitset.collect();
        assert_eq!(collected.len(), 2);
    }
}
