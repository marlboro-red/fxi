//! Block-Max WAND / MaxScore Algorithm Implementation
//!
//! This module implements an optimized top-k retrieval strategy based on the
//! Block-Max WAND and MaxScore algorithms. These algorithms enable early
//! termination by computing upper bound scores for documents and pruning
//! candidates that cannot make it into the top-k results.
//!
//! ## Algorithm Overview
//!
//! 1. For each candidate document, compute an upper bound score based on
//!    document metadata (depth, mtime) and optimistic assumptions about
//!    match counts.
//!
//! 2. Process candidates in descending order of their upper bound scores.
//!    This ensures we see the most promising documents first.
//!
//! 3. Maintain a min-heap of the top-k results found so far. The minimum
//!    score in this heap becomes our threshold.
//!
//! 4. Skip (prune) any candidate whose upper bound score is below the
//!    current threshold - they cannot enter the top-k.
//!
//! 5. Once we've processed enough candidates that the remaining candidates'
//!    upper bounds are all below threshold, we can terminate early.
//!
//! ## Block-Max Extension
//!
//! The "Block-Max" variant divides posting lists into blocks, each with
//! a pre-computed maximum score. This allows skipping entire blocks when
//! their max score is below threshold. In this implementation, we apply
//! the concept at the document level using metadata-based upper bounds.

use crate::index::types::DocId;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::PathBuf;

/// A candidate document with its upper bound score for WAND processing.
/// Used in the max-heap to process documents in descending upper bound order.
#[derive(Debug, Clone)]
pub struct WandCandidate {
    /// Document ID
    pub doc_id: DocId,
    /// Full path to the file
    pub full_path: PathBuf,
    /// Relative path for display
    pub rel_path: PathBuf,
    /// File modification time
    pub mtime: u64,
    /// Upper bound score (maximum possible score for this document)
    pub upper_bound: f32,
    /// Directory depth (for scoring)
    pub depth: usize,
}

impl PartialEq for WandCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.doc_id == other.doc_id
    }
}

impl Eq for WandCandidate {}

impl PartialOrd for WandCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WandCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap: higher upper bounds come first
        self.upper_bound
            .partial_cmp(&other.upper_bound)
            .unwrap_or(Ordering::Equal)
    }
}

/// A result entry in the top-k min-heap.
/// We use a min-heap so we can efficiently find and replace the minimum.
#[derive(Debug, Clone)]
pub struct TopKEntry {
    /// Actual computed score (not upper bound)
    pub score: f32,
    /// Document ID
    pub doc_id: DocId,
    /// Path for results
    pub path: PathBuf,
    /// Modification time
    pub mtime: u64,
    /// All matches found in this file
    pub matches: Vec<(u32, String, usize, usize)>, // (line_num, content, start, end)
}

impl PartialEq for TopKEntry {
    fn eq(&self, other: &Self) -> bool {
        self.doc_id == other.doc_id
    }
}

impl Eq for TopKEntry {}

impl PartialOrd for TopKEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopKEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap: lower scores come first (will be popped first when full)
        // Reverse comparison for min-heap behavior with BinaryHeap (which is max-heap)
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(Ordering::Equal)
    }
}

/// Top-K heap for tracking the best results during WAND processing.
/// Uses a min-heap so we can efficiently track the threshold (minimum score in top-k).
pub struct TopKHeap {
    /// The heap (min-heap by score)
    heap: BinaryHeap<TopKEntry>,
    /// Maximum capacity (k)
    capacity: usize,
}

impl TopKHeap {
    /// Create a new top-k heap with the given capacity
    pub fn new(k: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(k + 1),
            capacity: k,
        }
    }

    /// Get the current threshold (minimum score to enter top-k).
    /// Returns 0.0 if heap is not yet full.
    #[inline]
    pub fn threshold(&self) -> f32 {
        if self.heap.len() >= self.capacity {
            // Peek returns the "max" which in our reversed Ord is the minimum score
            self.heap.peek().map(|e| e.score).unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// Check if a score could potentially enter the top-k
    #[inline]
    pub fn would_enter(&self, score: f32) -> bool {
        self.heap.len() < self.capacity || score > self.threshold()
    }

    /// Try to insert an entry into the top-k heap.
    /// Returns true if the entry was inserted.
    pub fn try_insert(&mut self, entry: TopKEntry) -> bool {
        if self.heap.len() < self.capacity {
            // Not full yet, always insert
            self.heap.push(entry);
            true
        } else if entry.score > self.threshold() {
            // Better than current minimum, replace it
            self.heap.pop();
            self.heap.push(entry);
            true
        } else {
            false
        }
    }

    /// Get the current number of entries
    #[inline]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Check if heap is empty
    #[inline]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Check if heap is at capacity
    #[inline]
    pub fn is_full(&self) -> bool {
        self.heap.len() >= self.capacity
    }

    /// Consume the heap and return entries sorted by score (descending)
    pub fn into_sorted_vec(self) -> Vec<TopKEntry> {
        let mut entries: Vec<_> = self.heap.into_vec();
        // Sort by score descending
        entries.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
        });
        entries
    }
}

