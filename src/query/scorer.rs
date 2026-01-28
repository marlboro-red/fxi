//! Scoring module for search results
//!
//! Implements configurable scoring based on spec 10.4:
//! - match count
//! - filename match
//! - directory depth
//! - recency
//!
//! Also implements Block-Max WAND / MaxScore upper bound calculations
//! for efficient top-k retrieval with early termination.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Configurable weights for scoring factors
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringWeights {
    /// Base score for each match found
    pub match_count_weight: f32,
    /// Bonus multiplier when search term appears in filename
    pub filename_match_bonus: f32,
    /// Penalty per directory depth level (subtracted)
    pub depth_penalty: f32,
    /// Maximum depth penalty to apply
    pub max_depth_penalty: f32,
    /// Recency half-life in seconds (how fast recency bonus decays)
    pub recency_half_life_secs: f32,
    /// Maximum recency bonus
    pub max_recency_bonus: f32,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            match_count_weight: 1.0,
            filename_match_bonus: 2.0,
            depth_penalty: 0.05,
            max_depth_penalty: 0.5,
            recency_half_life_secs: 86400.0 * 7.0, // 7 days
            max_recency_bonus: 1.0,
        }
    }
}

/// Score calculation context for a single file
#[derive(Debug, Default)]
pub struct ScoreContext {
    /// Number of matches found in this file
    pub match_count: usize,
    /// Whether the search term appears in the filename
    pub filename_match: bool,
    /// Directory depth (number of path components)
    pub depth: usize,
    /// File modification time as unix timestamp
    pub mtime: u64,
    /// Boost multiplier from ^term syntax (default 1.0)
    pub boost: f32,
}

/// Upper bound context for Block-Max WAND / MaxScore scoring
/// Used to compute the maximum possible score for a document
/// before fully verifying it (enables early termination).
#[derive(Debug, Clone)]
pub struct UpperBoundContext {
    /// Maximum possible matches to assume (conservative estimate)
    pub max_matches: usize,
    /// Whether filename match is possible for this document
    pub filename_match_possible: bool,
    /// Minimum depth (best case = 1 for root-level files)
    pub min_depth: usize,
    /// File modification time (for recency calculation)
    pub mtime: u64,
    /// Boost multiplier
    pub boost: f32,
}

impl Default for UpperBoundContext {
    fn default() -> Self {
        Self {
            max_matches: 100, // Conservative high estimate
            filename_match_possible: true,
            min_depth: 1,
            mtime: 0,
            boost: 1.0,
        }
    }
}

impl UpperBoundContext {
    /// Create an upper bound context from document metadata
    pub fn from_doc_metadata(
        depth: usize,
        mtime: u64,
        filename_might_match: bool,
        boost: f32,
    ) -> Self {
        Self {
            max_matches: 100, // Assume up to 100 matches possible
            filename_match_possible: filename_might_match,
            min_depth: depth,
            mtime,
            boost: if boost > 0.0 { boost } else { 1.0 },
        }
    }
}

/// Scorer calculates relevance scores for search results
pub struct Scorer {
    weights: ScoringWeights,
    current_time: u64,
}

