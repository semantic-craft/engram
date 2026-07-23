//! Rule-based curator report for conservative wiki maintenance signals.

use std::collections::HashMap;

use engram_core::{ProjectId, Tier, WorkspaceId};
use engram_store::{DecayParams, ReaderPool, retention_score};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

const US_PER_DAY: f64 = 86_400_000_000.0;

/// Parameters for one curator report run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorParams {
    /// Maximum findings returned per signal class.
    pub max_findings_per_kind: usize,
    /// Age threshold for `_slots/current-focus.md`.
    pub current_focus_stale_days: f64,
    /// Age threshold for other `_slots/*` pages.
    pub other_slot_stale_days: f64,
    /// Retention parameters for cold episodic detection.
    pub decay_params: DecayParams,
}

impl Default for CuratorParams {
    fn default() -> Self {
        Self {
            max_findings_per_kind: 25,
            current_focus_stale_days: 7.0,
            other_slot_stale_days: 30.0,
            decay_params: DecayParams::default(),
        }
    }
}

/// One conservative maintenance signal found by the curator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorFinding {
    /// Signal kind (`cold_episodic`, `stale_slot`, `duplicate_title`, `dangling_cross_project_link`).
    pub kind: String,
    /// Human severity for UI/CLI rendering.
    pub severity: String,
    /// Human-readable finding text.
    pub message: String,
    /// Pages involved in the finding.
    pub pages: Vec<String>,
    /// Optional structured details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Structured curator report. It is report-only: approving a staged report
/// writes this report page and performs no maintenance actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorReport {
    /// Workspace reviewed.
    pub workspace: String,
    /// Project reviewed.
    pub project: String,
    /// ISO timestamp of report generation.
    pub generated_at: String,
    /// True when returned from the dry-run path.
    pub dry_run: bool,
    /// Short summary.
    pub summary: String,
    /// Parameters used for this run.
    pub params: CuratorParams,
    /// Conservative findings.
    pub findings: Vec<CuratorFinding>,
}

