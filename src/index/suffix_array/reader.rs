//! Suffix array reader
//!
//! Provides memory-mapped access to suffix arrays with O(m log n) search.

use super::types::*;
use crate::index::types::DocId;
use anyhow::{Context, Result};
use memmap2::Mmap;
use roaring::RoaringBitmap;
use std::fs::File;
use std::path::Path;

/// Reader for a suffix array segment
///
/// Uses memory-mapped files for efficient access without loading
/// the entire suffix array into memory.
pub struct SuffixArrayReader {
    /// Memory-mapped concatenated text
    text_mmap: Mmap,
    /// Memory-mapped suffix array
    sa_mmap: Mmap,
    /// Document boundaries (small enough to keep in memory)
    boundaries: Vec<DocBoundary>,
    /// Number of suffixes
    suffix_count: u64,
    /// Whether the SA was built case-insensitive
    case_insensitive: bool,
}

impl SuffixArrayReader {
    /// Open a suffix array from a segment directory
    ///
    /// Returns `Ok(None)` if no suffix array files exist (backwards compatibility)
    pub fn open(segment_path: &Path) -> Result<Option<Self>> {
        let concat_path = segment_path.join("concat.bin");
        let sa_path = segment_path.join("sa.bin");
        let idx_path = segment_path.join("concat.idx");

        // Check if SA files exist (backwards compatibility with old indexes)
        if !concat_path.exists() || !sa_path.exists() || !idx_path.exists() {
            return Ok(None);
        }

        // Memory-map the concatenated text
        let text_file = File::open(&concat_path)
            .context("Failed to open concat.bin")?;
        let text_mmap = unsafe { Mmap::map(&text_file)? };

        // Memory-map the suffix array
        let sa_file = File::open(&sa_path)
            .context("Failed to open sa.bin")?;
        let sa_mmap = unsafe { Mmap::map(&sa_file)? };

        // Validate and read suffix array header
        if sa_mmap.len() < SuffixArrayHeader::SIZE {
            anyhow::bail!("Invalid sa.bin: file too small");
        }

        let magic = u32::from_le_bytes(sa_mmap[0..4].try_into().unwrap());
        if magic != SA_MAGIC {
            anyhow::bail!("Invalid sa.bin: bad magic number");
        }

        let version = u32::from_le_bytes(sa_mmap[4..8].try_into().unwrap());
        if version != SA_VERSION {
            anyhow::bail!("Unsupported sa.bin version: {}", version);
        }

        let suffix_count = u64::from_le_bytes(sa_mmap[8..16].try_into().unwrap());

        // Read document index
        let boundaries = Self::read_index(&idx_path)?;

        Ok(Some(Self {
            text_mmap,
            sa_mmap,
            boundaries,
            suffix_count,
            case_insensitive: true, // Assumed for now; could be stored in header
        }))
    }

    /// Read document index from concat.idx
    fn read_index(idx_path: &Path) -> Result<Vec<DocBoundary>> {
        let data = std::fs::read(idx_path)?;

        if data.len() < ConcatIndexHeader::SIZE {
            anyhow::bail!("Invalid concat.idx: file too small");
        }

        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic != SA_MAGIC {
            anyhow::bail!("Invalid concat.idx: bad magic number");
        }

        let doc_count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

        let mut boundaries = Vec::with_capacity(doc_count);
        let mut offset = ConcatIndexHeader::SIZE;

        for _ in 0..doc_count {
            if offset + ConcatIndexEntry::SIZE > data.len() {
                break;
            }

            let doc_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            let start = u64::from_le_bytes(data[offset + 4..offset + 12].try_into().unwrap());
            let end = u64::from_le_bytes(data[offset + 12..offset + 20].try_into().unwrap());

            boundaries.push(DocBoundary { doc_id, start, end });
            offset += ConcatIndexEntry::SIZE;
        }

        Ok(boundaries)
    }

    /// Get suffix at index i in the suffix array
    #[inline]
    fn get_suffix(&self, i: u64) -> TextPosition {
        let byte_offset = SuffixArrayHeader::SIZE + (i as usize * 8);
        u64::from_le_bytes(
            self.sa_mmap[byte_offset..byte_offset + 8]
                .try_into()
                .unwrap(),
        )
    }

    /// Get text starting at a position
    #[inline]
    fn text_at(&self, pos: TextPosition) -> &[u8] {
        &self.text_mmap[pos as usize..]
    }