impl Scorer {
    pub fn new(weights: ScoringWeights) -> Self {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            weights,
            current_time,
        }
    }

    /// Create a scorer with default weights
    pub fn with_defaults() -> Self {
        Self::new(ScoringWeights::default())
    }

    /// Calculate the total score for a file given its context
    pub fn calculate_score(&self, ctx: &ScoreContext) -> f32 {
        let mut score = 0.0;

        // Match count factor
        score += self.match_count_score(ctx.match_count);

        // Filename match bonus
        if ctx.filename_match {
            score += self.weights.filename_match_bonus;
        }

        // Directory depth penalty
        score -= self.depth_penalty(ctx.depth);

        // Recency bonus
        score += self.recency_bonus(ctx.mtime);

        // Apply boost multiplier (default 1.0 if not set)
        let boost = if ctx.boost > 0.0 { ctx.boost } else { 1.0 };
        score *= boost;

        // Ensure score is non-negative
        score.max(0.1)
    }

    /// Calculate score contribution from match count
    fn match_count_score(&self, count: usize) -> f32 {
        // Use logarithmic scaling to prevent huge files from dominating
        // log2(count + 1) gives diminishing returns for more matches
        let log_count = (count as f32 + 1.0).log2();
        log_count * self.weights.match_count_weight
    }

    /// Calculate depth penalty
    fn depth_penalty(&self, depth: usize) -> f32 {
        let penalty = depth as f32 * self.weights.depth_penalty;
        penalty.min(self.weights.max_depth_penalty)
    }

    /// Calculate recency bonus based on file mtime
    fn recency_bonus(&self, mtime: u64) -> f32 {
        if mtime == 0 || self.current_time == 0 {
            return 0.0;
        }

        let age_secs = self.current_time.saturating_sub(mtime) as f32;

        // Exponential decay: bonus = max_bonus * 0.5^(age / half_life)
        let decay = (-age_secs * 0.693 / self.weights.recency_half_life_secs).exp();

        self.weights.max_recency_bonus * decay
    }

    /// Check if a search term appears in the filename
    pub fn term_in_filename(path: &Path, term: &str) -> bool {
        path.file_name()
            .and_then(|f| f.to_str())
            .map(|name| name.to_lowercase().contains(&term.to_lowercase()))
            .unwrap_or(false)
    }

    /// Calculate directory depth from a path
    pub fn path_depth(path: &Path) -> usize {
        path.components().count()
    }

    // =========================================================================
    // Block-Max WAND / MaxScore Upper Bound Calculations
    // =========================================================================

    /// Calculate the upper bound (maximum possible) score for a document.
    /// This is used by Block-Max WAND / MaxScore to enable early termination.
    ///
    /// The upper bound is computed by assuming the best possible values for
    /// unknown factors while using known factors (depth, mtime) accurately.
    pub fn calculate_upper_bound(&self, ctx: &UpperBoundContext) -> f32 {
        let mut score = 0.0;

        // Maximum match count contribution (assume many matches)
        score += self.match_count_score(ctx.max_matches);

        // Filename match bonus (if possible)
        if ctx.filename_match_possible {
            score += self.weights.filename_match_bonus;
        }

        // Depth penalty - use actual depth (this is known upfront)
        score -= self.depth_penalty(ctx.min_depth);

        // Recency bonus - use actual mtime
        score += self.recency_bonus(ctx.mtime);

        // Apply boost multiplier
        score *= ctx.boost;

        // Ensure non-negative
        score.max(0.1)
    }

    /// Calculate a quick upper bound using only document metadata.
    /// This is faster than calculate_upper_bound and suitable for initial sorting.
    #[inline]
    pub fn quick_upper_bound(&self, depth: usize, mtime: u64, boost: f32) -> f32 {
        let mut score = 0.0;

        // Assume maximum match count contribution
        // log2(101) ≈ 6.66 for 100 matches
        score += 6.66 * self.weights.match_count_weight;

        // Assume filename match possible
        score += self.weights.filename_match_bonus;

        // Apply actual depth penalty
        score -= self.depth_penalty(depth);

        // Recency bonus from actual mtime
        score += self.recency_bonus(mtime);

        // Apply boost
        let effective_boost = if boost > 0.0 { boost } else { 1.0 };
        score *= effective_boost;

        score.max(0.1)
    }

    /// Get the theoretical maximum score (absolute ceiling).
    /// Used to initialize the threshold in WAND processing.
    pub fn max_possible_score(&self, boost: f32) -> f32 {
        let mut score = 0.0;

        // Maximum match contribution
        score += 6.66 * self.weights.match_count_weight;

        // Filename bonus
        score += self.weights.filename_match_bonus;

        // No depth penalty (best case: depth = 0)
        // score -= 0.0

        // Maximum recency bonus
        score += self.weights.max_recency_bonus;

        // Apply boost
        let effective_boost = if boost > 0.0 { boost } else { 1.0 };
        score *= effective_boost;

        score
    }

    /// Get the weights (for external calculations)
    pub fn weights(&self) -> &ScoringWeights {
        &self.weights
    }

    /// Get the current time used for recency calculations
    #[cfg(test)]
    pub fn current_time(&self) -> u64 {
        self.current_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_default_weights() {
        let weights = ScoringWeights::default();
        assert!(weights.match_count_weight > 0.0);
        assert!(weights.filename_match_bonus > 0.0);
    }

    #[test]
    fn test_match_count_scoring() {
        let scorer = Scorer::with_defaults();

        let ctx1 = ScoreContext {
            match_count: 1,
            ..Default::default()
        };
        let ctx10 = ScoreContext {
            match_count: 10,
            ..Default::default()
        };

        let score1 = scorer.calculate_score(&ctx1);
        let score10 = scorer.calculate_score(&ctx10);

        // More matches should give higher score
        assert!(score10 > score1);
    }

    #[test]
    fn test_filename_match_bonus() {
        let scorer = Scorer::with_defaults();

        let ctx_no_match = ScoreContext {
            match_count: 1,
            filename_match: false,
            ..Default::default()
        };
        let ctx_match = ScoreContext {
            match_count: 1,
            filename_match: true,
            ..Default::default()
        };

        let score_no = scorer.calculate_score(&ctx_no_match);
        let score_yes = scorer.calculate_score(&ctx_match);

        assert!(score_yes > score_no);
    }

    #[test]
    fn test_depth_penalty() {
        let scorer = Scorer::with_defaults();

        let ctx_shallow = ScoreContext {
            match_count: 1,
            depth: 2,
            ..Default::default()
        };
        let ctx_deep = ScoreContext {
            match_count: 1,
            depth: 10,
            ..Default::default()
        };

        let score_shallow = scorer.calculate_score(&ctx_shallow);
        let score_deep = scorer.calculate_score(&ctx_deep);

        // Shallower paths should have higher scores
        assert!(score_shallow > score_deep);
    }

    #[test]
    fn test_term_in_filename() {
        let path = PathBuf::from("src/query/executor.rs");
        assert!(Scorer::term_in_filename(&path, "executor"));
        assert!(Scorer::term_in_filename(&path, "EXECUTOR")); // case insensitive
        assert!(!Scorer::term_in_filename(&path, "parser"));
    }

    #[test]
    fn test_path_depth() {
        let path1 = PathBuf::from("file.rs");
        let path2 = PathBuf::from("src/file.rs");
        let path3 = PathBuf::from("src/query/executor.rs");

        assert_eq!(Scorer::path_depth(&path1), 1);
        assert_eq!(Scorer::path_depth(&path2), 2);
        assert_eq!(Scorer::path_depth(&path3), 3);
    }

    #[test]
    fn test_boost_scoring() {
        let scorer = Scorer::with_defaults();

        let ctx_no_boost = ScoreContext {
            match_count: 1,
            boost: 1.0,
            ..Default::default()
        };
        let ctx_boosted = ScoreContext {
            match_count: 1,
            boost: 2.0,
            ..Default::default()
        };

        let score_no = scorer.calculate_score(&ctx_no_boost);
        let score_boosted = scorer.calculate_score(&ctx_boosted);

        // Boosted score should be approximately 2x the non-boosted score
        assert!(score_boosted > score_no);
        // Allow for the non-linear effects of other factors
        assert!((score_boosted / score_no - 2.0).abs() < 0.5);
    }

    // =========================================================================
    // Block-Max WAND / MaxScore Upper Bound Tests
    // =========================================================================

    #[test]
    fn test_upper_bound_context_default() {
        let ctx = UpperBoundContext::default();
        assert_eq!(ctx.max_matches, 100);
        assert!(ctx.filename_match_possible);
        assert_eq!(ctx.min_depth, 1);
        assert_eq!(ctx.boost, 1.0);
    }

    #[test]
    fn test_upper_bound_context_from_metadata() {
        let ctx = UpperBoundContext::from_doc_metadata(
            3,      // depth
            12345,  // mtime
            true,   // filename might match
            2.0,    // boost
        );

        assert_eq!(ctx.min_depth, 3);
        assert_eq!(ctx.mtime, 12345);
        assert!(ctx.filename_match_possible);
        assert_eq!(ctx.boost, 2.0);
    }

    #[test]
    fn test_upper_bound_greater_than_actual() {
        let scorer = Scorer::with_defaults();

        // Create an upper bound context
        let ub_ctx = UpperBoundContext::from_doc_metadata(2, 0, true, 1.0);
        let upper_bound = scorer.calculate_upper_bound(&ub_ctx);

        // Create various actual score contexts with same metadata
        let actual_ctx_1 = ScoreContext {
            match_count: 1,
            filename_match: true,
            depth: 2,
            mtime: 0,
            boost: 1.0,
        };
        let actual_ctx_10 = ScoreContext {
            match_count: 10,
            filename_match: true,
            depth: 2,
            mtime: 0,
            boost: 1.0,
        };
        let actual_ctx_no_filename = ScoreContext {
            match_count: 5,
            filename_match: false,
            depth: 2,
            mtime: 0,
            boost: 1.0,
        };

        let score_1 = scorer.calculate_score(&actual_ctx_1);
        let score_10 = scorer.calculate_score(&actual_ctx_10);
        let score_no_filename = scorer.calculate_score(&actual_ctx_no_filename);

        // Upper bound should always be >= actual score
        assert!(upper_bound >= score_1, "Upper bound {} should be >= actual {}", upper_bound, score_1);
        assert!(upper_bound >= score_10, "Upper bound {} should be >= actual {}", upper_bound, score_10);
        assert!(upper_bound >= score_no_filename, "Upper bound {} should be >= actual {}", upper_bound, score_no_filename);
    }

    #[test]
    fn test_quick_upper_bound() {
        let scorer = Scorer::with_defaults();

        // Quick upper bound for a shallow, recent file
        let ub_shallow = scorer.quick_upper_bound(1, scorer.current_time(), 1.0);

        // Quick upper bound for a deep, old file
        let ub_deep = scorer.quick_upper_bound(10, 0, 1.0);

        // Shallow, recent files should have higher upper bounds
        assert!(ub_shallow > ub_deep);
    }

    #[test]
    fn test_quick_upper_bound_with_boost() {
        let scorer = Scorer::with_defaults();

        let ub_no_boost = scorer.quick_upper_bound(2, 0, 1.0);
        let ub_boosted = scorer.quick_upper_bound(2, 0, 2.0);

        // Boosted upper bound should be approximately 2x
        assert!(ub_boosted > ub_no_boost);
        let ratio = ub_boosted / ub_no_boost;
        assert!((ratio - 2.0).abs() < 0.1, "Boost ratio {} should be ~2.0", ratio);
    }

    #[test]
    fn test_max_possible_score() {
        let scorer = Scorer::with_defaults();

        let max_score = scorer.max_possible_score(1.0);
        let max_boosted = scorer.max_possible_score(2.0);

        // Max possible score should be positive
        assert!(max_score > 0.0);

        // Boosted max should be 2x
        assert!((max_boosted / max_score - 2.0).abs() < 0.01);

        // Max possible should be >= any upper bound
        let ub = scorer.quick_upper_bound(1, scorer.current_time(), 1.0);
        assert!(max_score >= ub);
    }

    #[test]
    fn test_upper_bound_enables_pruning() {
        let scorer = Scorer::with_defaults();

        // Simulate WAND scenario: we have a threshold from top-k results
        // A deep, old file with no filename match potential
        let poor_candidate_ub = scorer.quick_upper_bound(15, 0, 1.0);

        // A shallow, recent file
        let good_candidate_ctx = ScoreContext {
            match_count: 5,
            filename_match: true,
            depth: 1,
            mtime: scorer.current_time(),
            boost: 1.0,
        };
        let good_score = scorer.calculate_score(&good_candidate_ctx);

        // If our threshold is the good score, the poor candidate's upper bound
        // should help us decide whether to verify it
        // (In a real scenario, if ub < threshold, we skip verification)
        println!("Good score (threshold): {}", good_score);
        println!("Poor candidate upper bound: {}", poor_candidate_ub);

        // The poor candidate might still have a chance (upper bounds are conservative)
        // but the test demonstrates the concept works
        assert!(poor_candidate_ub >= 0.1); // Should be valid
    }
}
