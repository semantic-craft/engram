//! `engram install-instructions` — drop the proactive-use snippet
//! into a project's `CLAUDE.md` / `AGENTS.md` / other rules file.
//!
//! ## Why this exists
//!
//! Lifecycle hooks handle *capture* and *handoff surfacing*
//! automatically. What they can't do is make the agent *proactively
//! call* `memory_query` / `memory_recent` when it should — that
//! decision lives in the model's system prompt, fed turn-by-turn by
//! the project's CLAUDE.md / AGENTS.md.
//!
//! This subcommand drops a small, opinionated snippet into that
//! file. Idempotent via HTML-comment markers so re-running picks up
//! whatever the snippet evolves into without duplicating the block.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::{
    InstallInstructionsArgs, InstallSkillsAgent, InstallSkillsArgs, InstallSkillsScope,
};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic};
use crate::commands::install_skills;
use crate::config::Config;
use crate::instruction_steward::managed_instruction_regions;

// Markers + the snippet body live in `engram_core::routing_snippet`
// so the `memory_install_self_routing` MCP tool can return the same
// block this subcommand writes. Single source of truth.
use engram_core::full_block;

#[cfg(test)]
use engram_core::{MARKER_END, MARKER_START};

/// Run the `install-instructions` subcommand.
///
/// # Errors
/// Returns an error if the target path can't be written or if the
/// existing file isn't valid UTF-8.
pub fn run(_config: &Config, args: InstallInstructionsArgs) -> Result<()> {
    let block = full_block();
    let targets = resolve_targets(args.target.as_ref())?;
    let skill_args = if args.no_skills {
        None
    } else {
        Some(skill_args_from_instruction_args(&args, &targets))
    };
    let prepared_skills = if !args.print {
        skill_args
            .as_ref()
            .map(install_skills::prepare_install)
            .transpose()?
    } else {
        None
    };

    if args.print {
        for t in &targets {
            println!("# Would write into: {}\n", t.display());
            println!("{block}");
        }
    } else {
        for target in &targets {
            let outcome = apply_atomic(target, |existing| {
                merge_instructions_block(existing, &block)
            })?;
            println!(
                "✓ {} {} ({})",
                outcome.verb(),
                target.display(),
                match outcome {
                    ApplyOutcome::Created => "new file",
                    ApplyOutcome::Updated => "backup written next to it",
                    ApplyOutcome::NoOp => "already up to date",
                }
            );
        }
    }

    if let Some(prepared_skills) = prepared_skills {
        install_skills::run_prepared(prepared_skills)?;
    }

    Ok(())
}

/// Decide which file(s) the snippet should land in.
///
/// Precedence:
/// 1. `--target` passed explicitly → use exactly that path (one file).
/// 2. Both `CLAUDE.md` and `AGENTS.md` exist in `$PWD` → write to both
///    (a project that's set up for multiple agent CLIs deserves the
///    snippet in each convention).
/// 3. Only `CLAUDE.md` exists → write to it.
/// 4. Only `AGENTS.md` exists → write to it.
/// 5. Neither exists → default to `CLAUDE.md` AND print a hint about
///    `--target AGENTS.md` for Codex / OpenCode / Cursor / Gemini.
///
/// The auto-pick exists because Claude Code uses CLAUDE.md while
/// every other supported agent (Codex, OpenCode, Cursor, Gemini CLI)
/// converged on AGENTS.md. The heuristic "extend whatever's already
/// there" matches the user's intent better than a hard-coded default.
fn resolve_targets(explicit: Option<&std::path::PathBuf>) -> Result<Vec<std::path::PathBuf>> {
    if let Some(p) = explicit {
        return Ok(vec![p.clone()]);
    }
    let cwd = std::env::current_dir().context("getting CWD for install-instructions target")?;
    let claude_md = cwd.join("CLAUDE.md");
    let agents_md = cwd.join("AGENTS.md");
    let has_claude = claude_md.exists();
    let has_agents = agents_md.exists();
    match (has_claude, has_agents) {
        (true, true) => Ok(vec![claude_md, agents_md]),
        (true, false) => Ok(vec![claude_md]),
        (false, true) => Ok(vec![agents_md]),
        (false, false) => {
            eprintln!(
                "note: neither CLAUDE.md nor AGENTS.md exists in {}; \
                 creating CLAUDE.md. If you use Codex / OpenCode / \
                 Cursor / Gemini CLI / Antigravity CLI, re-run with `--target AGENTS.md`.",
                cwd.display()
            );
            Ok(vec![claude_md])
        }
    }
}

