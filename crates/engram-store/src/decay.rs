//! Retention-formula math + tunable parameters.
//!
//! Adapted from agentmemory's `salience · exp(−λΔt) + Σ(σ/days_since_access)`.
//! We simplify the Σ term to `σ · log(1 + access_count) · exp(−μ · days_since_access)`
//! so we don't have to materialise a full access-history table on the
//! hot read path -- the `access_count` + `last_accessed_at` columns on
//! `pages` (V03 migration) are enough.
//!
//! The formula is pure: pass in everything you need, get a score out.
//! The forget-sweep job computes it from store rows; the property tests
//! pin the math without touching the database.

use serde::{Deserialize, Serialize};

/// Tunable retention coefficients.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DecayParams {
    /// Per-day exponential decay rate applied to "age since updated_at".
    /// `0.02` ≈ 35-day half-life.
    pub lambda: f64,
    /// Magnitude of the access-reinforcement boost.
    pub sigma: f64,
    /// Per-day exponential decay applied to "days since last access" --
    /// so a recent hit boosts more than an old one.
    pub mu: f64,
    /// Default salience used when a page doesn't have an explicit one.
    pub salience_default: f64,
    /// Below this score, an episodic page is a soft-delete candidate.
    pub cold_threshold: f64,
    /// Days a soft-deleted (sweep-evicted) page must survive before
    /// hard-delete, *with* zero subsequent access.
    pub hard_delete_after_days: i64,
}

impl Default for DecayParams {
    fn default() -> Self {
        Self {
            lambda: 0.02,
            sigma: 0.6,
            mu: 0.04,
            salience_default: 1.0,
            cold_threshold: 0.20,
            hard_delete_after_days: 180,
        }
    }
}

/// Compute the retention score. Higher = "keep this page".
///
/// * `age_days` — days since the page's `updated_at`.
/// * `access_count` — total number of search hits.
/// * `days_since_access` — `Some(N)` if the page has ever been
///   accessed; `None` if it never has.
#[must_use]
pub fn retention_score(
    params: &DecayParams,
    age_days: f64,
    access_count: u32,
    days_since_access: Option<f64>,
) -> f64 {
    let time_term = params.salience_default * (-params.lambda * age_days).exp();
    let access_term = days_since_access.map_or(0.0, |d| {
        params.sigma * (1.0 + f64::from(access_count)).ln() * (-params.mu * d).exp()
    });
    time_term + access_term
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_unused_page_starts_near_salience() {
        let p = DecayParams::default();
        let score = retention_score(&p, 0.0, 0, None);
        assert!((score - p.salience_default).abs() < 1e-9);
    }

    #[test]
    fn ancient_page_with_no_access_decays_below_threshold() {
        let p = DecayParams::default();
        let score = retention_score(&p, 365.0, 0, None);
        assert!(score < p.cold_threshold, "got {score}");
    }

    #[test]
    fn frequently_accessed_page_stays_above_threshold_even_old() {
        let p = DecayParams::default();
        let aged_unused = retention_score(&p, 200.0, 0, None);
        let aged_hot = retention_score(&p, 200.0, 50, Some(2.0));
        assert!(aged_unused < p.cold_threshold);
        assert!(
            aged_hot > p.cold_threshold,
            "hot page should survive: {aged_hot} (cold {aged_unused})",
        );
    }

    #[test]
    fn recent_access_boosts_more_than_old_access() {
        let p = DecayParams::default();
        let recent = retention_score(&p, 100.0, 10, Some(2.0));
        let stale = retention_score(&p, 100.0, 10, Some(120.0));
        assert!(recent > stale, "recent {recent} vs stale {stale}");
    }

    #[test]
    fn score_decreases_as_age_increases_without_access() {
        let p = DecayParams::default();
        let young = retention_score(&p, 10.0, 0, None);
        let old = retention_score(&p, 20.0, 0, None);
        assert!(young > old, "young {young} vs old {old}");
    }

    #[test]
    fn score_increases_with_access_count_when_access_age_matches() {
        let p = DecayParams::default();
        let low = retention_score(&p, 100.0, 1, Some(5.0));
        let high = retention_score(&p, 100.0, 20, Some(5.0));
        assert!(high > low, "high {high} vs low {low}");
    }
}