/// Statistics from WAND processing for debugging and optimization
#[derive(Debug, Default, Clone)]
pub struct WandStats {
    /// Total candidates considered
    pub total_candidates: usize,
    /// Candidates pruned by upper bound check (never verified)
    pub pruned_by_upper_bound: usize,
    /// Candidates fully verified
    pub verified: usize,
    /// Candidates that made it into top-k
    pub entered_top_k: usize,
    /// Early termination occurred
    pub early_terminated: bool,
}

impl WandStats {
    /// Calculate the pruning efficiency (% of candidates pruned)
    pub fn pruning_efficiency(&self) -> f32 {
        if self.total_candidates == 0 {
            0.0
        } else {
            (self.pruned_by_upper_bound as f32 / self.total_candidates as f32) * 100.0
        }
    }
}

/// WAND processor for top-k retrieval with early termination.
///
/// This struct manages the candidate processing pipeline:
/// 1. Candidates are added with their upper bound scores
/// 2. They are processed in upper bound order
/// 3. Pruning skips candidates below threshold
/// 4. Early termination when safe
pub struct WandProcessor {
    /// Max-heap of candidates ordered by upper bound
    candidates: BinaryHeap<WandCandidate>,
    /// Top-k results heap
    top_k: TopKHeap,
    /// Processing statistics
    stats: WandStats,
    /// Limit (k value)
    limit: usize,
}

impl WandProcessor {
    /// Create a new WAND processor for top-k retrieval
    pub fn new(limit: usize) -> Self {
        Self {
            candidates: BinaryHeap::new(),
            top_k: TopKHeap::new(limit),
            stats: WandStats::default(),
            limit,
        }
    }

    /// Add a candidate document with its upper bound score
    pub fn add_candidate(&mut self, candidate: WandCandidate) {
        self.stats.total_candidates += 1;
        self.candidates.push(candidate);
    }

    /// Add multiple candidates at once (more efficient)
    pub fn add_candidates(&mut self, candidates: impl IntoIterator<Item = WandCandidate>) {
        for candidate in candidates {
            self.add_candidate(candidate);
        }
    }

    /// Get the current threshold score
    #[inline]
    pub fn threshold(&self) -> f32 {
        self.top_k.threshold()
    }

    /// Get the next candidate to process, if any.
    /// Returns None if no more candidates or all remaining are below threshold.
    pub fn next_candidate(&mut self) -> Option<WandCandidate> {
        while let Some(candidate) = self.candidates.pop() {
            // Check if this candidate can beat the threshold
            if candidate.upper_bound > self.threshold() || !self.top_k.is_full() {
                return Some(candidate);
            }

            // Candidate's upper bound is below threshold - prune it
            self.stats.pruned_by_upper_bound += 1;

            // Check for early termination: if the best remaining candidate
            // can't beat threshold, we're done
            if self.top_k.is_full() {
                // All remaining candidates have lower upper bounds (max-heap property)
                // so they can't beat threshold either
                self.stats.early_terminated = true;
                self.stats.pruned_by_upper_bound += self.candidates.len();
                return None;
            }
        }
        None
    }

    /// Record that a candidate was verified and submit its result
    pub fn submit_result(&mut self, entry: TopKEntry) {
        self.stats.verified += 1;
        if self.top_k.try_insert(entry) {
            self.stats.entered_top_k += 1;
        }
    }

    /// Record that a candidate was verified but had no matches
    pub fn record_no_match(&mut self) {
        self.stats.verified += 1;
    }

    /// Check if we have enough results and can consider stopping
    #[inline]
    pub fn has_enough_results(&self) -> bool {
        self.top_k.is_full()
    }

    /// Get processing statistics
    pub fn stats(&self) -> &WandStats {
        &self.stats
    }

    /// Consume the processor and return the final results
    pub fn into_results(self) -> (Vec<TopKEntry>, WandStats) {
        (self.top_k.into_sorted_vec(), self.stats)
    }

