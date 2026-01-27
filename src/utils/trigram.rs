use crate::index::types::{bytes_to_trigram, Trigram};
use std::collections::HashSet;

/// Extract unique trigrams from content
pub fn extract_trigrams(content: &[u8]) -> HashSet<Trigram> {
    let mut trigrams = HashSet::new();

    if content.len() < 3 {
        return trigrams;
    }

    for window in content.windows(3) {
        let trigram = bytes_to_trigram(window[0], window[1], window[2]);
        trigrams.insert(trigram);
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
}
