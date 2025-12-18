//! Relay quality scoring computation.
//!
//! This module computes quality scores for relays based on:
//! - Novel event rate (events/hour that passed dedupe)
//! - Uptime (connection success rate)

use super::schema::RelayTier;

/// Weight for novel event rate in the composite score (0.0 - 1.0).
const NOVEL_RATE_WEIGHT: f64 = 0.7;

/// Weight for uptime in the composite score (0.0 - 1.0).
const UPTIME_WEIGHT: f64 = 0.3;

/// Cap for normalized novel rate (prevents outliers from dominating).
const NOVEL_RATE_CAP: f64 = 2.0;

/// Minimum score floor for seed relays (prevents eviction).
const SEED_SCORE_FLOOR: f64 = 0.5;

/// Stats used for score computation.
#[derive(Debug, Clone, Default)]
pub struct RelayStatsForScoring {
    /// Novel events per hour (7-day average).
    pub novel_rate_7d: f64,
    /// Novel events per hour (24-hour average).
    pub novel_rate_24h: f64,
    /// Novel events per hour (last hour).
    pub novel_rate_1h: f64,
    /// Connection success rate over 7 days (0.0 - 1.0).
    pub uptime_7d: f64,
    /// Connection success rate over 24 hours (0.0 - 1.0).
    pub uptime_24h: f64,
    /// Relay tier (seed vs discovered).
    pub tier: RelayTier,
}

/// Computed score result.
#[derive(Debug, Clone)]
pub struct RelayScore {
    /// Composite quality score.
    pub score: f64,
    /// Novel events per hour (1h).
    pub novel_rate_1h: f64,
    /// Novel events per hour (24h avg).
    pub novel_rate_24h: f64,
    /// Novel events per hour (7d avg).
    pub novel_rate_7d: f64,
    /// Connection success rate (24h).
    pub uptime_24h: f64,
    /// Connection success rate (7d).
    pub uptime_7d: f64,
}

/// Compute quality score for a single relay.
///
/// # Arguments
///
/// * `stats` - The relay's stats for scoring
/// * `network_median_novel_rate` - Median novel rate across all active relays (for normalization)
///
/// # Returns
///
/// A composite score where higher is better. Seed relays have a floor of 0.5.
pub fn compute_score(stats: &RelayStatsForScoring, network_median_novel_rate: f64) -> RelayScore {
    // Avoid division by zero
    let median = if network_median_novel_rate > 0.0 {
        network_median_novel_rate
    } else {
        1.0
    };

    // Normalize novel rate relative to network median, capped at 2x
    let novel_normalized = (stats.novel_rate_7d / median).min(NOVEL_RATE_CAP);

    // Uptime is already 0.0-1.0
    let uptime = stats.uptime_7d.clamp(0.0, 1.0);

    // Weighted combination
    let raw_score = (novel_normalized * NOVEL_RATE_WEIGHT) + (uptime * UPTIME_WEIGHT);

    // Apply seed floor
    let score = match stats.tier {
        RelayTier::Seed => raw_score.max(SEED_SCORE_FLOOR),
        RelayTier::Discovered => raw_score,
    };

    RelayScore {
        score,
        novel_rate_1h: stats.novel_rate_1h,
        novel_rate_24h: stats.novel_rate_24h,
        novel_rate_7d: stats.novel_rate_7d,
        uptime_24h: stats.uptime_24h,
        uptime_7d: stats.uptime_7d,
    }
}

/// Compute the median of a slice of values.
pub fn compute_median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_score_basic() {
        let stats = RelayStatsForScoring {
            novel_rate_7d: 100.0,
            novel_rate_24h: 120.0,
            novel_rate_1h: 150.0,
            uptime_7d: 0.95,
            uptime_24h: 1.0,
            tier: RelayTier::Discovered,
        };

        let score = compute_score(&stats, 100.0); // At median

        // Expected: (1.0 * 0.7) + (0.95 * 0.3) = 0.7 + 0.285 = 0.985
        assert!((score.score - 0.985).abs() < 0.001);
    }

    #[test]
    fn test_compute_score_above_median() {
        let stats = RelayStatsForScoring {
            novel_rate_7d: 300.0, // 3x median, but capped at 2x
            uptime_7d: 1.0,
            tier: RelayTier::Discovered,
            ..Default::default()
        };

        let score = compute_score(&stats, 100.0);

        // Expected: (2.0 * 0.7) + (1.0 * 0.3) = 1.4 + 0.3 = 1.7
        assert!((score.score - 1.7).abs() < 0.001);
    }

    #[test]
    fn test_compute_score_seed_floor() {
        let stats = RelayStatsForScoring {
            novel_rate_7d: 0.0, // No events
            uptime_7d: 0.0,     // Never connected
            tier: RelayTier::Seed,
            ..Default::default()
        };

        let score = compute_score(&stats, 100.0);

        // Seed floor should apply
        assert!((score.score - SEED_SCORE_FLOOR).abs() < 0.001);
    }

    #[test]
    fn test_compute_score_discovered_no_floor() {
        let stats = RelayStatsForScoring {
            novel_rate_7d: 0.0,
            uptime_7d: 0.0,
            tier: RelayTier::Discovered,
            ..Default::default()
        };

        let score = compute_score(&stats, 100.0);

        // No floor for discovered relays
        assert!((score.score - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_compute_median() {
        assert!((compute_median(&mut [1.0, 2.0, 3.0]) - 2.0).abs() < 0.001);
        assert!((compute_median(&mut [1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < 0.001);
        assert!((compute_median(&mut []) - 0.0).abs() < 0.001);
        assert!((compute_median(&mut [5.0]) - 5.0).abs() < 0.001);
    }
}