fn skill_args_from_instruction_args(
    args: &InstallInstructionsArgs,
    targets: &[PathBuf],
) -> InstallSkillsArgs {
    InstallSkillsArgs {
        scope: args.skills_scope.unwrap_or(InstallSkillsScope::Project),
        agent: args
            .skills_agent
            .unwrap_or_else(|| infer_skills_agent_from_instruction_targets(targets)),
        target_dir: args.skills_target_dir.clone(),
        print: args.print,
        force: args.skills_force,
    }
}

fn infer_skills_agent_from_instruction_targets(targets: &[PathBuf]) -> InstallSkillsAgent {
    let mut has_claude_target = false;
    let mut has_agents_target = false;

    for target in targets {
        match target.file_name().and_then(|name| name.to_str()) {
            Some("CLAUDE.md") => has_claude_target = true,
            Some("AGENTS.md") => has_agents_target = true,
            _ => {}
        }
    }

    match (has_claude_target, has_agents_target) {
        (true, true) => InstallSkillsAgent::Both,
        (false, true) => InstallSkillsAgent::Agents,
        _ => InstallSkillsAgent::ClaudeCode,
    }
}

/// Idempotent merge: when the markers exist, replace everything
/// between them (inclusive) with `block`. When they don't, append
/// `block` to the end of the file with a single blank-line
/// separator. The user's other content is never touched.
fn merge_instructions_block(existing: &str, block: &str) -> Result<String> {
    let existing_regions = managed_instruction_regions(existing)?;
    let newline = instruction_newline(existing)?;
    let normalized_block = block.replace("\r\n", "\n").replace('\r', "\n");
    let block = if newline == "\r\n" {
        normalized_block.replace('\n', "\r\n")
    } else {
        normalized_block
    };
    let out = if let Some(routing) = existing_regions.routing {
        let start_idx = routing.start;
        let end_idx = routing.end;
        // Consume a trailing newline after the end marker if present
        // so we don't accumulate blank lines on every re-run.
        let after_end = if existing[end_idx..].starts_with("\r\n") {
            end_idx + 2
        } else if existing.as_bytes().get(end_idx).copied() == Some(b'\n') {
            end_idx + 1
        } else {
            end_idx
        };
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..start_idx]);
        out.push_str(&block);
        out.push_str(&existing[after_end..]);
        out
    } else {
        // No prior block — append. If the file already ends with a
        // newline, separate with one blank line; otherwise add the
        // newline + a blank line.
        let mut out = existing.to_string();
        if !out.is_empty() && !out.ends_with(newline) {
            out.push_str(newline);
        }
        if !out.is_empty() {
            out.push_str(newline);
        }
        out.push_str(&block);
        out
    };
    let output_regions = managed_instruction_regions(&out)?;
    let existing_approved = existing_regions
        .approved_rules
        .map(|range| &existing[range]);
    let output_approved = output_regions.approved_rules.map(|range| &out[range]);
    if existing_approved != output_approved {
        anyhow::bail!("routing refresh would modify the approved-rules region");
    }
    Ok(out)
}

