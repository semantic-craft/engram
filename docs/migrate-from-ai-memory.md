# Migrating a live ai-memory deployment to Engram

Audience: machines that ran the pre-fork `ai-memory` daemon (loopback
`127.0.0.1:49374`, launchd service, lifecycle hooks wired into
agent CLIs) and are switching to the `engram` binary. The wiki/data
format is identical — migration is: stop old service, move the data
directory, start the new service, repoint the integrations.

Run one machine at a time, during a window with **no active agent
sessions** (hooks write on every prompt). Verify each machine before
starting the next.

## What changes and what doesn't

| Surface | Old | New |
|---|---|---|
| Binary | `ai-memory` | `engram` |
| Env prefix | `AI_MEMORY_*` | `ENGRAM_*` |
| Data dir (macOS) | `~/Library/Application Support/ai-memory` | `…/engram` |
| launchd label | `com.semantic-craft.ai-memory` | `com.semantic-craft.engram` |
| MCP tool names | `memory_query`, `memory_write_page`, … | **unchanged** |
| Wiki markdown + SQLite | — | **unchanged, moved as-is** |
| Port | `127.0.0.1:49374` | **unchanged** |

## Steps (per machine)

1. Build/install the new binary and confirm it runs:
   `cargo build --release && install target/release/engram ~/.local/bin/`.
2. Dry-run the migration script, read its plan, then apply:

   ```bash
   scripts/migrate-from-ai-memory.sh          # dry run, prints plan
   scripts/migrate-from-ai-memory.sh --apply
   ```

   The script stops the old service, `mv`s the data dir, rewrites the
   daemon wrapper script and service file, starts the new service, and
   health-checks `127.0.0.1:49374`. The old service file and wrapper
   are left in place (renamed `.retired`) for rollback.
3. Repoint integrations (manual, per machine):
   - Re-run hook/MCP installation so agent configs reference the new
     binary: `engram install-hooks --apply` (repeat per agent CLI in
     use) — or hand-edit the `command` paths in
     `~/.claude/settings.json` etc.
   - Update any personal docs that name the old binary (e.g. global
     `CLAUDE.md`: `ai-memory write-page …` → `engram write-page …`).
   - Refresh per-project routing blocks: in each active project,
     `engram install-instructions` (rewrites the
     `<!-- engram:start -->` block; delete a stale
     `<!-- ai-memory:start -->` block if present).
   - Grep for stragglers:
     `grep -rn 'AI_MEMORY_\|ai-memory' ~/.zshrc ~/.config ~/.local/bin 2>/dev/null`.
4. Verify: run a short agent session in some project; check
   `engram status`, confirm the session is captured, and confirm
   semantic search returns pre-migration pages (embeddings move with
   the data dir).

## Rollback

The data dir is moved with `mv` (no copy divergence). To roll back:
stop the engram service, `mv` the data dir back to its old path,
`launchctl bootstrap` the retired old service file, and restore the
old hook configs (`ai-memory install-hooks --apply`). Nothing is deleted by the migration script.
