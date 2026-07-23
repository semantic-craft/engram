# Day-to-day usage

This page covers what happens after engram is installed: handoffs,
compaction recovery, proactive memory queries, the web UI, and the
managed routing snippet + Agent Skills package.

## Cross-agent handoff

You normally do not create handoffs by hand. With lifecycle hooks
installed, session-end capture writes the handoff and the next
session-start hook fetches it.

```text
$ claude
> Working on the auth refactor. JWT rotation is broken; trying session cookies.
[work for an hour]
> /exit

$ codex   # in the same directory, later
[SessionStart hook fetches the handoff; Codex sees it before your prompt.]
> Picking up: you were investigating session cookies as an alternative...
```

If an agent has MCP but no lifecycle hook surface, ask it to call
`memory_handoff_begin` before quitting. The next hooked agent can still
consume that handoff automatically.

If an agent creates a handoff by mistake, cancel it immediately with
`memory_handoff_cancel` and the `handoff_id` returned by
`memory_handoff_begin`. Cancelling marks the handoff expired, so the next
session-start hook will not consume stale context.

## Compaction recovery

When Claude Code or Codex compact their working context, the
`PreCompact` hook fires and engram writes a fresh
`sessions/<id>.md` page summarising the session so far. After
compaction, the agent can recover the summary via `memory_recent` even
though its raw chat history was compacted away.

## Proactive memory queries

Hooks handle capture without prompting. Proactive querying depends on
the agent knowing which MCP tool to call for each situation. Install the
managed routing package once: a slim always-loaded snippet points agents
at the managed engram Agent Skills that carry detailed tool routing.

| You say | Agent calls | Effect |
|---|---|---|
| "Have we discussed X?" / "search memory for Y" | `memory_query` | FTS5 + graph/vector RRF over compiled wiki pages, with bounded raw-observation fallback. |
| Before proposing architecture | `memory_query` | Checks prior decisions and gotchas before suggesting designs. |
| "Catch me up" / "I've been away" | `memory_explore` | Prose digest whose verbosity scales with time since last activity. |
| "Where did we leave off?" | Existing handoff block, or `memory_handoff_accept` if no block exists | Resumes from the latest pending handoff. |
| "Save context for the next session" | `memory_handoff_begin` | Writes a terse session-end handoff with open questions and next steps. Do not use for status or briefing requests. |
| "Discard that handoff" / "I created a handoff by mistake" | `memory_handoff_cancel` | Marks an exact open handoff id expired before the next session can consume it. |
| "Consolidate this session" | `memory_consolidate` | Manually runs LLM consolidation. Also runs on PreCompact, and at session end only when `ENGRAM_CONSOLIDATE_ON_SESSION_END` is set (off by default; session end otherwise writes a rule-based summary page). |
| "What did we learn from this session?" / "what memory should we add?" | `memory_auto_improve` | Manually reviews the latest completed session by default. The server also runs scheduled auto-improvement for new completed sessions when an LLM is configured. `[auto_improve.scheduler] enabled = false` disables automatic review; `[auto_improve] require_approval = true` leaves scheduled and manual proposals in pending-writes for review. |
| "Remember this permanently" / "add an annotation" | `memory_write_page` | Writes durable wiki knowledge; not a single-use handoff. |
| "Delete this page" / "remove the note about X" | `memory_delete_page` | Removes a page by exact path. Pass `workspace` + `project` together when the page lives in a sibling workspace, so a project name shared between workspaces never silently routes the delete to the wrong slot. |
| "Audit the wiki" / "any contradictions?" | `memory_lint` | Runs stale-page, contradiction, and rule-suggestion checks. |
| "How big is the wiki?" / "stats?" | `memory_status`, `memory_briefing` | Counts and recent activity windows; `memory_briefing` is read-only. |

Agents should treat retrieved memory as operating guidance. When search returns
matching `_rules/`, `gotchas/`, `procedures/`, or `decisions/` pages, read the
full page before acting: rules are constraints, gotchas are preflight warnings,
procedures are checklists, and decisions are settled architecture unless the
user explicitly asks to revisit them.

## Install the routing snippet and Agent Skills

From an agent, say:

```text
Install engram routing into this project.
```

The agent calls `memory_install_self_routing` and receives the slim
`markered_block`, marker strings, rules-file hints, managed skill payloads,
skill target hints, and overwrite guidance. It then uses its normal file-edit
tool to preserve unrelated user content, replace or append the
`<!-- engram:start -->` / `<!-- engram:end -->` block, and write each
managed skill below the selected skill root. Skill files are engram-managed
only when they contain the managed marker, so unmanaged same-name skills should
not be overwritten unless the human explicitly forces replacement.

From a terminal:

```bash
engram install-instructions
engram install-instructions --target AGENTS.md
engram install-instructions --print
engram install-instructions --no-skills
```

`install-instructions` installs or updates managed skills by default. Use
`--no-skills` only when you intentionally want a snippet-only refresh.
The CLI replaces only the markered engram block, preserves unrelated content,
and writes a timestamped backup before changing an existing instruction file.
`install-instructions --print` previews the instruction snippet only; use
`install-skills --print` to preview skill payloads. Skill flags mirror
`install-skills` with an `--skills-` prefix:
`--skills-scope project|global`, `--skills-agent claude-code|agents|both`,
`--skills-target-dir <dir>`, and `--skills-force`.

Auto-detect extends `CLAUDE.md` when it exists, `AGENTS.md` when it
exists, both when both exist, or creates `CLAUDE.md` when neither exists. Use
`--target AGENTS.md` for non-Claude-only projects. The skill target follows the
instruction target unless you override it: `CLAUDE.md` implies
`.claude/skills`, `AGENTS.md` implies `.agents/skills`, and both files imply
both skill roots.

To refresh only the managed Agent Skills:

```bash
engram install-skills
engram install-skills --scope global --agent agents
engram install-skills --agent both --print
engram install-skills --target-dir .custom/skills --force
```

Project-local skill roots are `.claude/skills` for Claude-compatible installs
and `.agents/skills` for cross-client installs. Global roots are
`~/.claude/skills` and `~/.agents/skills`. `--target-dir` points at an explicit
skill root and bypasses scope/agent inference. `--print` previews target paths
and `SKILL.md` contents. `--force` allows replacement of unmanaged same-name
skills; without it, user-authored skills are preserved. Uninstall removes
engram-managed skills from the default project/global roots after marker
validation; custom `--target-dir` roots are a manual cleanup path.

This is prompt packaging only. engram does not run a runtime skill router,
does not store durable memory in `SKILL.md`, and does not turn the
auto-improvement loop into a skill-authoring system. Durable knowledge still
lives in the wiki.

## Bootstrap an existing project

If you install engram into a project that already has months of
history, the wiki starts empty. `engram bootstrap` seeds it from the
existing repo history and docs.

```bash
export ENGRAM_SERVER_URL="http://localhost:49374"
engram bootstrap --dry-run
engram bootstrap
```

