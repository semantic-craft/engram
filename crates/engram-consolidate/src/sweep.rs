//! M8 forget sweep — episodic-only retention pass.
//!
//! Walks the `is_latest = 1` pages for a project, computes the
//! retention score for each via [`engram_store::retention_score`],
//! and soft-deletes those below threshold. Semantic / procedural /
//! working tiers are skipped (M8 policy: semantic compounds, only
//! episodic decays). Pinned pages (schema flag OR `pinned: true` in
//! frontmatter) are exempt regardless of tier.
//!
//! Hard-delete pass cleans up rows soft-deleted more than
//! `hard_delete_after_days` ago that received zero subsequent access.
//! M7 supersession rows are safe: they have `supersedes IS NOT NULL`
//! and therefore never match the hard-delete predicate.

use engram_core::{PageId, ProjectId, Tier, WorkspaceId};
use engram_store::{DecayCandidate, DecayParams, ReaderPool, WriterHandle, retention_score};
use jiff::Timestamp;
use serde::Serialize;
use thiserror::Error;

/// One evicted page surfaced in the [`SweepReport`].
#[derive(Debug, Clone, Serialize)]
pub struct EvictedPage {
    /// Identifier of the soft-deleted page.
    pub id: PageId,
    /// Relative wiki path.
    pub path: String,
    /// Retention score at the time of the sweep.
    pub retention: f64,
    /// Days since the page's last update.
    pub age_days: f64,
    /// Total access count.
    pub access_count: u32,
}

/// Outcome of one sweep run.
#[derive(Debug, Clone, Serialize)]
pub struct SweepReport {
    /// `true` if `dry_run` was set and no rows were actually mutated.
    pub dry_run: bool,
    /// Total candidates evaluated (all tiers, before filtering).
    pub candidates_evaluated: usize,
    /// Pages that fell below the cold threshold (soft-deleted unless
    /// `dry_run`).
    pub evicted: Vec<EvictedPage>,
    /// Number of older soft-deleted rows hard-deleted on this pass.
    pub hard_deleted: usize,
}

/// Errors raised by the sweep.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SweepError {
    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] engram_store::StoreError),
}

const US_PER_DAY: f64 = 86_400_000_000.0;

/// Run a sweep against the given workspace/project.
///
/// # Errors
/// Propagates any store error encountered while reading candidates or
/// writing soft-deletions.
pub async fn run_sweep(
    reader: &ReaderPool,
    writer: &WriterHandle,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    params: &DecayParams,
    dry_run: bool,
) -> Result<SweepReport, SweepError> {
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let now_us = Timestamp::now().as_microsecond();

    let mut evicted = Vec::new();
    let mut to_evict_ids: Vec<PageId> = Vec::new();

    for c in &candidates {
        if !is_decayable(c) {
            continue;
        }
        let age_days = elapsed_days(now_us, c.updated_at_us);
        let days_since_access = c.last_accessed_at_us.map(|us| elapsed_days(now_us, us));
        let score = retention_score(params, age_days, c.access_count, days_since_access);
        if score < params.cold_threshold {
            evicted.push(EvictedPage {
                id: c.id,
                path: c.path.as_str().to_string(),
                retention: score,
                age_days,
                access_count: c.access_count,
            });
            to_evict_ids.push(c.id);
        }
    }

    let mut hard_deleted = 0usize;
    if !dry_run {
        if !to_evict_ids.is_empty() {
            writer.soft_delete_for_decay(to_evict_ids).await?;
        }
        hard_deleted = writer
            .hard_delete_decayed(params.hard_delete_after_days)
            .await?;
    }

    Ok(SweepReport {
        dry_run,
        candidates_evaluated: candidates.len(),
        evicted,
        hard_deleted,
    })
}

fn elapsed_days(now_us: i64, then_us: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let raw = (now_us - then_us) as f64 / US_PER_DAY;
    raw.max(0.0)
}

fn is_decayable(c: &DecayCandidate) -> bool {
    if c.tier != Tier::Episodic {
        return false;
    }
    if c.pinned {
        return false;
    }
    if let Ok(fm) = serde_json::from_str::<serde_json::Value>(&c.frontmatter_json)
        && fm.get("pinned").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_pages_skip_decay() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: engram_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
        };
        assert!(!is_decayable(&c));
    }

    #[test]
    fn pinned_pages_skip_decay() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: engram_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Episodic,
            pinned: true,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
        };
        assert!(!is_decayable(&c));
    }

    #[test]
    fn frontmatter_pinned_overrides() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: engram_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"pinned": true}"#.into(),
        };
        assert!(!is_decayable(&c));
    }

    #[test]
    fn fresh_episodic_page_is_decayable() {
        let c = DecayCandidate {
            id: PageId::new(),
            path: engram_core::PagePath::new("x.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: 0,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
        };
        assert!(is_decayable(&c));
    }

    #[test]
    fn elapsed_days_clamps_future_timestamps() {
        assert_eq!(elapsed_days(1_000, 2_000), 0.0);
        assert!(elapsed_days(US_PER_DAY as i64, 0) >= 1.0);
    }
}
