//! `engram rename-project` — thin HTTP client for project rename.

use anyhow::Result;

use crate::cli::RenameProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Run the `rename-project` subcommand.
///
/// Resolves the current project name (auto-derived from the git repo root
/// when `--from` is omitted), sends the rename request to the server, then
/// prints a human-readable confirmation line.
///
/// # Errors
/// Returns an error when the server is unreachable or returns a non-2xx
/// response (e.g. 404 workspace/project not found, 422 name taken or invalid).
pub async fn run(config: &Config, args: RenameProjectArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let from = super::resolve_project_name(args.from.as_deref())?;
    let body = serde_json::json!({
        "workspace": args.workspace,
        "from": from,
        "to": args.to,
    });
    let summary: serde_json::Value = post_json(&endpoint, "/admin/rename-project", &body).await?;
    let pages = summary["pages"].as_u64().unwrap_or(0);
    println!(
        "Renamed {}/{} → {}/{} ({} pages now under the new name).",
        args.workspace, from, args.workspace, args.to, pages
    );
    Ok(())
}
