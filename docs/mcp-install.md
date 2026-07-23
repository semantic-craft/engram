# MCP install guide - additional clients

> All snippets below default to `http://127.0.0.1:49374` (local server). For a
> remote server (homelab, LAN box) substitute the appropriate URL AND add an
> `Authorization: Bearer <token>` header to the `headers` block when bearer auth
> is enabled. The MCP wire protocol expects the `/mcp` path suffix on the URL.

> **Transport is stateless by default.** Since v0.1.2 the HTTP transport
> answers each request independently (plain JSON, no `Mcp-Session-Id`
> required), so any client that points a remote URL at `/mcp` — including
> OpenCode `type: "remote"` and plain `curl` — works without an
> `mcp-remote` stdio shim (issue #3). The `mcp-remote` bridge is still
> needed for **Claude Desktop** specifically, because its config only
> supports stdio servers — not because of session state. If you run a
> client that *requires* MCP session continuity or server-initiated SSE
> streams, start the server with `engram serve --transport http
> --http-stateful` to restore rmcp's session mode.

This page documents how to register engram as an MCP server with
agent CLIs beyond the README quick start.

Claude Code, OpenAI Codex, Cursor, Gemini CLI, Antigravity CLI, Grok Build CLI, OpenClaw, OpenCode, and
OMP have automatic capture integrations (shell/PowerShell hooks for
Claude Code / Codex / Cursor / Gemini CLI / Antigravity CLI / Grok Build CLI, TypeScript plugin/extension
files for OpenClaw / OpenCode / OMP) and are covered in the
[main README](../README.md#quick-start). On native Windows, Claude Code uses
Git Bash `.sh` hooks rather than the PowerShell default used by other
script-hook agents. Grok captures lifecycle events, but it ignores
SessionStart stdout, so engram does not auto-inject handoffs for Grok.

Claude Desktop and VS Code Copilot are **MCP-only** here: they expose
long-term memory to their LLMs via engram's MCP tools
(`memory_query`, `memory_recent`, `memory_handoff_accept`, etc.), but
they do not auto-capture session events into engram's `/hook`
endpoint. The trade-off:

| | What you get | What you don't get |
|---|---|---|
| **MCP only** | LLM can query the wiki, accept handoffs, run memory_consolidate, and run `memory_auto_improve` learning reviews | No automatic session-end summaries; no auto-handoff at session boundaries |
| **MCP + hooks** | All of the above *plus* every prompt/tool-call captured automatically; handoffs surface at SessionStart with no human prompting | - |

For MCP-only use, you can still cover the session-boundary gap by asking
the LLM to call `memory_handoff_begin` manually before quitting.

For proactive tool use in MCP-capable clients that read project instructions,
also install the managed routing package from
[`docs/usage.md`](usage.md#install-the-routing-snippet-and-agent-skills). The
slim instruction block stays in the agent rules file, while supported Agent
Skills carry the detailed engram tool-routing guidance.

## Custom lifecycle bridges

Built-in integrations should use `engram install-hooks` rather than
calling `/hook` directly. For a third-party bridge that has its own
lifecycle vocabulary, keep the core `event` query param on one of
engram's canonical events when possible:

```bash
curl -X POST \
  'http://127.0.0.1:49374/hook?event=user-prompt&agent=other' \
  -H 'content-type: application/json' \
  -d '{"session_id":"sess-123","cwd":"/repo","prompt":"Fix auth"}'
```

If the source event has no canonical equivalent, opt in to extension
metadata instead of asking engram to expand its stored event enum:

```bash
curl -X POST \
  'http://127.0.0.1:49374/hook?event=lead.contact&agent=other&extension=fstech' \
  -H 'content-type: application/json' \
  -d '{"session_id":"sess-123","title":"Lead contacted","message":"Lead Maria requested a proposal"}'
```

With `extension=<namespace>`, unknown events are still stored as the
canonical `other` observation kind, but engram also preserves the
validated source event. You may pass `source_event=<name>` explicitly;
otherwise an unknown `event` value becomes the source event. Both tokens
must be ASCII letters, digits, `.`, `_`, `-`, or `:`; namespaces are
limited to 64 bytes and source-event names to 128 bytes. Unknown events
without `extension` intentionally collapse to `other` with no source
metadata.

> **One-shot tip:** every snippet below is also reachable from the
> CLI:
> ```bash
> engram install-mcp --client gemini-cli   # or cursor / claude-desktop / openclaw / omp / pi / antigravity-cli / vscode-copilot
> ```

---

## Cursor

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`engram install-hooks --agent cursor --apply`.

**Config file:**
- Per-project: `.cursor/mcp.json` in the workspace root.
- Global: `~/.cursor/mcp.json`.

```json
{
  "mcpServers": {
    "engram": {
      "url": "http://127.0.0.1:49374/mcp"
    }
  }
}
```

**Gotchas:**
- Cursor uses the `url` key for HTTP/SSE transports. Stdio uses
  `command` + `args` instead.
- Cursor hooks live in `~/.cursor/hooks.json` or `.cursor/hooks.json`.
  engram maps `sessionStart`, `sessionEnd`, `beforeSubmitPrompt`,
  `preToolUse`, `postToolUse`, `postToolUseFailure`, `preCompact`, and
  `stop` to the shared capture path.
- Cursor watches `hooks.json` on save. For MCP config changes, restart
  Cursor or toggle the server off+on in **Settings → MCP**.
- Sources: <https://cursor.com/docs/mcp>, <https://cursor.com/docs/hooks.md>

---

## VS Code GitHub Copilot

**Status:** ✅ MCP supported (workspace-default). ❌ No lifecycle hooks
(Copilot's agent mode does not expose `PreToolUse` / `PostToolUse` /
`SessionStart` yet, so engram's automatic capture is not active in
VS Code — call `memory_query`, `memory_write_page`, etc. from chat).

**Config file:**
- Workspace (recommended): `.vscode/mcp.json` in the repo root. Matches
  engram's per-cwd auto-scoping.
- User profile: run **MCP: Open User Configuration** in VS Code and use
  the `mcp.json` file it opens. The exact path is platform- and
  profile-specific; pass it to `--config-file` if you want engram to
  write that file directly.

**Schema (verified against VS Code's MCP reference):** top-level key is
`servers` (NOT `mcpServers`). HTTP endpoints use `type: "http"` and the
`url` field; the bearer token goes into an inline `headers` object.

```json
{
  "servers": {
    "engram": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp"
    }
  }
}
```

**With a bearer token** (rendered when `--auth-token` is passed):

```json
{
  "servers": {
    "engram": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

**Install command:**

```bash
# Print the snippet:
engram install-mcp --client vscode-copilot

# Or write .vscode/mcp.json in the current workspace directly:
engram install-mcp --client vscode-copilot --apply

# Or write the user-profile mcp.json opened by VS Code directly:
engram install-mcp --client vscode-copilot \
  --config-file /path/to/vscode-profile/mcp.json --apply
```

Aliases: `copilot`, `github-copilot`.

**Gotchas:**
- The top-level key must be `servers`. The `mcpServers` form (used by
  Claude Code / Cursor / Gemini CLI) is silently ignored by VS Code.
- After editing, open the MCP view in the Extensions sidebar and start
  the server (or use **MCP: Show installed servers**). VS Code does not
  auto-reload `.vscode/mcp.json` while the window is focused on another
  tab.
- Copilot Enterprise behaves the same as Copilot Individual/Business
  for MCP — your org may restrict which MCP servers Copilot is allowed
  to call; check **Settings → Copilot → MCP servers** if the server
  shows as blocked.
- Lifecycle hooks aren't possible until VS Code Copilot adds an agent
  hook surface. Until then, the auto-handoff flow that other agents
  enjoy (SessionStart auto-fetches a "where you left off" block) does
  not run here — ask the agent to call `memory_handoff_accept`
  manually if you want it.
- Sources:
  <https://code.visualstudio.com/docs/copilot/customization/mcp-servers>,
  <https://code.visualstudio.com/docs/agents/reference/mcp-configuration>

---

## Claude Desktop

**Status:** ✅ MCP supported (via stdio shim for HTTP). ❌ No lifecycle hooks.

**Config file:**
- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json`
- Linux: not officially distributed by Anthropic. Use Claude Code
  (terminal) instead.

**Important:** Claude Desktop's JSON config supports stdio MCP
servers only. To talk to engram's HTTP endpoint, bridge through
the community [`mcp-remote`](https://www.npmjs.com/package/mcp-remote)
stdio shim. Requires Node.js installed on the same machine.

```json
{
  "mcpServers": {
    "engram": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:49374/mcp"]
    }
  }
}
```

**Gotchas:**
- After editing the config, **fully quit and relaunch** Claude
  Desktop. "Check for Updates…" is not enough.
- Claude Desktop also has account-level remote custom connectors and
  `.mcpb` desktop extensions. The engram CLI manages the local
  JSON-config path because it works with localhost/LAN servers and does
  not require publishing an HTTPS connector.
- Claude Desktop exposes MCP tools but no lifecycle hooks, so automatic
  prompt/tool capture and session-boundary handoffs are not possible
  unless Anthropic adds a desktop hook/plugin surface.
- If the MCP indicator doesn't appear after restart, check the logs:
  `~/Library/Logs/Claude/mcp*.log` (macOS) or `%APPDATA%\Claude\logs\`
  (Windows).
- Sources: <https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop>,
  <https://support.claude.com/en/articles/11175166-how-to-connect-remote-mcp-integrations-to-claude>

---

## Gemini CLI

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`engram install-hooks --agent gemini-cli --apply`.

**Config file:**
- User: `~/.gemini/settings.json`
- Project: `.gemini/settings.json`

Gemini CLI uses `httpUrl` (not `url`) for streamable-HTTP MCP
endpoints. The `timeout` is in milliseconds.

```json
{
  "mcpServers": {
    "engram": {
      "httpUrl": "http://127.0.0.1:49374/mcp",
      "timeout": 5000
    }
  }
}
```

**Hooks:**

```bash
engram install-hooks --agent gemini-cli --apply
```

Gemini CLI's lifecycle event names differ from Claude Code's, so use
`install-hooks --agent gemini-cli` rather than copying another agent's
settings. engram maps Gemini's `SessionStart`, `SessionEnd`,
`BeforeTool`, `AfterTool`, and `PreCompress` events to the shared hook
capture path; `SessionStart` also fetches pending handoffs.

**Gotchas:**
- Gemini supports stdio too via `command`/`args`, plus SSE via `url`.
  Only `httpUrl` covers streamable HTTP. Don't mix them in one entry.
- Restart the CLI session after changing `~/.gemini/settings.json` so
  both MCP servers and hooks are reloaded.
- Source: <https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md>

---

## Antigravity CLI (`agy`)

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`engram install-hooks --agent antigravity-cli --apply`.

**Config file (MCP):** `~/.gemini/antigravity-cli/mcp_config.json`

Antigravity CLI is the successor to Gemini CLI, built in Go with
parallel subagent support. It uses a separate `mcp_config.json`
(instead of Gemini CLI's combined `settings.json`) and uses
`serverUrl` (not `httpUrl`) for streamable-HTTP endpoints.

```bash
# One-shot via CLI:
engram install-mcp --client antigravity-cli
```

The rendered snippet writes to `mcp_config.json` under `mcpServers`:

```json
{
  "mcpServers": {
    "engram": {
      "serverUrl": "http://127.0.0.1:49374/mcp",
      "timeout": 5000
    }
  }
}
```

**Config file (hooks):** `~/.gemini/config/hooks.json`

Antigravity CLI uses a named-groups hook format. Each top-level key
is a hook group name; inside, event arrays map to handlers. Tool
events (`PreToolUse`, `PostToolUse`) use nested shape with matcher;
lifecycle events (`PreInvocation`, `Stop`) use flat shape.

```bash
# One-shot via CLI:
engram install-hooks --agent antigravity-cli --apply
```

The rendered hooks config looks like:

```json
{
  "engram": {
    "PreInvocation": [
      {
        "type": "command",
        "command": "ENGRAM_HOOK_URL=http://127.0.0.1:49374 /path/to/session-start.sh"
      }
    ],
    "PreToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "ENGRAM_HOOK_URL=http://127.0.0.1:49374 /path/to/pre-tool-use.sh"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "ENGRAM_HOOK_URL=http://127.0.0.1:49374 /path/to/post-tool-use.sh"
          }
        ]
      }
    ],
    "Stop": [
      {
        "type": "command",
        "command": "ENGRAM_HOOK_URL=http://127.0.0.1:49374 /path/to/stop.sh"
      }
    ]
  }
}
```

**Gotchas:**
- Antigravity CLI uses `serverUrl` for HTTP MCP endpoints, not `url`
  or `httpUrl`. The `--apply` flag writes the correct key.
- Hook scripts are staged under `~/.local/share/engram/hooks/antigravity-cli/`.
- The `PreInvocation` event fires before each model call (not just at
  session start). engram uses it as the closest equivalent to Gemini
  CLI's `SessionStart`; when a pending handoff exists, the hook injects
  it via Antigravity's `injectSteps[].ephemeralMessage` output.
- Antigravity CLI does not expose a true session-end hook. `Stop` records
  a stop observation only; call `memory_handoff_begin` before quitting when
  you need the next agent to receive a handoff.
- Source: <https://antigravity.google/docs/hooks>

---

## OpenClaw

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via a native
OpenClaw plugin generated by `engram install-hooks --agent openclaw --apply`.

**Config file:** `~/.openclaw/config.json` (the OpenClaw docs reference
this path indirectly; verify with your `openclaw config show`).

OpenClaw distinguishes transports explicitly. Use
`"transport": "streamable-http"` for engram's HTTP endpoint.

```json
{
  "mcp": {
    "servers": {
      "engram": {
        "url": "http://127.0.0.1:49374/mcp",
        "transport": "streamable-http"
      }
    }
  }
}
```

**Gotchas:**
- `install-hooks --agent openclaw --apply` writes a local plugin package
  under engram's data dir, then runs `openclaw plugins install --link
  <dir> --force` when the `openclaw` CLI is on `PATH`. If the CLI is not
  available, it prints the exact install command.
- The plugin registers OpenClaw `session_start`, `session_end`,
  `before_prompt_build`, `before_tool_call`, `after_tool_call`,
  `before_compaction`, and `agent_end` hooks. `before_prompt_build`
  injects pending handoffs via OpenClaw's `prependContext` hook result.
- Plugin installs or updates require a Gateway restart unless your
  managed OpenClaw Gateway auto-restarts after plugin source changes.
- Sources: <https://docs.openclaw.ai/cli/mcp>,
  <https://docs.openclaw.ai/plugins/hooks>,
  <https://docs.openclaw.ai/plugins/manage-plugins>

---

## Oh My Pi / OMP

**Status:** ✅ MCP supported via `install-mcp --client omp` (or
`--client oh-my-pi`). ✅ Lifecycle capture supported via
`engram install-hooks --agent omp --apply` (or `--agent oh-my-pi`).

**Config file:**
- User: `~/.omp/agent/mcp.json`
- Project: `.omp/mcp.json`

The current Oh My Pi package exposes the `omp` binary and native
`.omp` config directories. Use `omp` (or `oh-my-pi`) for this integration;
real `pi` is recognized separately and uses the generated bridge extension below.

```json
{
  "mcpServers": {
    "engram": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp",
      "enabled": true
    }
  }
}
```

**Lifecycle extension:**

```bash
engram install-hooks --agent omp --apply
# or: engram install-hooks --agent oh-my-pi --apply
```

This writes `~/.omp/agent/extensions/engram.ts`, which OMP discovers
as a direct TypeScript extension on startup. Restart `omp` after
installing or changing the file.

**Gotchas:**
- OMP extensions are TypeScript modules, not shell hooks; stdout is not
  used for context injection.
- The extension uses OMP lifecycle events for prompt/tool capture and
  `before_agent_start` to inject pending engram handoffs.

## Pi

**Status:** ✅ MCP and lifecycle capture supported via generated bridge
extension. Pi has no native `mcp.json`; use `install-hooks --agent pi --apply`
to write `~/.pi/agent/extensions/engram.ts`.

```bash
engram install-hooks --agent pi --apply
```

The generated extension posts lifecycle events to `/hook`, fetches pending
handoffs in `before_agent_start`, initializes engram's HTTP `/mcp` endpoint,
lists tools, and registers each one with `pi.registerTool`. `install-mcp
--client pi` intentionally prints this bridge guidance instead of writing an
ignored `~/.pi/agent/mcp.json`.

OMP / Oh My Pi remains separate: use `--client omp` / `--agent omp` (or
`oh-my-pi`) for `.omp` paths.

---

## After registering MCP - verify it works

Regardless of which client you used, the first sanity check is the
same: ask the model to list its available MCP tools, or to call
`memory_status` explicitly.

```
You: List the MCP tools you can call. Use one of them to check
     engram's status.