    /// Get the full text slice
    #[inline]
    pub fn text(&self) -> &[u8] {
        &self.text_mmap
    }

    /// Find document ID for a global text position using binary search
    pub fn position_to_doc(&self, pos: TextPosition) -> Option<DocId> {
        // Binary search for the document containing this position
        let idx = self
            .boundaries
            .binary_search_by(|b| {
                if pos < b.start {
                    std::cmp::Ordering::Greater
                } else if pos >= b.end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;

        Some(self.boundaries[idx].doc_id)
    }

    /// Get document boundary by doc_id
    pub fn get_boundary(&self, doc_id: DocId) -> Option<&DocBoundary> {
        // Linear search is fine since we typically have few lookups
        // and boundaries are small
        self.boundaries.iter().find(|b| b.doc_id == doc_id)
    }

    /// Search for a pattern in the suffix array
    ///
    /// Returns the range [lo, hi) of indices in the suffix array
    /// where all suffixes start with the pattern.
    pub fn search(&self, pattern: &[u8]) -> (u64, u64) {
        if pattern.is_empty() || self.suffix_count == 0 {
            return (0, 0);
        }

        // Apply case folding if the SA was built case-insensitive
        let search_pattern: Vec<u8>;
        let pattern = if self.case_insensitive {
            search_pattern = pattern.iter().map(|&b| b.to_ascii_lowercase()).collect();
            &search_pattern
        } else {
            pattern
        };

        let lo = self.lower_bound(pattern);
        let hi = self.upper_bound(pattern, lo);
        (lo, hi)
    }

    /// Find first index where suffix starts with pattern (or would if inserted)
    /// This is the lower bound of the range of matching suffixes
    fn lower_bound(&self, pattern: &[u8]) -> u64 {
        let mut lo: u64 = 0;
        let mut hi: u64 = self.suffix_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let suffix_pos = self.get_suffix(mid);
            let suffix = self.text_at(suffix_pos);

            // Compare the suffix with the pattern (only up to pattern length)
            let cmp_len = pattern.len().min(suffix.len());
            let suffix_prefix = &suffix[..cmp_len];

            if suffix_prefix < pattern {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        lo
    }

    /// Find first index where suffix does NOT start with pattern
    /// This is one past the upper bound of the range of matching suffixes
    fn upper_bound(&self, pattern: &[u8], start: u64) -> u64 {
        let mut lo = start;
        let mut hi: u64 = self.suffix_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let suffix_pos = self.get_suffix(mid);
            let suffix = self.text_at(suffix_pos);

            // Check if suffix starts with pattern
            let starts_with = suffix.len() >= pattern.len()
                && &suffix[..pattern.len()] == pattern;

            if starts_with {
                // This suffix matches, so upper bound is higher
                lo = mid + 1;
            } else {
                // This suffix doesn't match, upper bound is at most here
                hi = mid;
            }
        }

        lo
    }

    /// Search for pattern and return matching document IDs as a bitmap
    ///
    /// This is the primary search interface used by the query executor.
    pub fn search_doc_ids(&self, pattern: &[u8]) -> RoaringBitmap {
        let (lo, hi) = self.search(pattern);

        let mut doc_ids = RoaringBitmap::new();

        // Collect unique doc_ids from matches
        for i in lo..hi {
            let pos = self.get_suffix(i);
            if let Some(doc_id) = self.position_to_doc(pos) {
                doc_ids.insert(doc_id);
            }
        }

        doc_ids
    }

    /// Search for pattern and return detailed matches
    ///
    /// Returns up to `limit` matches with position information.
    pub fn search_with_positions(&self, pattern: &[u8], limit: usize) -> Vec<SuffixMatch> {
        let (lo, hi) = self.search(pattern);
        let match_count = (hi - lo) as usize;

        if match_count == 0 {
            return Vec::new();
        }

        let mut matches = Vec::with_capacity(match_count.min(limit));

        for i in lo..hi.min(lo + limit as u64) {
            let global_pos = self.get_suffix(i);

            if let Some(doc_id) = self.position_to_doc(global_pos) {
                // Calculate position within document
                if let Some(boundary) = self.get_boundary(doc_id) {
                    let position = (global_pos - boundary.start) as usize;

                    matches.push(SuffixMatch {
                        doc_id,
                        position,
                        global_position: global_pos,
                    });
                }
            }
        }

        matches
    }

    /// Get the number of matches for a pattern (without collecting doc_ids)
    pub fn count_matches(&self, pattern: &[u8]) -> u64 {
        let (lo, hi) = self.search(pattern);
        hi - lo
    }

    /// Check if pattern exists in the suffix array
    pub fn contains(&self, pattern: &[u8]) -> bool {
        let (lo, hi) = self.search(pattern);
        lo < hi
    }

    /// Get statistics about this suffix array
    pub fn stats(&self) -> SuffixArrayStats {
        SuffixArrayStats {
            text_size: self.text_mmap.len(),
            suffix_count: self.suffix_count,
            doc_count: self.boundaries.len(),
        }
    }
}

/// Statistics about a suffix array
#[derive(Debug, Clone)]
pub struct SuffixArrayStats {
    pub text_size: usize,
    pub suffix_count: u64,
    pub doc_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::suffix_array::builder::SuffixArrayBuilder;
    use crate::index::suffix_array::writer::SuffixArrayWriter;
    use std::fs;
    use tempfile::tempdir;

    fn setup_test_sa() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp_dir = tempdir().unwrap();
        let segment_path = temp_dir.path().join("seg_0001");
        fs::create_dir_all(&segment_path).unwrap();

        let mut builder = SuffixArrayBuilder::with_defaults();
        builder.add_document(1, b"hello world");
        builder.add_document(2, b"world hello");
        builder.add_document(3, b"foo bar baz");
        let built = builder.build();

        SuffixArrayWriter::write(&segment_path, &built).unwrap();

        (temp_dir, segment_path)
    }

    #[test]
    fn test_open_reader() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap();
        assert!(reader.is_some());

        let reader = reader.unwrap();
        assert_eq!(reader.boundaries.len(), 3);
    }