/// Build a rule-based curator report. This function only reads store state.
///
/// # Errors
/// Propagates store read failures.
pub async fn run_curator_report(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    workspace_name: &str,
    project_name: &str,
    params: CuratorParams,
) -> engram_store::StoreResult<CuratorReport> {
    let now = Timestamp::now();
    let now_us = now.as_microsecond();
    let mut findings = Vec::new();

    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let mut cold = Vec::new();
    for c in &candidates {
        if c.tier != Tier::Episodic || c.pinned || frontmatter_pinned(&c.frontmatter_json) {
            continue;
        }
        let page_age_days = age_days(now_us, c.updated_at_us);
        let days_since_access = c.last_accessed_at_us.map(|us| age_days(now_us, us));
        let score = retention_score(
            &params.decay_params,
            page_age_days,
            c.access_count,
            days_since_access,
        );
        if score < params.decay_params.cold_threshold {
            cold.push(CuratorFinding {
                kind: "cold_episodic".into(),
                severity: "info".into(),
                message: format!(
                    "Episodic page {} is cold (score {:.3}, age {:.0} days)",
                    c.path.as_str(),
                    score,
                    page_age_days
                ),
                pages: vec![c.path.as_str().to_string()],
                detail: Some(serde_json::json!({
                    "score": score,
                    "threshold": params.decay_params.cold_threshold,
                    "age_days": page_age_days,
                    "access_count": c.access_count,
                })),
            });
        }
    }
    cold.sort_by(|a, b| a.message.cmp(&b.message));
    findings.extend(cold.into_iter().take(params.max_findings_per_kind));

    let pages = reader.list_pages(workspace_name, project_name).await?;
    let mut stale_slots = Vec::new();
    let mut titles: HashMap<String, Vec<String>> = HashMap::new();
    for page in &pages {
        if page.path.starts_with("_pending/") {
            continue;
        }
        if page.path.starts_with("_slots/")
            && let Ok(updated) = page.updated_at.parse::<Timestamp>()
        {
            let slot_age_days = age_days(now_us, updated.as_microsecond());
            let threshold = if page.path == "_slots/current-focus.md" {
                params.current_focus_stale_days
            } else {
                params.other_slot_stale_days
            };
            if slot_age_days > threshold {
                stale_slots.push(CuratorFinding {
                    kind: "stale_slot".into(),
                    severity: "warning".into(),
                    message: format!(
                        "Slot {} has not changed for {:.0} days (threshold {:.0})",
                        page.path, slot_age_days, threshold
                    ),
                    pages: vec![page.path.clone()],
                    detail: Some(serde_json::json!({
                        "age_days": slot_age_days,
                        "threshold_days": threshold,
                    })),
                });
            }
        }
        let title = normalize_title(&page.title);
        if !title.is_empty() {
            titles.entry(title).or_default().push(page.path.clone());
        }
    }
    stale_slots.sort_by(|a, b| a.pages.cmp(&b.pages));
    findings.extend(stale_slots.into_iter().take(params.max_findings_per_kind));

    let mut duplicate_titles = Vec::new();
    for (title, mut paths) in titles {
        if paths.len() > 1 {
            paths.sort();
            duplicate_titles.push(CuratorFinding {
                kind: "duplicate_title".into(),
                severity: "info".into(),
                message: format!("{} pages share the normalized title '{title}'", paths.len()),
                pages: paths,
                detail: Some(serde_json::json!({"normalized_title": title})),
            });
        }
    }
    duplicate_titles.sort_by(|a, b| a.message.cmp(&b.message));
    findings.extend(
        duplicate_titles
            .into_iter()
            .take(params.max_findings_per_kind),
    );

    let dangling = reader
        .dangling_cross_project_links(workspace_id, project_id)
        .await?;
    findings.extend(
        dangling
            .into_iter()
            .take(params.max_findings_per_kind)
            .map(|link| {
                let target = format!(
                    "{}/{}/{}",
                    link.workspace.as_deref().unwrap_or(workspace_name),
                    link.project,
                    link.path
                );
                CuratorFinding {
                    kind: "dangling_cross_project_link".into(),
                    severity: "warning".into(),
                    message: format!(
                        "{} links to missing cross-project target {target}",
                        link.from_path
                    ),
                    pages: vec![link.from_path],
                    detail: Some(serde_json::json!({
                        "target": target,
                        "project_exists": link.project_exists,
                    })),
                }
            }),
    );

    let summary = if findings.is_empty() {
        "No conservative curator findings.".to_string()
    } else {
        format!("{} conservative curator finding(s).", findings.len())
    };

    Ok(CuratorReport {
        workspace: workspace_name.to_string(),
        project: project_name.to_string(),
        generated_at: now.to_string(),
        dry_run: true,
        summary,
        params,
        findings,
    })
}

/// Render the report as a normal wiki markdown page.
#[must_use]
pub fn render_curator_report_markdown(report: &CuratorReport) -> String {
    let mut out = String::new();
    out.push_str("# Curator Report\n\n");
    out.push_str("> Report-only: approving this pending write stores this report page only. ");
    out.push_str("It does not edit, delete, merge, rewrite links, or update slots.\n\n");
    out.push_str(&format!("- Workspace: `{}`\n", report.workspace));
    out.push_str(&format!("- Project: `{}`\n", report.project));
    out.push_str(&format!("- Generated: `{}`\n", report.generated_at));
    out.push_str(&format!("- Summary: {}\n\n", report.summary));
    if report.findings.is_empty() {
        out.push_str("No conservative curator findings.\n");
        return out;
    }
    out.push_str("## Findings\n\n");
    for finding in &report.findings {
        out.push_str(&format!(
            "- **{}** ({}) — {}\n",
            finding.kind, finding.severity, finding.message
        ));
        if !finding.pages.is_empty() {
            out.push_str(&format!("  - Pages: `{}`\n", finding.pages.join("`, `")));
        }
    }
    out
}

fn frontmatter_pinned(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("pinned").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn age_days(now_us: i64, then_us: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let age = now_us.saturating_sub(then_us) as f64 / US_PER_DAY;
    age.max(0.0)
}
