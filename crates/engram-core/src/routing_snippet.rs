//! Canonical CLAUDE.md / AGENTS.md routing snippet.
//!
//! This module owns the slim always-loaded base guidance that points agents at
//! the engram MCP server and, when installed, the detailed engram Agent
//! Skills.
//!
//! Two callers consume it:
//!
//! - `engram-cli`'s `install-instructions` subcommand — writes the
//!   block into `./CLAUDE.md` directly from the host.
//! - `engram-mcp`'s `memory_install_self_routing` MCP tool — returns
//!   the block plus managed skill files to the agent, which then uses its
//!   own Write/Edit tool to update the target file and skill root (the MCP
//!   server can't reach the agent's host filesystem).
//!
//! Keeping the snippet in one constant means "what gets written" stays
//! consistent across both paths; updating it once propagates.

/// HTML-comment marker that opens the managed section. Anything that
/// edits a CLAUDE.md must key off this exact string — install /
/// uninstall / refresh all locate the block by these markers.
pub const MARKER_START: &str = "<!-- engram:start -->";

/// HTML-comment marker that closes the managed section.
pub const MARKER_END: &str = "<!-- engram:end -->";

/// The canonical snippet body. Trimmed of leading/trailing whitespace
/// by callers; wrap with `MARKER_START` + `MARKER_END` before writing.
pub const SNIPPET_BODY: &str = r#"
## Long-term memory (engram)

This project uses [engram](https://github.com/semantic-craft/engram)
for cross-session continuity.

**Default to the current project - always.** Every engram tool
auto-scopes to the project resolved from your session's working
directory. **Do NOT pass `project`, `workspace`, or `cwd` arguments unless
the user explicitly references a *different* project by name** (e.g. "what
did we decide in the `other-app` project?"). Phrases like "this project",
"here", "we", "our work", and "where did we leave off" all mean the
*current* project, so call tools with no scoping args.

This default assumes the MCP client can identify the current agent
session. Static MCP clients in parallel sessions for the same user cannot
forward the real agent session id automatically; pass explicit
`workspace` + `project` / `scopes`, or use a session-aware bridge that
forwards the lifecycle-hook session id on MCP calls.

**Lifecycle hooks already capture every prompt and tool call
automatically.** Do not manually write routine notes. Only write durable
memory when the user explicitly asks to remember or annotate something
permanently.

### Use the installed engram Agent Skills

Detailed tool-routing guidance lives in the installed engram Agent
Skills. When a task matches an installed engram Agent Skill, load and
follow that skill before calling engram tools. The skills cover memory
retrieval, handoffs, durable pages, learning maintenance,
project-instruction maintenance, and routing install or refresh work.
Memory retrieval uses caller-budgeted context packages with stable,
revisioned references; load the retrieval skill for the exact package and
full-evidence workflow.

### When you write a project rule, write it here

If you're about to write a durable project rule ("always X", "never
Y", "all PRs must ..."), write it in the project's canonical agent instruction file.
Many projects use CLAUDE.md for Claude Code and
AGENTS.md for Codex / OpenCode / Cursor / Gemini CLI, but if the project
says one file is canonical, use that file.

If the rule is a standing *user/team* preference that should apply to
every project (tech choices, code style, personal conventions), save it
to engram's reserved global scope instead — the durable-pages skill
covers how. Default memory reads surface global-scope pages in every
project automatically.

### Refreshing this snippet

This block is maintained by engram. Two ways to refresh it with the
latest binary's recommended copy:

- **From the agent** (no terminal needed): ask "refresh the engram
  routing in this project". The agent calls `memory_install_self_routing`,
  picks the right filename for itself (Claude Code -> `CLAUDE.md`; Codex /
  OpenCode / Cursor / Gemini -> `AGENTS.md`), uses its Write / Edit tool
  to replace or append the returned `markered_block` while preserving
  non-engram user content, then writes or updates each returned
  `managed_skills` item under the selected skill root from `target_hints`
  using its `relative_path`.
- **From the CLI**: `engram install-instructions` (defaults to
  `CLAUDE.md`; pass `--target AGENTS.md` for non-Claude agents or projects
  that use `AGENTS.md` as the canonical instruction file).

Both are idempotent: re-runs replace the block bracketed by
`<!-- engram:start -->` / `<!-- engram:end -->` markers without
disturbing the rest of the file.
"#;

/// Build the full markered block that should land in CLAUDE.md /
/// AGENTS.md, including the `<!-- engram:start -->` / `<!-- ai-
/// memory:end -->` wrappers and a trailing newline.
///
/// Both the CLI's `install-instructions` and the MCP tool
/// `memory_install_self_routing` emit this exact string.
#[must_use]
pub fn full_block() -> String {
    format!("{MARKER_START}\n{}\n{MARKER_END}\n", SNIPPET_BODY.trim())
}

#[cfg(test)]
mod tests {
    use super::SNIPPET_BODY;

    #[test]
    fn routes_project_instruction_maintenance_without_embedding_the_workflow() {
        assert!(
            SNIPPET_BODY.contains("project-instruction maintenance"),
            "the slim routing block must point agents at the maintenance skill"
        );
        for detailed_command in [
            "instructions doctor",
            "instructions propose",
            "pending-writes approve",
            "instructions apply",
        ] {
            assert!(
                !SNIPPET_BODY.contains(detailed_command),
                "the always-loaded block must not embed `{detailed_command}`"
            );
        }
        assert!(
            SNIPPET_BODY.lines().count() <= 70,
            "the always-loaded block must remain concise"
        );
    }
}
