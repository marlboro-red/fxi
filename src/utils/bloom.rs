//! High-performance bloom filter for fast document pre-filtering.
//!
//! Uses multiple hash functions derived from a single ahash computation
//! for cache-friendly and SIMD-optimized membership testing.

use ahash::RandomState;
use std::hash::{BuildHasher, Hasher};

/// A space-efficient probabilistic data structure for fast membership testing.
///
/// Used to quickly reject documents that definitely don't contain certain trigrams
/// before doing expensive posting list lookups.
#[derive(Clone, Debug)]
pub struct BloomFilter {
    /// Bit array stored as u64 words for efficient access
    bits: Vec<u64>,
    /// Number of bits in the filter
    num_bits: usize,
    /// Number of hash functions to use
    num_hashes: u8,
}

impl BloomFilter {
    /// Create a new bloom filter optimized for the expected number of elements
    /// and desired false positive rate.
    ///
    /// # Arguments
    /// * `expected_elements` - Expected number of unique elements to insert
    /// * `false_positive_rate` - Desired false positive rate (e.g., 0.01 for 1%)
    pub fn new(expected_elements: usize, false_positive_rate: f64) -> Self {
        // Calculate optimal number of bits: m = -n * ln(p) / (ln(2)^2)
        let n = expected_elements.max(1) as f64;
        let p = false_positive_rate.max(0.0001).min(0.5);
        let ln2_sq = std::f64::consts::LN_2 * std::f64::consts::LN_2;

        let num_bits = ((-n * p.ln()) / ln2_sq).ceil() as usize;
        let num_bits = num_bits.max(64); // Minimum 64 bits

        // Round up to nearest u64
        let num_words = (num_bits + 63) / 64;
        let num_bits = num_words * 64;

        // Calculate optimal number of hash functions: k = (m/n) * ln(2)
        let num_hashes = ((num_bits as f64 / n) * std::f64::consts::LN_2).round() as u8;
        let num_hashes = num_hashes.clamp(1, 16);

        Self {
            bits: vec![0u64; num_words],
            num_bits,
            num_hashes,
        }
    }

    /// Create a bloom filter with specific parameters (for loading from disk)
    pub fn with_params(num_bits: usize, num_hashes: u8) -> Self {
        let num_words = (num_bits + 63) / 64;
        Self {
            bits: vec![0u64; num_words],
            num_bits: num_words * 64,
            num_hashes,
        }
    }

    /// Create from raw data (for loading from disk)
    pub fn from_raw(bits: Vec<u64>, num_hashes: u8) -> Self {
        let num_bits = bits.len() * 64;
        Self {
            bits,
            num_bits,
            num_hashes,
        }
    }

    /// Insert an element into the bloom filter
    #[inline]
    pub fn insert(&mut self, item: u32) {
        let (h1, h2) = self.hash_pair(item);

        for i in 0..self.num_hashes as u64 {
            // Double hashing: h(i) = h1 + i*h2
            let hash = h1.wrapping_add(i.wrapping_mul(h2));
            let bit_index = (hash as usize) % self.num_bits;
            let word_index = bit_index / 64;
            let bit_offset = bit_index % 64;
            self.bits[word_index] |= 1u64 << bit_offset;
        }
    }

    /// Check if an element might be in the set.
    /// Returns false if definitely not present, true if possibly present.
    #[inline]
    pub fn might_contain(&self, item: u32) -> bool {
        let (h1, h2) = self.hash_pair(item);

        for i in 0..self.num_hashes as u64 {
            let hash = h1.wrapping_add(i.wrapping_mul(h2));
            let bit_index = (hash as usize) % self.num_bits;
            let word_index = bit_index / 64;
            let bit_offset = bit_index % 64;

            if (self.bits[word_index] & (1u64 << bit_offset)) == 0 {
                return false;
            }
        }
        true
    }

    /// Check if ALL items might be contained (for query trigram sets)
    /// Returns false if ANY item is definitely not present.
    #[inline]
    pub fn might_contain_all(&self, items: &[u32]) -> bool {
        for &item in items {
            if !self.might_contain(item) {
                return false;
            }
        }
        true
    }

    /// Compute two hash values for double hashing using ahash
    #[inline]
    fn hash_pair(&self, item: u32) -> (u64, u64) {
        // Use two independent hashers with different seeds for proper double hashing.
        // Note: Reusing a hasher after finish() is undefined behavior and corrupts
        // the hash distribution, leading to higher false positive rates.
        let mut hasher1 = RandomState::with_seeds(0, 0, 0, 0).build_hasher();
        hasher1.write_u32(item);
        let h1 = hasher1.finish();

        let mut hasher2 = RandomState::with_seeds(
            0x517cc1b727220a95,
            0x9e3779b97f4a7c15,
            0xbf58476d1ce4e5b9,
            0x94d049bb133111eb,
        )
        .build_hasher();
        hasher2.write_u32(item);
        let h2 = hasher2.finish();

        (h1, h2)
    }

