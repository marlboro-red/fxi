//! Scoring module for search results
//!
//! Implements configurable scoring based on spec 10.4:
//! - match count
//! - filename match
//! - directory depth
//! - recency

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
}
