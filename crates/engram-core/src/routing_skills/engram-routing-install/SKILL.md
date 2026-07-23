---
name: engram-routing-install
description: "Use this skill for any request to install, refresh, repair, inspect, or remove engram's agent-facing routing: managed instruction snippets, Agent Skills, CLAUDE.md/AGENTS.md integration, or local/global skill roots. Trigger by semantic intent rather than exact wording."
---
<!-- engram-managed: routing-skill -->

# engram routing install

Use this skill when the user wants engram's agent-facing routing instructions or managed Agent Skills installed, refreshed, repaired, or removed.

## Tools in this cluster

- `memory_install_self_routing` returns the canonical markered instruction block, marker strings, filename hints, notes, and managed skill payloads for agents that cannot let the MCP server write the host filesystem directly.

## Managed instruction marker

The always-loaded instruction block is owned by engram only between these markers.

- Start marker: `<!-- engram:start -->`
- End marker: `<!-- engram:end -->`

Refresh must replace the first complete marker-bounded block in place, preserving unrelated content before and after it. If no complete block exists, append the canonical block with one blank line of separation. Never edit unrelated instructions while refreshing engram routing.

## Managed skill marker

Every engram-managed `SKILL.md` file contains this ownership marker.

`<!-- engram-managed: routing-skill -->`

Installers and uninstallers should overwrite or remove same-name skill files only when that marker is present, unless the user supplied an explicit force option. If a same-name skill lacks the marker, skip it with an actionable message so user-authored skills are preserved.

## Skill install targets

Managed skills are ordinary Agent Skills. Their relative file path is `<skill>/SKILL.md`, and the installer prepends the selected skill root.

Project-local targets:

- `.claude/skills/<skill>/SKILL.md` for Claude-compatible installs.
- `.agents/skills/<skill>/SKILL.md` for cross-client installs.

Global targets:

- `~/.claude/skills/<skill>/SKILL.md` for Claude-compatible installs.
- `~/.agents/skills/<skill>/SKILL.md` for cross-client installs.

Use platform-aware path joining. Do not build paths by string concatenation.

## Refresh guidance

For an agent-side refresh, call the install-routing tool, choose the right instruction filename from its hints, write the markered block with the agent's file-edit tool, and write each managed skill file to the selected skill roots. Claude Code normally uses `CLAUDE.md` and `.claude/skills`; Codex, OpenCode, Cursor, Gemini CLI, and AGENTS-aware clients normally use `AGENTS.md` and `.agents/skills` unless the project says otherwise.

For a CLI refresh, prefer the canonical install command. The snippet and skills must be updated from the same core-owned assets so they do not drift.
