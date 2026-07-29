//! `engram pending-writes` — review staged auto-improvement proposals.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{
    PendingWriteEditArgs, PendingWriteIdArgs, PendingWriteRejectArgs, PendingWritesArgs,
    PendingWritesCommand, PendingWritesListArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json_with_query};

#[derive(Debug, Deserialize, Serialize)]
struct ProposalSummary {
    id: String,
    status: String,
    target_kind: String,
    operation: String,
    target_path: String,
    logical_target: String,
    target_context_layer: String,
    kind: String,
    title: String,
    confidence: f64,
    proposing_actor: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProjectInstructionApplication {
    approval_sha256: String,
    before_sha256: String,
    after_sha256: String,
    outcome: String,
    backup_path: Option<String>,
    proposing_actor: serde_json::Value,
    approving_actor: serde_json::Value,
    applying_actor: serde_json::Value,
    applied_by_author_id: Option<String>,
    applied_at: i64,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProposalDetail {
    summary: ProposalSummary,
    rationale: String,
    body_markdown: String,
    proposed_content: String,
    base_sha256: Option<String>,
    boundary_kind: Option<String>,
    boundary_value: Option<String>,
    unified_diff: Option<String>,
    estimated_token_delta: Option<i64>,
    provenance: serde_json::Value,
    approval_sha256: Option<String>,
    review_revision: Option<i64>,
    revisions: Vec<serde_json::Value>,
    events: Vec<serde_json::Value>,
    #[serde(default)]
    application: Option<ProjectInstructionApplication>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DiffResponse {
    diff: String,
}

pub async fn run(config: &Config, args: PendingWritesArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        PendingWritesCommand::List(args) => list(&ep, args).await,
        PendingWritesCommand::Show(args) => show(&ep, args).await,
        PendingWritesCommand::Diff(args) => diff(&ep, args).await,
        PendingWritesCommand::Edit(args) => edit(&ep, args).await,
        PendingWritesCommand::Approve(args) => approve(&ep, args).await,
        PendingWritesCommand::Reject(args) => reject(&ep, args).await,
    }
}

async fn list(ep: &ServerEndpoint, args: PendingWritesListArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let limit = args.limit.to_string();
    let mut query = vec![
        ("workspace", args.workspace.as_str()),
        ("project", project.as_str()),
        ("limit", limit.as_str()),
    ];
    if let Some(status) = args.status.as_deref() {
        query.push(("status", status));
    }
    let proposals: Vec<ProposalSummary> = get_json(ep, "/admin/pending-writes", &query).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&proposals)?);
    } else if proposals.is_empty() {
        println!("(no pending writes)");
    } else {
        for p in proposals {
            println!(
                "{}  {}  {}  {}  {}",
                p.id, p.status, p.target_kind, p.operation, p.logical_target
            );
        }
    }
    Ok(())
}

async fn show(ep: &ServerEndpoint, args: PendingWriteIdArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let detail: ProposalDetail = get_json(
        ep,
        &format!("/admin/pending-writes/{}", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        println!(
            "{} [{}] {}",
            detail.summary.logical_target, detail.summary.status, detail.summary.title
        );
        println!(
            "target: {} ({})\noperation: {}\n\n{}\n\n--- proposed content ---\n{}",
            detail.summary.target_kind,
            detail.summary.target_context_layer,
            detail.summary.operation,
            detail.rationale,
            detail.proposed_content
        );
    }
    Ok(())
}

async fn diff(ep: &ServerEndpoint, args: PendingWriteIdArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let resp: DiffResponse = get_json(
        ep,
        &format!("/admin/pending-writes/{}/diff", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        print!("{}", resp.diff);
    }
    Ok(())
}

async fn edit(ep: &ServerEndpoint, args: PendingWriteEditArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let detail: ProposalDetail = get_json(
        ep,
        &format!("/admin/pending-writes/{}", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    let approval_sha256 = detail
        .approval_sha256
        .ok_or_else(|| anyhow::anyhow!("proposal does not support editable instruction review"))?;
    let resp: serde_json::Value = post_json_with_query(
        ep,
        &format!("/admin/pending-writes/{}/edit", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
        &serde_json::json!({
            "proposed_content": args.content,
            "expected_approval_sha256": approval_sha256,
        }),
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("✓ edited {}", args.id);
    }
    Ok(())
}

async fn approve(ep: &ServerEndpoint, args: PendingWriteIdArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let detail: ProposalDetail = get_json(
        ep,
        &format!("/admin/pending-writes/{}", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await?;
    let body = if detail.summary.target_kind == "project_instruction" {
        let approval_sha256 = detail.approval_sha256.ok_or_else(|| {
            anyhow::anyhow!("project-instruction proposal has no approval binding")
        })?;
        serde_json::json!({ "expected_approval_sha256": approval_sha256 })
    } else {
        serde_json::json!({})
    };
    let resp: serde_json::Value = post_json_with_query(
        ep,
        &format!("/admin/pending-writes/{}/approve", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
        &body,
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else if detail.summary.target_kind == "project_instruction" {
        println!("✓ approved {} (apply-ready; target unchanged)", args.id);
    } else {
        println!("✓ approved {}", args.id);
    }
    Ok(())
}

async fn reject(ep: &ServerEndpoint, args: PendingWriteRejectArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;
    let resp: serde_json::Value = post_json_with_query(
        ep,
        &format!("/admin/pending-writes/{}/reject", args.id),
        &[
            ("workspace", args.workspace.as_str()),
            ("project", project.as_str()),
        ],
        &serde_json::json!({ "reason": args.reason }),
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("✓ rejected {}", args.id);
    }
    Ok(())
}
