# Engram

> Long-term memory for AI coding agents — engine + desktop workbench.
> Quit Claude Code mid-task, start OpenAI Codex in the same directory,
> continue without re-explaining the architecture, the failed
> approaches, or the open questions. Then open the Engram app to
> search, read, and curate what your agents remember.

[![Rust](https://img.shields.io/badge/rust-1.95+-blue)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Engram is an independent hard fork of
[akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory)
(forked at v1.8.0, MIT). It keeps and deepens the same core idea — a
Karpathy-style LLM wiki as durable agent memory, markdown-in-git as
the source of truth — and adds a first-class desktop app
([`apps/desktop`](apps/desktop)) so the memory is something you can
actually browse, search in natural language, and edit. Upstream
changelog up to the fork point is preserved verbatim in
[`docs/upstream-changelog.md`](docs/upstream-changelog.md).

## Support Matrix

| Area | Status | Notes |
|---|---|---|
| macOS (Apple Silicon) | Supported | Workspace tests run in CI; tagged releases publish a native `engram-macos-aarch64.tar.gz` (arm64) binary — download, extract, run. The recommended path. See [`docs/macos.md`](docs/macos.md). |
| Windows (x86_64) | Experimental | Tagged releases publish `engram-windows-x86_64.zip` with `engram.exe` — download, extract, run. Claude Code uses Claude exec form with a real native `engram.exe` by default; other script-hook agents use the current PowerShell defaults pending harness feedback. See [`docs/windows.md`](docs/windows.md). |
| Claude Code | Supported | MCP config + lifecycle hooks. |
| Codex | Supported | MCP config + lifecycle hooks; no automatic true session-end hook, so run `engram finalize-session` when you need a final summary/handoff. |
| OpenCode | Supported | Remote MCP config + generated TypeScript plugin. |
| Cursor | Supported | MCP config + lifecycle hooks. |
| Gemini CLI | Supported | MCP config + lifecycle hooks. |
| Oh My Pi / OMP | Supported | Use `--client omp` / `--agent omp` (or `oh-my-pi`) for native `.omp` MCP config + TypeScript extension. |
| Pi | Supported | Generated `~/.pi/agent/extensions/engram.ts` extension provides lifecycle capture and an HTTP MCP bridge; use `install-hooks --agent pi --apply`. |
| Claude Desktop | MCP-only | Uses `mcp-remote`; no lifecycle hooks. |
| OpenClaw | Supported | MCP config + native plugin lifecycle hooks. |
| Antigravity CLI | Supported | MCP config (`serverUrl`) + lifecycle hooks (`agy` alias). |
| Grok Build CLI | Hooks | Lifecycle hooks via `install-hooks --agent grok` (`~/.grok/hooks/engram.json`, Grok-specific hook bundle, native `--agent grok`). Capture works; no handoff injection — Grok ignores `SessionStart` stdout, so recover handoffs via MCP `memory_handoff_accept`. |
| VS Code Copilot | MCP-only | `.vscode/mcp.json` for Copilot agent mode; no lifecycle hooks (Copilot does not expose them yet). |
| LLM/auth providers | Supported | Anthropic, OpenAI, OpenAI OAuth/Codex, GitHub Copilot, Gemini, OpenCode Zen/Go, OpenAI-compatible endpoints, and generic OIDC device auth for native hooks. |
| Embedding providers | Supported | OpenAI, Voyage, and Google Gemini. |

## What it is

LLM coding agents lose all context when a session ends. engram
gives them a shared, persistent wiki: every prompt, tool call, and
decision is captured automatically; when a session ends, the relevant
pages get rewritten as a coherent narrative; when the next agent
starts (Claude Code, Codex, OpenCode, …) it sees a handoff with
"where you left off" already prepended.

The wiki is plain markdown in a git repo - `grep`-able, openable in
Obsidian, backed up with `rsync`. No vector database to babysit, no
`write_note` ceremony, no manual context-loading. The full design is
in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the influences and
priors are at the [bottom](#influences-and-prior-art).

## Key features

- **Zero-friction capture.** Lifecycle hooks fire-and-forget every
  prompt + tool call + session boundary. You never type `write_note`.
- **Cross-agent handoffs.** Quit Claude Code mid-task, start Codex
  in the same directory hours later - the next agent sees a
  "where you left off" block before its first prompt.
- **Per-project isolation by construction.** Each project lives at
  `<wiki_root>/<workspace_id>/<project_id>/…` keyed by stable UUIDs.
  Workspace defaults to `"default"`. Project is derived from `$cwd`:
  CLI subcommands (`bootstrap`, `write-page`, `lint`, …) walk to the
  main git repo root so all worktrees of the same repo share one
  project identity; the hook router defaults to `basename($cwd)` and
  can opt into the repo-root rule. Drop a
  [`.engram.toml` marker file](docs/marker-file.md) in any
  ancestor directory to override either field explicitly — perfect for
  multi-client consultancies, work/personal split, mono-repos, or
  linked git worktrees.
  Same page path can exist in two projects without collision; a
  rename is one column update; a purge is one `rm -rf`.
- **Karpathy-style LLM wiki.** Pages are compiled from observations
  at session-end (or PreCompact; Codex can use `engram finalize-session`
  for a manual final close), not retrieved over raw logs.
  Supersession chain + git-versioned markdown means you can
  time-travel with `engram checkpoints`, `restore-page`, or raw `git log`.
- **Built-in `/web` browser.** Read-only HTML UI for the wiki -
  project list, folder tree, FTS5 search, markdown rendering, dark
  mode. Mounted on the same axum server as MCP.
- **Multi-agent + multi-machine ready.** Supported clients: Claude
  Code, Codex, OpenCode, Cursor, Claude Desktop (via `mcp-remote`),
  Gemini CLI, Antigravity CLI, Grok Build CLI, OpenClaw, Oh My Pi / OMP
  (`omp` / `oh-my-pi`), Pi via generated bridge extension, and VS Code GitHub
  Copilot agent mode
  (MCP-only, workspace `.vscode/mcp.json`).
  Server runs local (loopback) OR on a homelab box (LAN/VPN/cloud)
  with bearer-token auth. Shared servers can opt into
  [`[auto_scope]` modes](docs/auto-scope.md) for per-user or
  session-aware current-project routing.
- **Thin-client CLI.** `engram status`, `bootstrap`, `checkpoints`,
  `restore-page`, `purge-project`, `rename-project`, `move-project`,
  `audit-contamination`, `lint`, `curator`, `auto-improve`,
  `auto-improve-report`, `pending-writes`, `embed`, `forget-sweep`, `backup` are
  all HTTP clients of the running server - never touch SQLite or
  wiki files directly. `status` also reports passive LLM/embedding
  provider health from the last real provider call. Server is the
  single source of truth. `finalize-session` is the exception: it reads the
  local SQLite index only to find matching open sessions, then posts synthetic
  `session-end` hooks back to the server.
- **LLM is opt-in.** Zero-LLM mode still gives you FTS5 search +
  rule-based summarisation. Add a provider when you want consolidated
  pages, lint contradictions, or staged auto-improvement proposals.

## Use cases

- **"Quit at 4 PM, pick up at 9 AM in a different agent."** The
  classic. SessionStart hook in the next supported hook client prepends a
  typed handoff with open questions, next steps, and a session summary. Grok
  captures lifecycle events but ignores SessionStart stdout, so ask it to call
  `memory_handoff_accept` when resuming from a handoff.
- **"What did we decide about X six weeks ago?"** Type
  `memory_query X` from the agent (or `engram search X` from a
  terminal) - FTS5 over the wiki. Pages are LLM-consolidated, so
  the hit is a coherent decision page, not a raw chat log.
- **"Remember this permanently."** When something is worth keeping
  beyond auto-captured session logs - a decision, a convention, a
  gotcha - tell the agent "save a permanent note that we standardised
  on Postgres for X" or "annotate this as a project rule" and it calls
  `memory_write_page` to write a durable, git-versioned wiki page. From
  a terminal it's `engram write-page --path decisions/0007-db.md
  --body $'# Standardised on Postgres\n\n...' --pinned`. `--pinned`
  exempts it from the decay sweep; the H1 on the first line of
  `--body` becomes the page title (omit `--title` — it's still
  accepted, but LLM callers trip over JSON-escaping their way through
  it, see issue #67). Unlike a handoff (single-use) or an
  auto-synthesised session page (rewritten on consolidation), a
  write-page note is yours: it shows up in `memory_query`, renders in
  `/web`, and stays until you change it. For standing preferences that
  apply to *every* project (tech choices, code style, durable personal
  rules), pass `scope: "global"` — the page lands in the reserved
  `_global` scope and default `memory_query` calls union it into every
  project's results as `global_scope_hits`.
- **"This new project has months of history before engram."**
  `cd /path/to/my-project && engram bootstrap` collects
  `git log`, README, `docs/`, module headers, project rules and
  one-shot-summarises them into seed wiki pages. Future sessions
  build on top.
- **"What durable lesson did that session teach?"**
  When an LLM provider is configured, engram runs a background
  auto-improvement scheduler for newly completed sessions in every project. It
  records proposed wiki edits in the pending-writes audit trail, then approves
  them immediately through the normal wiki write path by default. Scheduler ticks
  are non-overlapping: if reviewing all projects takes longer than the interval,
  the next tick is delayed until the current one finishes. Scheduling and
  approval are separate: set `[auto_improve.scheduler] enabled = false` to stop
  automatic review, or set `[auto_improve] require_approval = true` to keep both
  scheduled and manual proposals pending for human review. `engram
  auto-improve --session-id <uuid>` and MCP `memory_auto_improve` remain
  available for manual catch-up or targeted reruns. `engram
  auto-improve-report --workspace <w> --project <p>` returns a read-only
  telemetry report for recent auto-improvement outcomes without staging or
  creating proposals; add `--stage` to create one pending report page for
  audit/approval. See
  [`docs/auto-improve-eval-gates.md`](docs/auto-improve-eval-gates.md) for
  example executable eval scorers.

  Existing installs do not need per-project migration. The scheduler initializes
  a per-project first-run watermark so historical sessions are not reviewed
  automatically on upgrade, then records per-session claims so failed scheduled
  reviews do not retry forever; use manual auto-improve for old sessions or
  failed scheduled sessions you want to catch up. Older configs may still contain
  an `[auto_improve] mode = ...` line; current engram ignores that legacy key,
  so you can remove it when convenient.
- **"What housekeeping should I consider?"**
  `engram curator` runs a no-LLM, rule-based maintenance report over cold
  episodic pages, stale slots, duplicate exact normalized titles, and dangling
  cross-project links. It is report-only unless `--stage` is passed; staging
  queues one report page for approval and still performs no maintenance actions
  itself.
- **"Run one engram for the whole household."** Stand the server
  up on a homelab box at `0.0.0.0:49374` with a bearer token; every
  laptop/desktop talks to it. Per-cwd routing keeps each project's
  pages cleanly separated; the `/web` UI is reachable from a
  browser anywhere on the LAN.
- **"Audit what landed before sharing with a teammate."** Browse
  the wiki at `http://<server>:49374/web` - HTTP Basic dialog if
  auth is on, paste the token as password. Per-project tree view,
  rendered markdown, supersession chain visible per page.
- **"Undo one bad page edit without rolling back the whole server."**
  `engram checkpoints` shows recent wiki commits, then
  `engram restore-page --path notes/foo.md --from <rev>` restores that one
  markdown file and reindexes it into SQLite. Full `backup` / `restore` is
  still the answer for DB-only state such as sessions, observations, handoffs,
  users, audit rows, and embeddings.
- **"Drop an experiment, keep the rest."**
  `engram purge-project --project experimental --confirm`.
  Atomic: that project's DB rows cascade away, its wiki subdir gets
  `rm -rf`'d, every sibling project is untouched by construction.

## Quick start

You need: an agent CLI (Claude Code, Codex, OpenCode, OMP, Cursor,
Antigravity CLI, Grok Build CLI, or anything else that speaks MCP).

The default quick-start has **no authentication** - the server binds
to loopback only, so on a single-user laptop nothing else can reach
it. Adding a bearer token is a one-line change once you're ready to
expose the server on the LAN; see [Security](#security) below.

### macOS (Apple Silicon)

```bash
# 1. Download the release archive and extract it to a stable location.
mkdir -p ~/Applications/engram && cd ~/Applications/engram
curl -fsSL -O https://github.com/semantic-craft/engram/releases/latest/download/engram-macos-aarch64.tar.gz
tar -xzf engram-macos-aarch64.tar.gz
# `curl` downloads are not Gatekeeper-quarantined. If you downloaded via a
# browser instead, clear the quarantine flag once:
#     xattr -d com.apple.quarantine ./engram

# 2. Initialise the data dir (defaults to
#    ~/Library/Application Support/engram; override with ENGRAM_DATA_DIR)
#    and start the server on loopback. Prefix `engram serve` with provider
#    env vars (ENGRAM_LLM_PROVIDER / ANTHROPIC_API_KEY, ENGRAM_EMBEDDING_PROVIDER
#    / OPENAI_API_KEY, …) when you want them; omit them for zero-LLM mode —
#    FTS5 search still works without any keys.
./engram init
./engram serve --transport http --bind 127.0.0.1:49374

# 3. In a second terminal, wire your agent CLI. `install-hooks`
#    auto-discovers the bundled hooks/ directory beside the binary. Re-run
#    with `--agent codex`, `--agent opencode`, `--agent gemini-cli`,
#    `--agent omp`, `--agent oh-my-pi`, `--client cursor`, etc. for
#    additional agents; full list in docs/install.md.
cd ~/Applications/engram
./engram install-mcp   --client claude-code --apply
./engram install-hooks --agent  claude-code --apply
```

That's it. Start a Claude Code session as usual - every prompt and tool
call now lands in engram, and the next session you open in this project
will see a handoff with where you left off. Keep the extracted `engram`
at a stable path; the hook commands reference it, so re-run
`install-hooks` if you move it. See [`docs/macos.md`](docs/macos.md) for
source builds and hook-platform notes.

### Windows (x86_64)

```powershell
# 1. Download + extract into a stable path.
$Dest = "$env:LOCALAPPDATA\engram"
New-Item -ItemType Directory -Force $Dest | Out-Null
Invoke-WebRequest `
    -Uri "https://github.com/semantic-craft/engram/releases/latest/download/engram-windows-x86_64.zip" `
    -OutFile "$env:TEMP\engram.zip"
Expand-Archive "$env:TEMP\engram.zip" -DestinationPath $Dest -Force
Get-ChildItem "$Dest\engram.exe" | Unblock-File

# 2. Initialise the data dir and start the server on loopback.
& "$Dest\engram.exe" init
& "$Dest\engram.exe" serve --transport http --bind 127.0.0.1:49374

# 3. In a second terminal, wire your agent CLI.
& "$Dest\engram.exe" install-mcp   --client claude-code --apply
& "$Dest\engram.exe" install-hooks --agent  claude-code --apply
```

Keep the extracted `engram.exe` at a stable location because
`install-hooks` reads its path from the running binary; re-run
`install-hooks` if you move it. See [`docs/windows.md`](docs/windows.md)
for the native hook command details and harness caveats.

The `install-mcp` / `install-hooks` commands use
`ENGRAM_SERVER_URL` / `ENGRAM_AUTH_TOKEN` when set; otherwise
they default to `http://127.0.0.1:49374` (matching the server above)
and no bearer token. If hooks are installed after an engram MCP
entry already exists, `install-hooks` reuses that endpoint so a remote
MCP setup cannot silently regenerate loopback-only hooks. Both commands
are idempotent - re-runs replace engram's entry, preserve every
other server / hook you have configured, and write a timestamped
`.bak-<ts>` next to the file before each modifying write. `install-hooks`
auto-discovers the bundled `hooks/` directory beside the binary, so a
fresh release archive ships updated hooks. Drop `--apply` to print the
snippet instead of mutating.
If your agent often starts inside repository subdirectories or linked
worktrees, add `--project-strategy repo-root` to `install-hooks` so captures
collapse to the main git repo name; see [`docs/install.md`](docs/install.md)
and [`docs/marker-file.md`](docs/marker-file.md) for details.

Thin-client commands such as `engram status` and `engram bootstrap`
default to the loopback server, so with the local quick start above no
`ENGRAM_SERVER_URL` override is needed.

To remove engram later, run `engram uninstall --apply` from the
same host environment. It removes engram-owned config entries, instruction
blocks, default-root managed skill files, and generated plugin files only after
matching their engram signatures; custom skill roots installed with
`--target-dir` are cleaned up manually. Use `--mcp-url` if you installed MCP
with a custom endpoint, and `--mcp-name` only when you need to narrow removal to
one matching entry.

### Install Notes

- **Windows:** run the native `engram.exe` from PowerShell/cmd. Native
  Claude Code uses Claude exec form with a real `engram.exe` by default, with
  `ENGRAM_HOOK_PLATFORM=windows-bash` available for Git Bash `.sh` hooks and
  older Claude Code builds; other script-hook agents use PowerShell defaults.
  See [`docs/windows.md`](docs/windows.md).
- **Remote server:** set `ENGRAM_SERVER_URL=http://<server-ip>:49374`
  and `ENGRAM_AUTH_TOKEN=<token>` on the client before installing
  MCP/hooks. Explicit `--server-url` flags still work, but are no longer
  required when the env vars are set. Any non-loopback server should use
  bearer auth.
- **Upgrades:** download the newer release archive and extract it over the
  previous install, keeping the same path, then rerun
  `engram install-hooks --agent <agent> --apply` so the hook commands point at
  the refreshed binary and bundled hooks. Remote/homelab servers must still be
  redeployed separately. Existing project prompt files keep working. Refresh the
  managed engram routing package (`engram install-instructions`, or
  `--target AGENTS.md` for AGENTS-based projects) when you want new tool
  guidance. The refresh writes the slim markered snippet and managed Agent
  Skills from the same binary-owned assets.

For Codex, OpenCode, OMP, Cursor, Claude Desktop, Gemini CLI, Antigravity CLI,
Grok Build CLI, OpenClaw, VS Code Copilot, curl-based hook installs, source builds,
CLI env vars, and the full subcommand reference, see [`docs/install.md`](docs/install.md).

## Security

Loopback-only (`127.0.0.1:49374`) with no auth is the default because
it is safe for a single-user laptop: no process outside the machine can
reach the server.

Enable bearer auth when the server is exposed beyond loopback, when
untrusted local processes share the machine, or when the data dir holds
sensitive project history:

```bash
TOKEN=$(engram generate-auth-token)

# Bind beyond loopback and require the token. Set ENGRAM_ALLOWED_HOSTS to
# guard against DNS rebinding. (Both can also live in config.toml as
# [auth].bearer_token and allowed_hosts.)
ENGRAM_AUTH_TOKEN="$TOKEN" \
ENGRAM_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1" \
  engram serve --transport http --bind 0.0.0.0:49374

engram install-mcp   --client claude-code --apply \
    --server-url "http://<server-ip>:49374/mcp" --auth-token "$TOKEN"
engram install-hooks --agent  claude-code --apply \
    --server-url "http://<server-ip>:49374" --auth-token "$TOKEN"
```

Bearer auth protects `/mcp`, `/hook`, `/handoff`, `/admin/*`, and
`/web/*`. Browser access to `/web` uses HTTP Basic auth with the token
as the password. Non-loopback binds should also set
`ENGRAM_ALLOWED_HOSTS` to guard against DNS rebinding.

For shared servers where each developer should authenticate their own hook
writes, native Claude Code hooks can use a stored OIDC device token instead of
embedding a shared static token:

```bash
engram auth login oidc-device \
    --issuer "https://issuer.example.com/realms/team" \
    --client-id "engram-cli"

engram install-hooks --agent claude-code --apply \
    --server-url "http://<server-ip>:49374"
```

OIDC hook auth requires the native `engram hook ...` command path, which the
native release binary and source installs use by default. Thin-client HTTP
commands such as `engram status`
and `engram search` also use the stored OIDC access token when no static
`ENGRAM_AUTH_TOKEN` / `[auth].bearer_token` is configured; the static bearer
still wins when present. This is for OIDC-aware gateways/bridges; native
engram server auth still accepts static root bearer / DB-user tokens, and
`/admin/*` remains root-only unless a gateway translates accepted OIDC auth into
upstream auth that engram accepts.

OIDC/Keycloak session ids are login-provider sessions, not engram agent
sessions. Shared servers that rely on `[auto_scope]` session isolation still
need explicit `workspace` + `project` / `scopes`, or a bridge that forwards the
real lifecycle-hook session id on MCP requests.

**Want HTTPS?** engram deliberately does not terminate TLS itself —
the right answer is a battle-tested reverse proxy in front of it.
[`docs/https-via-proxy.md`](docs/https-via-proxy.md) is the deployment
guide, covering Caddy (Let's Encrypt or internal CA) and Cloudflare
Tunnel (no open ports). A reverse proxy is recommended once you turn on
multi-user or bind beyond loopback. The Quick Start happy path of
single-user on loopback doesn't need TLS — that case is called out
explicitly in the guide so you don't add ceremony where it doesn't earn
its keep.

**Multi-user attribution (v0.8, optional).** When more than one human
shares a server, engram can attribute each write to a named user.
The bearer token continues to authenticate at the wire level; users
created via `engram user add` get their own tokens that resolve to
their identity in audit logs (and, in subsequent milestones, page
frontmatter + the web UI). Data stays single-tenant — there is no
per-page RBAC — but once `[auth].token_pepper` enables multi-user
mode, every `/admin/*` endpoint requires the root token, including
status/search/read-page and user-management routes. Existing single-user installs
are not affected unless you opt in by setting `[auth].token_pepper`
(auto-generated for new installs by `engram init`). See
[`docs/users.md`](docs/users.md) for the full walkthrough and the
four-rung auth ladder.

## Using Memory

Day to day, you mostly do not think about engram. Lifecycle hooks
capture prompts, tool calls, compaction checkpoints, and session
boundaries. SessionStart hooks fetch pending handoffs before your first
prompt in the next agent.

Useful entry points:

- Ask "where did we leave off?" to continue from the pending handoff.
- Ask "have we discussed X?" or "search memory for Y" to query the wiki.
- Ask "catch me up" for a prose digest of recent project activity.
- Run `engram bootstrap` once when adopting engram in an existing
  project with months of history.
- Start the server with `--enable-web` and visit `/web` for a read-only
  browser view of the markdown wiki. `--enable-web` also mounts a
  read-only JSON frontend API at `/api/v1` (workspaces, projects, pages,
  recent, briefing, search) so custom web UIs can read the memory without
  opening SQLite or wiki files directly:

  ```text
  GET  /api/v1/workspaces
  GET  /api/v1/projects?workspace=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/pages
  GET  /api/v1/workspaces/{workspace}/projects/{project}/pages/{path}
  GET  /api/v1/workspaces/{workspace}/projects/{project}/recent?limit=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/briefing?limit=...
  GET  /api/v1/workspaces/{workspace}/overview?limit=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/overview?limit=...
  GET  /api/v1/search?q=...&workspace=...&project=...&limit=...
  POST /api/v1/search   { "q": "...", "scopes": [{ "workspace": "...", "project": "..." }] }
  ```

  `overview` bundles the open handoff + briefing + memory-health for a workspace
  or project in one call (the data a project overview screen needs).

  **Full integration guide:** see [`docs/frontend-api.md`](docs/frontend-api.md)
  for auth setup, response schemas, error model, limits/pagination,
  custom-UI hosting, a worked `fetch`/`curl` example, and the canonical
  source-of-truth files. Read that first if you're building a frontend.

  To serve your own static frontend instead of the built-in UI, point
  `--web-ui-dir` at the frontend's build output (same-origin with
  `/api/v1`, `/mcp`, `/admin/*`, so the existing auth applies):

  ```bash
  engram serve --transport http --bind 127.0.0.1:49374 \
    --enable-web --web-ui-dir ../engram-ui/dist
  ```

  A reference implementation — a SolidJS knowledge browser with
  screenshots and e2e tests — lives at
  [djalmajr/engram-ui](https://github.com/djalmajr/engram-ui).

  The early-stage Tauri desktop client lives at
  [`apps/desktop`](apps/desktop). It is co-located in this repository for
  unified management, keeps its own Cargo workspace, and talks to the daemon
  through the same public HTTP/MCP surfaces.

  Richer products such as import/migration pipelines and write-capable
  browser chat/editors should live as optional companion crates or projects
  that call engram's public HTTP/MCP surfaces. The first implemented
  companion is the standalone OMC wiki importer at
  [`companions/engram-importer`](companions/engram-importer), which is
  intentionally not a root workspace member and is not included in root
  `cargo test --workspace`. See
  [`docs/companion-crates.md`](docs/companion-crates.md) for the boundary.

  When a reverse proxy hosts engram under a URL subpath, set
  `--base-path` (or `ENGRAM_BASE_PATH`) so every HTTP surface moves
  together. Example: `--base-path /wiki` serves MCP at `/wiki/mcp`, hooks at
  `/wiki/hook`, the API at `/wiki/api/v1`, and the default browser at
  `/wiki/web`. Set `--web-slug /` if you want the browser or custom SPA at
  `/wiki` itself.

Install the managed routing package once so agents proactively call the
right MCP tool for those prompts:

```bash
engram install-instructions
```

That command writes or updates the slim `<!-- engram:start -->` block and
the managed engram Agent Skills that carry the detailed routing guidance.
See [`docs/usage.md`](docs/usage.md) for handoff examples, proactive query
routing, bootstrap details, web UI screenshots, and the raw-wiki inspection
commands. CLI URL/auth configuration lives in
[`docs/install.md`](docs/install.md#configuring-the-cli-url-and-auth).

## LLM Providers

engram runs without an LLM: hooks still capture sessions, search uses
FTS5, and summaries fall back to rule-based output. Add an LLM provider
when you want LLM consolidation (on PreCompact, on demand via
`memory_consolidate`, or opt-in at session end with
`ENGRAM_CONSOLIDATE_ON_SESSION_END`), richer linting, and bootstrap.
Session end always writes a rule-based summary page + handoff either way.

Recommended defaults:

| Provider | Default | Use when |
|---|---|---|
| `anthropic` | `claude-haiku-4-5` | Best default for consolidation quality and rule classification. |
| `anthropic-oauth` | `claude-sonnet-4-6` | Use a Claude Pro/Max subscription via `claude setup-token`, no API key. |
| `openai` | `gpt-5.4-mini` | Cheaper and faster hosted option. |
| `openai-oauth` | `gpt-5.5` | ChatGPT Pro/Plus/Codex backend via `engram auth login openai-oauth`; no Platform API key. |
| `copilot` | `gpt-5.5` | GitHub Copilot Chat backend via `engram auth login copilot` or `COPILOT_GITHUB_TOKEN`; requires a Copilot subscription. |
| `gemini` | `gemini-2.5-flash` | Google-hosted option with a generous free tier. |
| `openai-compat` | no default | OpenRouter, Ollama, vLLM, LM Studio, and other compatible endpoints. |

`openai-oauth` stores a refresh token in `<data_dir>/auth.json` and talks to
the ChatGPT/Codex Responses backend, not `api.openai.com`. Run
`engram auth login openai-oauth` so the token lands in the same data dir the
server uses.

`anthropic-oauth` hits the same `/v1/messages` endpoint as `anthropic` but
authenticates with an OAuth bearer token instead of an API key. Run
`claude setup-token` once, then set `ENGRAM_LLM_PROVIDER=anthropic-oauth` and
`ANTHROPIC_OAUTH_TOKEN=<token>` (or `CLAUDE_CODE_OAUTH_TOKEN`, which `claude
setup-token` writes automatically). No `ANTHROPIC_API_KEY` is needed.
**⚠️ Unofficial and against Anthropic's usage policies — use at your own risk;
it may get your account rate-limited or banned. See
[the warning in `docs/install.md`](docs/install.md#anthropic-via-claude-subscription-oauth).**

`copilot` stores a GitHub user token in the same auth file, exchanges it for a
short-lived Copilot API token via GitHub's `/copilot_internal/v2/token`, and
uses the Copilot Chat endpoint with `vscode-chat` integration headers. You can
also set `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN` on the server.

> [!TIP]
> **For the OAuth/subscription backends (`anthropic-oauth`, `openai-oauth`,
> `copilot`), pick a small, fast model** via `ENGRAM_LLM_MODEL` — e.g.
> `claude-haiku-4-5` or `gpt-5-mini`. engram's LLM work (consolidation,
> lint, explore) is summarisation, not hard reasoning, so a Haiku/mini-class
> model is plenty and is much easier on subscription rate limits. Save the
> high-effort thinking models for your coding agent.

> [!TIP]
> **On a local engine (Ollama, vLLM, LM Studio, llama.cpp) with
> `openai-compat`, if consolidation fails on large sessions** with
> `did not contain a JSON object` or `serde: unknown variant`, set
> `ENGRAM_LLM_COMPAT_STRICT=true`. It sends `response_format=json_schema`
> (strict) so capable engines constrain output to the schema. If the strict
> raw call fails, engram falls back to the default tolerant parser. Off by
> default.

Embeddings are optional and separate from the LLM provider. Set
`ENGRAM_EMBEDDING_PROVIDER=openai`, `voyage`, `google`, or `gemini` when
you want vector reranking in addition to FTS5 + graph-neighbor retrieval.

See [`docs/install.md#llm-provider-tiers`](docs/install.md#llm-provider-tiers)
for env vars and Ollama/OpenRouter examples, and
[`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md)
for the empirical model comparison.

## Architecture

One Rust binary runs an MCP/HTTP server and owns one data directory:

```text
<data_dir>/
├── wiki/    # markdown source of truth, git-versioned
├── raw/     # immutable session log archive
├── db/      # SQLite indexes, including FTS5 and embeddings
├── models/  # reserved for local embedding models
└── logs/    # rolling tracing output
```

Hooks POST observations to the server. The server serializes writes
through one SQLite writer, compiles session observations into markdown
pages, and serves retrieval through FTS5, graph-neighbor RRF, optional
vector RRF, and bounded raw-observation fallback.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the data-flow
diagram, crate breakdown, schema notes, and invariants.

## Docs

| File | What it is |
|---|---|
| [`docs/install.md`](docs/install.md) | **Installation cookbook.** Every agent CLI, every alternative (curl, source build, no-auth), and the server-on-a-different-machine (homelab/LAN) walkthrough. Read after the Quick start if your setup doesn't match the happy path. |
| [`docs/usage.md`](docs/usage.md) | Handoffs, proactive memory queries, slim routing snippet + managed Agent Skills, migration from other memory tools, web UI, raw-wiki inspection, and rules-vs-facts workflow. |
| [`docs/marker-file.md`](docs/marker-file.md) | `.engram.toml` workspace/project routing for multi-client trees, mono-repos, worktrees, and work/personal separation. |
| [`docs/auto-scope.md`](docs/auto-scope.md) | `[auto_scope]` modes for shared servers: default single-slot routing, session-aware isolation, and multi-user `per_actor` behavior. |
| [`docs/macos.md`](docs/macos.md) | macOS install paths: native release binary (recommended), source build, hook-platform notes, and current macOS limitations. |
| [`docs/windows.md`](docs/windows.md) | Windows install modes: prebuilt native release zip, native source builds, and current hook/MCP harness caveats. |
| [`docs/mcp-install.md`](docs/mcp-install.md) | Per-client MCP and lifecycle notes (Cursor, Claude Desktop, Gemini CLI, Antigravity CLI, OpenClaw, OMP, VS Code Copilot). |
| [`docs/users.md`](docs/users.md) | **Multi-user attribution (v0.8).** Four-rung auth ladder, `engram user add/list/expire/revive/rotate-token` walkthrough, backward-compat migration for pre-v0.8 installs, token storage rationale. |
| [`docs/https-via-proxy.md`](docs/https-via-proxy.md) | **HTTPS via a reverse proxy.** When you need TLS (multi-user, non-loopback) and when you don't (loopback / stdio). Reverse-proxy recipes for Caddy + Let's Encrypt, Caddy + internal CA (LAN-only), Cloudflare Tunnel (no open ports), external cert files, and nginx. The "thinking you're secure when you're not" failure modes explicitly called out. |
| [`docs/lifecycle-ops.md`](docs/lifecycle-ops.md) | **Read before running purge / rename / backup / restore / reset / reindex / restore-page.** Safety matrix for state-touching commands, per-project disk layout (how isolation actually works), checkpoint-based page recovery, and operator workflows for "fresh start", "snapshot before risky op", "drop one project", and rebuilding SQLite from wiki files. |
| [`docs/auto-improvement-loop.md`](docs/auto-improvement-loop.md) | Auto-improvement design notes: Hermes-inspired scheduled review, auto-approval default, manual review opt-in, pending proposal storage, and curator work. |
| [`docs/companion-crates.md`](docs/companion-crates.md) | Boundary and implementation plan for optional companion projects, including the standalone importer at [`companions/engram-importer`](companions/engram-importer), without widening core engram. |
| [`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md) | Empirical notes behind the recommended LLM defaults. |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Operational summary: data flow, crate layout, cross-cutting invariants, schema. |
| [`docs/design-decisions.md`](docs/design-decisions.md) | The full v1 spec. |
| Research docs under `docs/` | Karpathy LLM Wiki notes, Hermes Agent, agentmemory / basic-memory / cognee deep-dives, lessons-learned from upstream issues. |

## Influences and prior art

- **[Karpathy LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)** - the compile-not-retrieve pattern.
- **[agentmemory](https://github.com/rohitg00/agentmemory)** - most of the right ideas; this project is the Rust successor.
- **[basic-memory](https://github.com/basicmachines-co/basic-memory)** - the markdown-on-disk source-of-truth model.
- **[cognee](https://github.com/topoteretes/cognee)** - pipeline composition and triplet embeddings.
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** - the self-improvement loop: post-turn review, approval gates, and curator boundaries.
- **[A-MEM](https://arxiv.org/abs/2502.12110)** - Zettelkasten-style atomic notes with link evolution.

## License

MIT - see [LICENSE](LICENSE).

## Acknowledgements

This codebase is being built collaboratively with Claude Code
(Anthropic Claude Opus 4.7) following the plan documented in
`docs/design-decisions.md`.