    /// Get the limit (k value)
    #[inline]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Get number of remaining candidates
    #[inline]
    pub fn remaining_candidates(&self) -> usize {
        self.candidates.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_top_k_heap_basic() {
        let mut heap = TopKHeap::new(3);

        // Initially threshold is 0
        assert_eq!(heap.threshold(), 0.0);
        assert!(!heap.is_full());

        // Add entries
        heap.try_insert(TopKEntry {
            score: 1.0,
            doc_id: 1,
            path: PathBuf::from("a.txt"),
            mtime: 0,
            matches: vec![],
        });
        heap.try_insert(TopKEntry {
            score: 3.0,
            doc_id: 2,
            path: PathBuf::from("b.txt"),
            mtime: 0,
            matches: vec![],
        });
        heap.try_insert(TopKEntry {
            score: 2.0,
            doc_id: 3,
            path: PathBuf::from("c.txt"),
            mtime: 0,
            matches: vec![],
        });

        // Now full, threshold is minimum = 1.0
        assert!(heap.is_full());
        assert_eq!(heap.threshold(), 1.0);

        // Try to insert something worse - should fail
        let inserted = heap.try_insert(TopKEntry {
            score: 0.5,
            doc_id: 4,
            path: PathBuf::from("d.txt"),
            mtime: 0,
            matches: vec![],
        });
        assert!(!inserted);
        assert_eq!(heap.len(), 3);

        // Insert something better - should succeed and evict lowest
        let inserted = heap.try_insert(TopKEntry {
            score: 4.0,
            doc_id: 5,
            path: PathBuf::from("e.txt"),
            mtime: 0,
            matches: vec![],
        });
        assert!(inserted);
        assert_eq!(heap.len(), 3);
        assert_eq!(heap.threshold(), 2.0); // 1.0 was evicted

        // Get sorted results
        let results = heap.into_sorted_vec();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].score, 4.0);
        assert_eq!(results[1].score, 3.0);
        assert_eq!(results[2].score, 2.0);
    }

    #[test]
    fn test_wand_candidate_ordering() {
        let mut heap = BinaryHeap::new();

        heap.push(WandCandidate {
            doc_id: 1,
            full_path: PathBuf::from("/a"),
            rel_path: PathBuf::from("a"),
            mtime: 0,
            upper_bound: 1.0,
            depth: 1,
        });
        heap.push(WandCandidate {
            doc_id: 2,
            full_path: PathBuf::from("/b"),
            rel_path: PathBuf::from("b"),
            mtime: 0,
            upper_bound: 3.0,
            depth: 1,
        });
        heap.push(WandCandidate {
            doc_id: 3,
            full_path: PathBuf::from("/c"),
            rel_path: PathBuf::from("c"),
            mtime: 0,
            upper_bound: 2.0,
            depth: 1,
        });

        // Should come out in descending upper_bound order (max-heap)
        assert_eq!(heap.pop().unwrap().upper_bound, 3.0);
        assert_eq!(heap.pop().unwrap().upper_bound, 2.0);
        assert_eq!(heap.pop().unwrap().upper_bound, 1.0);
    }

    #[test]
    fn test_wand_processor_early_termination() {
        let mut processor = WandProcessor::new(2);

        // Add candidates with decreasing upper bounds
        for i in 0..10 {
            processor.add_candidate(WandCandidate {
                doc_id: i,
                full_path: PathBuf::from(format!("/{}", i)),
                rel_path: PathBuf::from(format!("{}", i)),
                mtime: 0,
                upper_bound: 10.0 - i as f32,
                depth: 1,
            });
        }

        // Process first candidate (upper_bound = 10.0)
        let c1 = processor.next_candidate().unwrap();
        assert_eq!(c1.upper_bound, 10.0);
        processor.submit_result(TopKEntry {
            score: 9.0,
            doc_id: c1.doc_id,
            path: c1.rel_path,
            mtime: 0,
            matches: vec![],
        });

        // Process second candidate (upper_bound = 9.0)
        let c2 = processor.next_candidate().unwrap();
        assert_eq!(c2.upper_bound, 9.0);
        processor.submit_result(TopKEntry {
            score: 8.0,
            doc_id: c2.doc_id,
            path: c2.rel_path,
            mtime: 0,
            matches: vec![],
        });

        // Now threshold is 8.0
        // Third candidate has upper_bound = 8.0, which is NOT > 8.0
        // But we need to check if remaining candidates can still beat threshold
        let c3 = processor.next_candidate();

        // All remaining candidates have upper_bound <= 8.0
        // So they can't beat the threshold, early termination should occur
        assert!(c3.is_none());
        assert!(processor.stats().early_terminated);

        // Check stats
        let stats = processor.stats();
        assert_eq!(stats.verified, 2);
        assert!(stats.pruned_by_upper_bound > 0);
    }

    #[test]
    fn test_wand_stats() {
        let stats = WandStats {
            total_candidates: 100,
            pruned_by_upper_bound: 75,
            verified: 25,
            entered_top_k: 10,
            early_terminated: true,
        };

        assert_eq!(stats.pruning_efficiency(), 75.0);
    }
}