    #[test]
    fn test_search_basic() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap().unwrap();

        // Search for "hello" - should be in docs 1 and 2
        let doc_ids = reader.search_doc_ids(b"hello");
        assert!(doc_ids.contains(1));
        assert!(doc_ids.contains(2));
        assert!(!doc_ids.contains(3));
    }

    #[test]
    fn test_search_case_insensitive() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap().unwrap();

        // Search for "HELLO" should match (case insensitive)
        let doc_ids = reader.search_doc_ids(b"HELLO");
        assert!(doc_ids.contains(1));
        assert!(doc_ids.contains(2));
    }

    #[test]
    fn test_search_no_match() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap().unwrap();

        let doc_ids = reader.search_doc_ids(b"notfound");
        assert!(doc_ids.is_empty());
    }

    #[test]
    fn test_search_with_positions() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap().unwrap();

        let matches = reader.search_with_positions(b"hello", 100);
        assert_eq!(matches.len(), 2);

        // Check positions are correct
        let doc1_match = matches.iter().find(|m| m.doc_id == 1).unwrap();
        assert_eq!(doc1_match.position, 0); // "hello" at start of doc 1

        let doc2_match = matches.iter().find(|m| m.doc_id == 2).unwrap();
        assert_eq!(doc2_match.position, 6); // "hello" after "world " in doc 2
    }

    #[test]
    fn test_count_matches() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap().unwrap();

        // "o" appears multiple times
        let count = reader.count_matches(b"o");
        assert!(count > 0);

        // Non-existent pattern
        let count = reader.count_matches(b"xyz123");
        assert_eq!(count, 0);
    }

    #[test]
    fn test_backwards_compatibility() {
        let temp_dir = tempdir().unwrap();
        let segment_path = temp_dir.path().join("seg_0001");
        fs::create_dir_all(&segment_path).unwrap();

        // No SA files - should return None (not error)
        let reader = SuffixArrayReader::open(&segment_path).unwrap();
        assert!(reader.is_none());
    }

    #[test]
    fn test_position_to_doc() {
        let (_temp_dir, segment_path) = setup_test_sa();
        let reader = SuffixArrayReader::open(&segment_path).unwrap().unwrap();

        // Position 0 should be in doc 1
        assert_eq!(reader.position_to_doc(0), Some(1));

        // Position after first doc + sentinel should be in doc 2
        // "hello world" = 11 bytes + 1 sentinel = 12
        assert_eq!(reader.position_to_doc(12), Some(2));
    }
}