The bootstrap collector reads `git log`, the root README, `docs/`,
project rule files, and Rust module docs, then POSTs the selected
sources to the running server. It requires an LLM provider on the
server. See [Installation cookbook - bootstrap mid-project](install.md#bootstrap-mid-project)
for flags, token budgets, and source priority.

## Migrate from another memory tool

When replacing an existing memory system, treat the old data as untrusted
historical input until you curate it. Do not pipe raw transcripts or old memory
stores directly into engram.

Migration checklist:

1. Export the old memory or history before changing hooks.
2. Keep the raw export as an archive, not as current project truth.
3. Scrub secrets, tokens, credentials, API keys, and raw logs that should not
   become durable memory.
4. Curate the useful material into reviewed Markdown pages under a temporary
   docs directory or directly into `concepts/`, `decisions/`, `gotchas/`,
   `procedures/`, `notes/`, or `_rules/`.
5. If this checkout might be ambiguous, add `.engram.toml` to pin the intended
   workspace/project before importing or installing hooks.
6. Start `engram serve` locally and confirm `engram status` can reach the
   server before touching existing client configs.
7. Import curated material first; avoid importing the full legacy raw history.
8. Verify expected pages are searchable with `memory_query` or `engram search`.
9. Configure MCP and lifecycle hooks for one client at a time.
10. Only after engram capture and retrieval work, disable the old memory
    hooks, plugins, or MCP servers.
11. Search each client config for stale references to the old tool and remove
    stale `Authorization` headers or env vars if bearer auth changed.
12. Restart each agent CLI after changing hooks, plugins, or MCP config.

Client cleanup hints:

- Claude Code: check plugins, hooks, old SessionStart injection, and MCP servers.
- Codex: check MCP config plus session/user-prompt/tool/compaction/stop hooks.
- Gemini CLI and Antigravity CLI: check `settings.json` or equivalent hook/MCP
  config files.
- OpenCode, OpenClaw, and OMP: check MCP config and plugin/extension directories;
  move old memory plugins to a disabled/quarantine directory before deleting.
- VS Code Copilot and Claude Desktop: these are usually MCP-only, so confirm
  whether the old tool was providing capture hooks elsewhere.

If you want a visible startup reminder during the transition, keep it small. A
rules-file note such as “Active memory: engram; legacy export is historical
reference only; use memory_query for retrieval” is safer than dumping large
legacy context into every session.

If you use the ChatGPT/Codex OAuth provider, sign in once before starting the
server with `ENGRAM_LLM_PROVIDER=openai-oauth`:

```bash
engram auth login openai-oauth
engram auth status
```

The login command stores only provider credentials in `<data_dir>/auth.json`.
It is separate from `ENGRAM_AUTH_TOKEN`, which protects MCP, hooks, and the
web UI.

For GitHub Copilot, use the matching provider login before starting the server
with `ENGRAM_LLM_PROVIDER=copilot`:

```bash
engram auth login copilot
engram auth status
```

Copilot auth stores a GitHub user token, then the provider exchanges it for a
short-lived Copilot API token before each LLM call.

## Browse the wiki in a browser

Start the server with `--enable-web` and open
`http://<host>:49374/web`.

```bash
engram serve --transport http --bind 127.0.0.1:49374 --enable-web
```

The web UI is read-only: project list, per-project page tree,
breadcrumbs, rendered markdown, metadata, and FTS5 search. In rendered
pages, `[[wiki links]]` become clickable links to the target page —
`[[path]]`, `[[path|label]]`, `[[project:path]]`, and
`[[workspace/project:path]]` are all supported (resolved against the
current page's project unless the target carries its own scope).
`[[…]]` stays literal inside fenced code (` ``` ` and `~~~` close
only by their own glyph), inline `` `…` `` code, and 4-space-indented
code; external schemes inside the brackets (`http://`, `https://`,
`mailto:`, `data:`, `javascript:`, `vbscript:`, `tel:`, `file:`)
stay literal too. If the server has `ENGRAM_AUTH_TOKEN` set, the
browser uses HTTP Basic auth: leave the username blank and paste the
token as the password. MCP and hook clients continue to use
`Authorization: Bearer <token>`.

To host the web UI under a URL subpath behind a reverse proxy, the
`--base-path` / `--web-slug` flags do the work — see
[`docs/frontend-api.md`](frontend-api.md#6-custom-ui-hosting-and-base-paths)
for the flag semantics and
[`docs/https-via-proxy.md`](https-via-proxy.md#hosting-under-a-subpath)
for the proxy-side walk-through.

## Inspect the raw wiki

The wiki is plain markdown plus git history, stored under `<data_dir>/wiki`
(the data dir defaults to `~/Library/Application Support/engram` on macOS and
`%LOCALAPPDATA%\engram` on Windows; override with `ENGRAM_DATA_DIR`).

```bash
WIKI="$HOME/Library/Application Support/engram/wiki"
ls "$WIKI/sessions/"
cat "$WIKI/sessions/<uuid>.md"

# It's already a local directory — point Obsidian or any markdown viewer at it:
open "$WIKI"

# Time-travel:
git -C "$WIKI" log --oneline
```

## Rules vs facts

Durable project rules belong in the agent's rules file, not only in the
wiki. For Claude Code that is `CLAUDE.md`; for Codex, OpenCode,
Cursor, and Gemini CLI it is usually `AGENTS.md`.

The consolidator classifies compiled observations as `decision`,
`fact`, `rule`, or `gotcha`. Rule-tagged pages are routed to
`wiki/_rules/<slug>.md`, and `memory_lint` reports a suggestion when a
rule looks durable enough to copy into `CLAUDE.md` or `AGENTS.md`.

engram never edits the rules file on its own. The lint suggestion is
the whole workflow: copy the rule if it should apply every turn, ignore
it if it was temporary context.

## Architecture Decision Records (ADRs)

Two facts frame how ADRs and engram interact:

1. **engram never touches files in your repository.** Its wiki lives
   in the server's data dir; the background jobs (consolidation,
   curation, retention decay, auto-improvement) read and write wiki
   pages only. A `docs/adr/` directory in the repo — maintained by hand
   or by a dedicated ADR tool/MCP server (e.g.
   [joshrotenberg/adrs](https://github.com/joshrotenberg/adrs)) — is
   categorically outside engram's write surface. Run both side by
   side without ceremony: the ADR tool owns the canonical log, engram
   owns cross-session recall.

2. **Wiki pages marked `pinned: true` are immutable to automation.**
   Retention decay and curation skip them, and the auto-improvement
   apply path hard-refuses to rewrite them (the proposal is recorded as
   a conflict with the reason). Unpinning is the explicit opt-out.

For decisions recorded *in* the wiki, the managed durable-pages Agent
Skill teaches agents the recipe: `decisions/<slug>.md`, ADR structure
(Status / Context / Decision / Consequences, including rejected
alternatives), `pinned: true`, and supersede-by-new-page instead of
editing history. Ask an agent to "record this as an architectural
decision" and the skill does the rest; the structured shape also
retrieves noticeably better through `memory_query` than free-form
prose.