    /// Get the raw bits for serialization
    pub fn bits(&self) -> &[u64] {
        &self.bits
    }

    /// Get the number of hash functions
    pub fn num_hashes(&self) -> u8 {
        self.num_hashes
    }

    /// Get the number of bits
    pub fn num_bits(&self) -> usize {
        self.num_bits
    }

    /// Get approximate memory usage in bytes
    pub fn memory_usage(&self) -> usize {
        self.bits.len() * 8 + std::mem::size_of::<Self>()
    }

    /// Merge another bloom filter into this one (union)
    pub fn merge(&mut self, other: &BloomFilter) {
        debug_assert_eq!(self.bits.len(), other.bits.len());
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= *b;
        }
    }
}

impl Default for BloomFilter {
    fn default() -> Self {
        // Default: optimized for ~10k elements with 1% FPR
        Self::new(10000, 0.01)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_filter_basic() {
        let mut bf = BloomFilter::new(1000, 0.01);

        // Insert some items
        for i in 0..100 {
            bf.insert(i);
        }

        // All inserted items should be found
        for i in 0..100 {
            assert!(bf.might_contain(i), "Item {} should be found", i);
        }

        // Items not inserted should mostly not be found
        // (some false positives expected)
        let mut false_positives = 0;
        for i in 1000..2000 {
            if bf.might_contain(i) {
                false_positives += 1;
            }
        }

        // False positive rate should be roughly 1%
        assert!(false_positives < 50, "Too many false positives: {}", false_positives);
    }

    #[test]
    fn test_bloom_filter_trigrams() {
        let mut bf = BloomFilter::new(50000, 0.01);

        // Simulate trigram insertion
        let trigrams: Vec<u32> = (0..1000).map(|i| 0x616263 + i).collect();
        for &t in &trigrams {
            bf.insert(t);
        }

        // Check all trigrams
        assert!(bf.might_contain_all(&trigrams[0..10]));

        // Non-existent trigrams
        assert!(!bf.might_contain_all(&[0xFFFFFF, 0xFFFFFE]));
    }

    #[test]
    fn test_bloom_filter_merge() {
        let mut bf1 = BloomFilter::new(1000, 0.01);
        let mut bf2 = BloomFilter::new(1000, 0.01);

        for i in 0..50 {
            bf1.insert(i);
        }
        for i in 50..100 {
            bf2.insert(i);
        }

        bf1.merge(&bf2);

        // Both ranges should now be in bf1
        for i in 0..100 {
            assert!(bf1.might_contain(i));
        }
    }

    #[test]
    fn test_bloom_filter_false_positive_rate() {
        // Test that actual FPR is close to expected FPR
        let expected_fpr = 0.01; // 1%
        let num_elements = 10000;
        let num_test_elements = 100000; // Large sample for statistical accuracy

        let mut bf = BloomFilter::new(num_elements, expected_fpr);

        // Insert elements
        for i in 0..num_elements as u32 {
            bf.insert(i);
        }

        // Test for false positives with elements that were never inserted
        let mut false_positives = 0;
        for i in (num_elements as u32 * 2)..(num_elements as u32 * 2 + num_test_elements as u32) {
            if bf.might_contain(i) {
                false_positives += 1;
            }
        }

        let actual_fpr = false_positives as f64 / num_test_elements as f64;

        // Allow 3x tolerance (FPR should be <= 3% for 1% target)
        // This accounts for statistical variance while still catching broken hash functions
        assert!(
            actual_fpr <= expected_fpr * 3.0,
            "False positive rate too high: {:.2}% (expected <= {:.2}%)",
            actual_fpr * 100.0,
            expected_fpr * 3.0 * 100.0
        );

        // Also ensure we're not way under (which would indicate broken insertions)
        // A working bloom filter should have *some* false positives
        assert!(
            actual_fpr >= expected_fpr * 0.1,
            "False positive rate suspiciously low: {:.4}% (may indicate broken hash function)",
            actual_fpr * 100.0
        );
    }

    #[test]
    fn test_hash_pair_independence() {
        // Verify that h1 and h2 are independent (different values)
        let bf = BloomFilter::new(1000, 0.01);

        let mut same_count = 0;
        for i in 0..1000u32 {
            let (h1, h2) = bf.hash_pair(i);
            if h1 == h2 {
                same_count += 1;
            }
        }

        // With properly independent hash functions, it's astronomically unlikely
        // for h1 and h2 to be the same (1 in 2^64 chance per item)
        assert!(
            same_count == 0,
            "Hash values h1 and h2 are not independent: {} collisions out of 1000",
            same_count
        );
    }
}