Model (any client): I can call: memory_query, memory_recent,
     memory_status, memory_briefing, memory_explore,
     memory_handoff_accept, memory_handoff_begin, memory_handoff_cancel,
     memory_consolidate, memory_auto_improve, memory_write_page, memory_read_page, memory_delete_page,
     memory_lint, memory_forget_sweep, memory_install_self_routing.
     memory_status reports: 0 pages, 0 observations, 0 sessions.
```

If the model sees the tools but does not call them proactively, refresh the
managed routing package. The `memory_install_self_routing` tool is read-only:
it returns the slim markered instruction block, marker strings, agent filename
hints, managed skill payloads (`name`, `description`, `relative_path`,
`content`), project/global target hints for `.claude/skills` and
`.agents/skills`, and overwrite guidance. Agents should use their own file
editing tools to write those artifacts while preserving unrelated user content.

If the model doesn't see any of those tools, the MCP registration
isn't being picked up. Check:

1. **Is the server running?** `curl http://127.0.0.1:49374/mcp` should
   return a JSON-RPC error (not a connection refused). If refused,
   start engram: `engram serve --transport http`.
2. **Did the client reload the config?** Claude Desktop and OMP need a
   restart. Cursor watches hooks but usually needs MCP reload/toggle.
   OpenClaw plugin changes need a Gateway restart unless it auto-restarted.
