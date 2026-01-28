use crate::index::types::{bytes_to_trigram, Trigram};
use ahash::AHashSet;

/// Extract unique trigrams from content using optimized SIMD-friendly approach.
///
/// This implementation is optimized for large files by:
/// 1. Processing data in cache-friendly chunks
/// 2. Using AHashSet for faster hashing
/// 3. Minimizing bounds checks via direct indexing
/// 4. Enabling LLVM auto-vectorization through predictable access patterns
pub fn extract_trigrams(content: &[u8]) -> AHashSet<Trigram> {
    let len = content.len();
    if len < 3 {
        return AHashSet::new();
    }

    // Pre-allocate with estimated capacity (typical: 30-40% unique trigrams)
    let estimated_capacity = ((len - 2) / 3).max(64);
    let mut trigrams = AHashSet::with_capacity(estimated_capacity);

    // Process in chunks for better cache utilization and auto-vectorization
    // SIMD-friendly: process 8 trigrams at a time where possible
    let main_end = len.saturating_sub(10); // Leave room for safe indexing

    let mut i = 0;

    // Main loop: process 8 trigrams per iteration (unrolled for SIMD)
    while i < main_end {
        // Unroll 8 iterations for better instruction-level parallelism
        // This pattern enables LLVM to auto-vectorize the hash computations
        let t0 = bytes_to_trigram(content[i], content[i + 1], content[i + 2]);
        let t1 = bytes_to_trigram(content[i + 1], content[i + 2], content[i + 3]);
        let t2 = bytes_to_trigram(content[i + 2], content[i + 3], content[i + 4]);
        let t3 = bytes_to_trigram(content[i + 3], content[i + 4], content[i + 5]);
        let t4 = bytes_to_trigram(content[i + 4], content[i + 5], content[i + 6]);
        let t5 = bytes_to_trigram(content[i + 5], content[i + 6], content[i + 7]);
        let t6 = bytes_to_trigram(content[i + 6], content[i + 7], content[i + 8]);
        let t7 = bytes_to_trigram(content[i + 7], content[i + 8], content[i + 9]);

        trigrams.insert(t0);
        trigrams.insert(t1);
        trigrams.insert(t2);
        trigrams.insert(t3);
        trigrams.insert(t4);
        trigrams.insert(t5);
        trigrams.insert(t6);
        trigrams.insert(t7);

        i += 8;
    }

    // Handle remaining bytes
    while i + 2 < len {
        let trigram = bytes_to_trigram(content[i], content[i + 1], content[i + 2]);
        trigrams.insert(trigram);
        i += 1;
    }

    trigrams
}

/// Extract trigrams using SIMD-optimized batch processing.
/// Returns a Vec for cases where we don't need deduplication.
#[allow(dead_code)]
pub fn extract_trigrams_batch(content: &[u8]) -> Vec<Trigram> {
    let len = content.len();
    if len < 3 {
        return Vec::new();
    }

    let count = len - 2;
    let mut trigrams = Vec::with_capacity(count);

    // Use chunks for cache-friendly processing
    for chunk in content.windows(3) {
        trigrams.push(bytes_to_trigram(chunk[0], chunk[1], chunk[2]));
    }

    trigrams
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
        // Test the SIMD-optimized path with larger input
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
    fn test_extract_trigrams_batch() {
        let content = b"hello";
        let trigrams = extract_trigrams_batch(content);
        assert_eq!(trigrams.len(), 3); // Non-deduplicated count
    }
}
