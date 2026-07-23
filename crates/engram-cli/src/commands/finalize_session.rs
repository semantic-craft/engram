//! `engram finalize-session` — manually synthesize SessionEnd for Codex.

use anyhow::{Context, Result, bail};
use engram_core::AgentKind;
use engram_store::{DB_FILENAME, OpenSession, ReaderPool};
use serde::{Deserialize, Serialize};

use crate::cli::FinalizeSessionArgs;
use crate::config::Config;
use crate::http_client::ServerEndpoint;

#[derive(Debug, Serialize)]
struct SessionEndPayload<'a> {
    session_id: String,
    cwd: &'a str,
}

#[derive(Debug, Serialize)]
struct HookBatchItem<'a> {
    url: String,
    body: SessionEndPayload<'a>,
}

#[derive(Debug, Deserialize)]
struct HookBatchAck {
    accepted: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct FinalizeSessionReport {
    workspace: String,
    project: String,
    agent: String,
    finalized: Vec<String>,
}

/// Run the `finalize-session` subcommand.
///
/// # Errors
/// Returns an error if the local store cannot be read or the configured server
/// rejects a synthetic `session-end` hook.
pub async fn run(config: &Config, args: FinalizeSessionArgs) -> Result<()> {
    let agent = args.agent.kind();
    let project = super::resolve_project_name(args.project.as_deref())?;
    let db_path = config.data_dir.join("db").join(DB_FILENAME);
    if !db_path.exists() {
        return print_report(args, project, agent, Vec::new());
    }
    let reader = ReaderPool::new(&db_path, 1).context("opening local store reader")?;
    let workspace_id = match reader.find_workspace(args.workspace.clone()).await? {
        Some(id) => id,
        None => return print_report(args, project, agent, Vec::new()),
    };
    let project_id = match reader.find_project(workspace_id, project.clone()).await? {
        Some(id) => id,
        None => return print_report(args, project, agent, Vec::new()),
    };

    let limit = if args.all { None } else { Some(1) };
    let sessions = reader
        .open_sessions_for_scope_agent(workspace_id, project_id, agent, limit)
        .await?;
    if sessions.is_empty() {
        return print_report(args, project, agent, Vec::new());
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let client = reqwest::Client::new();
    let fallback_cwd = effective_cwd()?;
    let mut finalized = Vec::with_capacity(sessions.len());
    for session in &sessions {
        post_session_end_batch(
            &client,
            &endpoint,
            session,
            fallback_cwd.as_str(),
            &args.workspace,
            &project,
            agent,
        )
        .await?;
        finalized.push(session.session_id.to_string());
    }

    print_report(args, project, agent, finalized)
}

fn print_report(
    args: FinalizeSessionArgs,
    project: String,
    agent: AgentKind,
    finalized: Vec<String>,
) -> Result<()> {
    let report = FinalizeSessionReport {
        workspace: args.workspace,
        project,
        agent: agent.as_str().to_string(),
        finalized,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.finalized.is_empty() {
        println!(
            "No open {} sessions matched {}/{}",
            report.agent, report.workspace, report.project
        );
    } else {
        println!(
            "Finalized {} {} session(s) for {}/{}",
            report.finalized.len(),
            report.agent,
            report.workspace,
            report.project
        );
        for session_id in &report.finalized {
            println!("  - {session_id}");
        }
    }
    Ok(())
}

async fn post_session_end_batch(
    client: &reqwest::Client,
    endpoint: &ServerEndpoint,
    session: &OpenSession,
    fallback_cwd: &str,
    workspace: &str,
    project: &str,
    agent: AgentKind,
) -> Result<()> {
    let cwd = session.cwd.as_deref().unwrap_or(fallback_cwd);
    let hook_url = session_end_hook_url(endpoint, cwd, workspace, project, agent)?;
    let batch_url = endpoint.build_url("/hook/batch");
    let items = [HookBatchItem {
        url: hook_url,
        body: SessionEndPayload {
            session_id: session.session_id.to_string(),
            cwd,
        },
    }];
    let request = client.post(&batch_url).json(&items);
    let request = endpoint.authenticate(request);
    let response = request
        .send()
        .await
        .with_context(|| format!("posting synthetic session-end batch to {batch_url}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("server returned {status}: {body}");
    }
    let ack: HookBatchAck = response
        .json()
        .await
        .with_context(|| format!("parsing hook batch ack from {batch_url}"))?;
    if ack.accepted != 1 {
        bail!(
            "server accepted {} of 1 synthetic session-end events",
            ack.accepted
        );
    }
    Ok(())
}

fn session_end_hook_url(
    endpoint: &ServerEndpoint,
    cwd: &str,
    workspace: &str,
    project: &str,
    agent: AgentKind,
) -> Result<String> {
    let mut url = reqwest::Url::parse(&endpoint.build_url("/hook"))
        .context("building synthetic session-end hook URL")?;
    url.query_pairs_mut()
        .append_pair("event", "session-end")
        .append_pair("agent", agent.as_str())
        .append_pair("cwd", cwd)
        .append_pair("workspace", workspace)
        .append_pair("project", project);
    Ok(url.into())
}

fn effective_cwd() -> Result<String> {
    Ok(std::env::current_dir()
        .context("getting CWD for synthetic session-end")?
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{NewSession, SessionId};
    use engram_store::Store;
    use tempfile::TempDir;

    #[tokio::test]
    async fn selects_latest_scoped_codex_session_only_by_default() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let target = store
            .writer
            .get_or_create_project(ws, "target".to_string(), None)
            .await
            .unwrap();
        let other_project = store
            .writer
            .get_or_create_project(ws, "other".to_string(), None)
            .await
            .unwrap();
        let older = SessionId::new();
        let latest = SessionId::new();
        let other_agent = SessionId::new();
        let other_scope = SessionId::new();
        for (id, project_id, agent) in [
            (older, target, AgentKind::Codex),
            (other_agent, target, AgentKind::ClaudeCode),
            (other_scope, other_project, AgentKind::Codex),
            (latest, target, AgentKind::Codex),
        ] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: agent,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                })
                .await
                .unwrap();
        }

        let selected = store
            .reader
            .open_sessions_for_scope_agent(ws, target, AgentKind::Codex, Some(1))
            .await
            .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].session_id, latest);
    }
}