3. **Are you on the right port?** engram's default is **49374**
   (`0xC0DE` in hex). If you remapped, update the URL in every
   client's config.

If the model sees the tools but they all error, the server is
probably running in a different data dir than expected. Check
`engram status --json` for the data dir on disk.

---

## When does the auto-handoff actually work?

The cross-agent handoff feature (the "headline" pitch in the README)
requires both sides - the agent that *ends* a session, and the agent
that *starts* the next one - to play nicely with engram:

| Side | What's needed | Covered by |
|---|---|---|
| **Ending side** | The agent must create a handoff, either through a true session-end hook, the supported Codex manual finalizer, or by calling `memory_handoff_begin`. | Built-in automatically for Claude Code, Cursor, Gemini CLI, Grok Build CLI, OpenClaw, OpenCode, and OMP. Codex has no reliable true session-end event, so run `engram finalize-session` when you need the final summary/handoff/auto-improve eligibility. Antigravity CLI has no true session-end event in the current integration, so ask it to call `memory_handoff_begin` before quitting when you need a handoff. |
| **Starting side** | Either (a) the session-start/plugin path injects the handoff via `/handoff`, OR (b) the model proactively calls `memory_handoff_accept` on first turn. | (a) is built-in for Claude Code / Codex / Cursor / Gemini CLI / Antigravity CLI / OpenClaw / OpenCode / OMP. Grok is explicitly excluded because it ignores SessionStart stdout; use (b). (b) works for any MCP-capable client if you nudge the model - see [the managed routing package](usage.md#install-the-routing-snippet-and-agent-skills). |

OpenCode uses its official `session.deleted` plugin event for true session-end
delivery. Its generated plugin also sends a deduped best-effort close for any
still-active sessions from `dispose` during normal plugin teardown; abrupt
process exits can still lose that fallback, so `session.deleted` remains the
primary close path.

Codex `Stop` is not a session end. The Codex hook install intentionally omits
`SessionEnd`; `engram finalize-session` finds the latest open Codex session
for the current workspace/project and posts a synthetic `session-end` event
through the same server path as real hook clients. Use `--all` only when you
want to close every matching open Codex session in that scope.

So a typical mixed workflow looks like:

- **Claude Code → Cursor.** Claude Code's `SessionEnd` creates the
  handoff automatically. Cursor's `sessionStart` hook fetches and
  prepends it when `install-hooks --agent cursor --apply` is installed.
- **Claude Desktop → Claude Code.** Claude Desktop doesn't write a
  handoff (no hooks). To resume in Claude Code, you'd have had to
  call `memory_handoff_begin` manually in Claude Desktop before
  quitting. engram's wiki content via `memory_query` is still
  available either way.
