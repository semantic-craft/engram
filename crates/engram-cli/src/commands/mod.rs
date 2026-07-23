//! Subcommand implementations.

use anyhow::{Context, Result, anyhow};

pub mod apply_shared;
pub mod audit_contamination;
pub mod auth;
pub mod auto_improve;
pub mod auto_improve_report;
pub mod backup;
pub mod bootstrap;
pub mod checkpoints;
pub mod commit;
pub mod curator;
pub mod data_purge;
pub mod delete_page;
pub mod embed;
pub mod finalize_session;
pub mod forget_sweep;
pub mod generate_auth_token;
pub mod hook;
pub mod hook_capture;
pub mod hook_drain_process;
pub mod hook_spool;
pub mod init;
pub mod install_hooks;
pub mod install_instructions;
pub mod install_mcp;
pub mod install_skills;
pub mod lint;
pub mod llm_test;
pub mod move_project;
pub mod openclaw_plugin;
pub mod path_util;
pub mod pending_writes;
pub mod purge_project;
pub mod read_page;
pub mod reindex;
pub mod rename_project;
pub mod render_shared;
pub mod reorg;
pub mod reset;
pub mod restore;
pub mod restore_page;
pub mod search;
pub mod serve;
pub mod setup_agent;
pub mod status;
pub mod uninstall;
pub mod user;
pub mod write_page;

/// Resolve the effective project name for a client command.
///
/// Precedence:
/// 1. `explicit` (the user's `--project` flag) when non-empty.
/// 2. Basename of the git repo root walked up from CWD (handles
///    running from any subdir of the project).
/// 3. Basename of the bare CWD (covers non-git directories).
///
/// Mirrors the heuristic the hook router uses in
/// `engram-hooks::router::resolve_project_ids`, so commands
/// auto-target the same project the user's interactive sessions
/// have been writing into. Dot-prefixed dirs are preserved
/// verbatim (`~/.config` → project `.config`).
pub(crate) fn resolve_project_name(explicit: Option<&str>) -> Result<String> {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        return Ok(p.to_string());
    }

    let cwd = std::env::current_dir().context("getting CWD for project auto-detect")?;

    // Shared with the hook router via `derive_project_name` so the CLI
    // and hooks agree on what "the project for this cwd" means. The
    // `MainRepoRoot` strategy walks worktrees back to the main repo
    // — a session in `~/repo-worktrees/feature-x/` and one in the
    // main checkout resolve to the same project name (the main repo's
    // basename), instead of fragmenting into separate projects.
    // Aligned change from the earlier CLI behaviour (which used the
    // worktree-local `discover_repo_root`).
    if let Some((name, _)) = engram_consolidate::derive_project_name(
        &cwd,
        engram_consolidate::ProjectNameStrategy::MainRepoRoot,
    ) {
        return Ok(name);
    }
    Err(anyhow!(
        "could not derive project name from CWD ({}); \
         pass --project explicitly",
        cwd.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_project_name_prefers_explicit_value() {
        assert_eq!(
            resolve_project_name(Some("explicit-project")).unwrap(),
            "explicit-project"
        );
    }
}