fn instruction_newline(content: &str) -> Result<&'static str> {
    let without_crlf = content.replace("\r\n", "");
    let has_crlf = content.contains("\r\n");
    let has_lf = without_crlf.contains('\n');
    let has_bare_cr = without_crlf.contains('\r');
    if has_bare_cr || (has_crlf && has_lf) {
        anyhow::bail!("instruction target uses unsupported mixed or bare-CR newlines");
    }
    Ok(if has_crlf { "\r\n" } else { "\n" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_appends_to_empty_file() {
        let out = merge_instructions_block("", "BLOCK\n").unwrap();
        assert_eq!(out, "BLOCK\n");
    }

    #[test]
    fn merge_appends_when_no_markers_present() {
        let original = "# My project\n\nSome notes.\n";
        let out = merge_instructions_block(original, "BLOCK\n").unwrap();
        assert!(out.starts_with("# My project"));
        assert!(out.ends_with("BLOCK\n"));
        // One blank line between user content and our block.
        assert!(out.contains("Some notes.\n\nBLOCK\n"));
    }

    /// Real-world contract: the caller passes a marker-wrapped
    /// block (that's what `run()` builds). The merge replaces the
    /// prior bracketed section in place.
    #[test]
    fn merge_replaces_existing_block() {
        let original =
            format!("# My project\n\n{MARKER_START}\nOLD\n{MARKER_END}\n\nMore notes.\n");
        let new_block = format!("{MARKER_START}\nNEW BLOCK\n{MARKER_END}\n");
        let out = merge_instructions_block(&original, &new_block).unwrap();
        assert!(out.contains("# My project"));
        assert!(out.contains("NEW BLOCK"));
        // Old content gone.
        assert!(!out.contains("OLD"));
        // User content after the block is preserved.
        assert!(out.contains("More notes."));
        // No duplicate markers.
        assert_eq!(out.matches(MARKER_START).count(), 1);
        assert_eq!(out.matches(MARKER_END).count(), 1);
    }

    #[test]
    fn merge_idempotent_double_run() {
        let block = format!("{MARKER_START}\nBLOCK\n{MARKER_END}\n");
        let first = merge_instructions_block("# Title\n", &block).unwrap();
        let second = merge_instructions_block(&first, &block).unwrap();
        assert_eq!(first, second, "second merge must be a no-op");
    }

    /// Defensive: existing file ends without trailing newline. We
    /// should still produce well-formed output.
    #[test]
    fn merge_tolerates_missing_trailing_newline() {
        let out = merge_instructions_block("# Title", "BLOCK\n").unwrap();
        assert!(out.starts_with("# Title\n"));
        assert!(out.ends_with("BLOCK\n"));
    }

    #[test]
    fn routing_refresh_preserves_approved_rules_region_exactly() {
        let approved = "<!-- engram:approved-rules:start -->\nhuman-approved\n<!-- engram:approved-rules:end -->";
        let original =
            format!("# Rules\n\n{approved}\n\n{MARKER_START}\nOLD\n{MARKER_END}\n\nHuman tail.\n");
        let block = format!("{MARKER_START}\nNEW\n{MARKER_END}\n");
        let out = merge_instructions_block(&original, &block).unwrap();
        assert!(out.contains(approved));
        assert!(out.contains("NEW"));
        assert!(out.ends_with("Human tail.\n"));
    }

    #[test]
    fn routing_refresh_rejects_malformed_or_crossed_markers() {
        let approved_start = "<!-- engram:approved-rules:start -->";
        let approved_end = "<!-- engram:approved-rules:end -->";
        for malformed in [
            format!("{MARKER_START}\nmissing end\n"),
            format!("{MARKER_START}\none\n{MARKER_END}\n{MARKER_START}\ntwo\n{MARKER_END}"),
            format!("{MARKER_START}\n{approved_start}\ncrossed\n{MARKER_END}\n{approved_end}"),
        ] {
            assert!(merge_instructions_block(&malformed, "BLOCK\n").is_err());
        }
    }

    #[test]
    fn routing_refresh_preserves_crlf_style() {
        let original = format!("# Rules\r\n\r\n{MARKER_START}\r\nOLD\r\n{MARKER_END}\r\n");
        let block = format!("{MARKER_START}\nNEW\n{MARKER_END}\n");
        let out = merge_instructions_block(&original, &block).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert!(out.contains("\r\nNEW\r\n"));
    }
}
