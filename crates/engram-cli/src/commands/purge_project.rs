//! `engram purge-project` — thin HTTP client for project purge.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PurgeProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/purge-project`.
#[derive(Serialize)]
struct PurgeProjectRequest {
    workspace: String,
    project: String,
    confirm: bool,
}

/// Run the `purge-project` subcommand.
///
/// Resolves the project name (auto-derived from the git repo root when
/// `--project` is omitted), requires `--confirm` before sending the
/// destructive request, then prints the JSON summary.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the server is unreachable,
/// or the server returns a non-2xx response.
pub async fn run(config: &Config, args: PurgeProjectArgs) -> Result<()> {
    let project = super::resolve_project_name(args.project.as_deref())?;

    if !args.confirm {
        bail!(
            "purge-project is destructive and irreversible.\n\
             Re-run with --confirm to proceed:\n\n  \
             engram purge-project --workspace {} --project {} --confirm",
            args.workspace,
            project,
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/purge-project",
        &PurgeProjectRequest {
            workspace: args.workspace.clone(),
            project: project.clone(),
            confirm: true,
        },
    )
    .await?;

    // Human-friendly one-liner followed by the raw JSON for scripting.
    let fallback_label = format!("{}/{}", args.workspace, project);
    let label = report["label"].as_str().unwrap_or(&fallback_label);
    let pages = report["pages_deleted"].as_u64().unwrap_or(0);
    let sessions = report["sessions_deleted"].as_u64().unwrap_or(0);
    let observations = report["observations_deleted"].as_u64().unwrap_or(0);
    let handoffs = report["handoffs_deleted"].as_u64().unwrap_or(0);
    let embeddings = report["embeddings_deleted"].as_u64().unwrap_or(0);
    println!(
        "Purged {label}: {pages} pages, {sessions} sessions, \
         {observations} observations, {handoffs} handoffs, {embeddings} embeddings."
    );
    if let Some(failed) = report["files_failed"].as_array()
        && !failed.is_empty()
    {
        println!(
            "Warning: {} wiki file(s) could not be removed from disk (DB rows are gone).",
            failed.len()
        );
    }
    Ok(())
}
