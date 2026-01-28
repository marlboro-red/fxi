//! Suffix array builder
//!
//! Builds a suffix array from a collection of documents by:
//! 1. Concatenating all document content with sentinel separators
//! 2. Building a sorted suffix array using parallel sort
//!
//! The resulting suffix array enables O(m log n) substring search.

use super::types::*;
use crate::index::types::DocId;
use rayon::prelude::*;

/// Builder for constructing suffix arrays from documents
pub struct SuffixArrayBuilder {
    config: SuffixArrayConfig,
    /// Concatenated text (case-folded if configured)
    text: Vec<u8>,
    /// Document boundaries in the concatenated text
    boundaries: Vec<DocBoundary>,
    /// Current position in the text buffer
    current_pos: TextPosition,
    /// Count of excluded documents
    excluded_count: u32,
}

impl SuffixArrayBuilder {
    /// Create a new suffix array builder with the given configuration
    pub fn new(config: SuffixArrayConfig) -> Self {
        Self {
            config,
            text: Vec::new(),
            boundaries: Vec::new(),
            current_pos: 0,
            excluded_count: 0,
        }
    }

    /// Create a builder with default configuration
    pub fn with_defaults() -> Self {
        Self::new(SuffixArrayConfig::default())
    }

    /// Add a document to the suffix array
    ///
    /// Returns `true` if the document was added, `false` if it was skipped
    /// (too large, binary, etc.)
    pub fn add_document(&mut self, doc_id: DocId, content: &[u8]) -> bool {
        // Skip files that are too large
        if content.len() as u64 > self.config.max_file_size {
            self.excluded_count += 1;
            return false;
        }

        // Skip binary files (files with null bytes in first 8KB)
        if is_likely_binary(content) {
            self.excluded_count += 1;
            return false;
        }

        // Skip empty files
        if content.is_empty() {
            return false;
        }

        let start = self.current_pos;

        // Append content (case-folded if configured)
        if self.config.case_insensitive {
            self.text
                .extend(content.iter().map(|&b| b.to_ascii_lowercase()));
        } else {
            self.text.extend_from_slice(content);
        }

        let end = self.text.len() as TextPosition;

        // Append sentinel to separate documents
        self.text.push(SENTINEL_BYTE);
        self.current_pos = self.text.len() as TextPosition;

        self.boundaries.push(DocBoundary { doc_id, start, end });

        true
    }

    /// Build the suffix array from accumulated documents
    ///
    /// This is the main computation - sorts all suffixes in parallel
    pub fn build(self) -> BuiltSuffixArray {
        let text = self.text;
        let n = text.len();

        if n == 0 {
            return BuiltSuffixArray {
                text: Vec::new(),
                suffix_array: Vec::new(),
                boundaries: Vec::new(),
                config: self.config,
                excluded_count: self.excluded_count,
            };
        }

        // Build suffix array using parallel sort
        // This is O(n log n) but very cache-friendly and parallelizes well
        let suffix_array = build_suffix_array_parallel(&text);

        BuiltSuffixArray {
            text,
            suffix_array,
            boundaries: self.boundaries,
            config: self.config,
            excluded_count: self.excluded_count,
        }
    }

    /// Get the current size of accumulated text
    pub fn text_size(&self) -> usize {
        self.text.len()
    }

    /// Get the number of documents added
    pub fn doc_count(&self) -> usize {
        self.boundaries.len()
    }

    /// Check if suffix array building is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

/// Result of building a suffix array
pub struct BuiltSuffixArray {
    /// Concatenated document text (case-folded if configured)
    pub text: Vec<u8>,
    /// Sorted suffix array (positions into text)
    pub suffix_array: Vec<SuffixEntry>,
    /// Document boundaries for position-to-doc mapping
    pub boundaries: Vec<DocBoundary>,
    /// Configuration used for building
    pub config: SuffixArrayConfig,
    /// Number of documents excluded from SA
    pub excluded_count: u32,
}

impl BuiltSuffixArray {
    /// Get metadata about this suffix array
    pub fn meta(&self) -> SuffixArrayMeta {
        SuffixArrayMeta {
            enabled: true,
            total_text_size: self.text.len() as u64,
            suffix_count: self.suffix_array.len() as u64,
            doc_count: self.boundaries.len() as u32,
            excluded_count: self.excluded_count,
            case_insensitive: self.config.case_insensitive,
        }
    }
}

/// Build suffix array using parallel sort
///
/// This approach:
/// 1. Creates array of all suffix positions [0, 1, 2, ..., n-1]
/// 2. Sorts positions by comparing the suffixes they point to
/// 3. Uses rayon for parallel sorting
///
/// Time: O(n log n) average case, O(n log^2 n) worst case
/// Space: O(n) for the suffix array
fn build_suffix_array_parallel(text: &[u8]) -> Vec<SuffixEntry> {
    let n = text.len();

    // Create initial array of positions
    let mut sa: Vec<SuffixEntry> = (0..n as SuffixEntry).collect();

    // Sort by suffix comparison
    // For large texts, use parallel sort
    if n > 100_000 {
        sa.par_sort_unstable_by(|&a, &b| {
            compare_suffixes(text, a as usize, b as usize)
        });
    } else {
        sa.sort_unstable_by(|&a, &b| {
            compare_suffixes(text, a as usize, b as usize)
        });
    }

    sa
}

/// Compare two suffixes lexicographically
///
/// Uses a bounded comparison to avoid worst-case O(n) comparisons
#[inline]
fn compare_suffixes(text: &[u8], a: usize, b: usize) -> std::cmp::Ordering {
    // Compare up to 256 bytes to bound worst-case comparison time
    // This is sufficient for most code patterns
    const MAX_COMPARE: usize = 256;

    let len_a = (text.len() - a).min(MAX_COMPARE);
    let len_b = (text.len() - b).min(MAX_COMPARE);

    let suffix_a = &text[a..a + len_a];
    let suffix_b = &text[b..b + len_b];

    suffix_a.cmp(suffix_b)
}

/// Check if content is likely binary
///
/// Examines first 8KB for null bytes or high ratio of non-text bytes
fn is_likely_binary(content: &[u8]) -> bool {
    let sample_size = content.len().min(8192);
    let sample = &content[..sample_size];

    // Check for null bytes (very strong indicator of binary)
    if sample.contains(&0) {
        return true;
    }

    // Check for high ratio of non-printable, non-whitespace bytes
    let non_text_count = sample
        .iter()
        .filter(|&&b| {
            // Allow printable ASCII, common whitespace, and UTF-8 continuation bytes
            !((b >= 0x20 && b <= 0x7E)  // Printable ASCII
                || b == b'\n'
                || b == b'\r'
                || b == b'\t'
                || (b >= 0x80 && b <= 0xBF)  // UTF-8 continuation
                || (b >= 0xC0 && b <= 0xFD)) // UTF-8 start bytes
        })
        .count();

    // If more than 10% are non-text bytes, likely binary
    non_text_count > sample_size / 10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_simple() {
        let mut builder = SuffixArrayBuilder::with_defaults();
        builder.add_document(1, b"banana");
        let built = builder.build();

        assert_eq!(built.boundaries.len(), 1);
        assert_eq!(built.text.len(), 7); // "banana" + sentinel
    }

    #[test]
    fn test_suffix_array_correctness() {
        let text = b"banana\x00";
        let sa = build_suffix_array_parallel(text);

        // Suffix array for "banana\0" should be:
        // 6: \0
        // 5: a\0
        // 3: ana\0
        // 1: anana\0
        // 0: banana\0
        // 4: na\0
        // 2: nana\0
        assert_eq!(sa, vec![6, 5, 3, 1, 0, 4, 2]);
    }

    #[test]
    fn test_case_insensitive() {
        let mut builder = SuffixArrayBuilder::new(SuffixArrayConfig {
            case_insensitive: true,
            ..Default::default()
        });
        builder.add_document(1, b"HELLO");
        let built = builder.build();

        assert_eq!(&built.text[..5], b"hello");
    }

    #[test]
    fn test_binary_detection() {
        assert!(is_likely_binary(b"hello\x00world"));
        assert!(!is_likely_binary(b"hello world"));
        assert!(!is_likely_binary(b"fn main() {\n    println!(\"hi\");\n}"));
    }

    #[test]
    fn test_skip_large_files() {
        let mut builder = SuffixArrayBuilder::new(SuffixArrayConfig {
            max_file_size: 100,
            ..Default::default()
        });

        let small = b"small file";
        let large = vec![b'x'; 200];

        assert!(builder.add_document(1, small));
        assert!(!builder.add_document(2, &large));

        let built = builder.build();
        assert_eq!(built.boundaries.len(), 1);
        assert_eq!(built.excluded_count, 1);
    }

    #[test]
    fn test_multiple_documents() {
        let mut builder = SuffixArrayBuilder::with_defaults();
        builder.add_document(1, b"hello");
        builder.add_document(2, b"world");
        builder.add_document(3, b"foo");

        let built = builder.build();

        assert_eq!(built.boundaries.len(), 3);
        // 5 + 1 + 5 + 1 + 3 + 1 = 16
        assert_eq!(built.text.len(), 16);

        // Verify boundaries
        assert_eq!(built.boundaries[0].doc_id, 1);
        assert_eq!(built.boundaries[0].start, 0);
        assert_eq!(built.boundaries[0].end, 5);

        assert_eq!(built.boundaries[1].doc_id, 2);
        assert_eq!(built.boundaries[1].start, 6);
        assert_eq!(built.boundaries[1].end, 11);

        assert_eq!(built.boundaries[2].doc_id, 3);
        assert_eq!(built.boundaries[2].start, 12);
        assert_eq!(built.boundaries[2].end, 15);
    }
}
