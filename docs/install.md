# Installation cookbook

The [README quick-start](../README.md#quick-start) covers the happy
path: download the native binary for your platform and wire up Claude
Code. This page covers everything else:

- [Server on a different machine](#server-on-a-different-machine)
  (homelab, LAN box, remote server)
- [Configuring the CLI URL and auth](#configuring-the-cli-url-and-auth)
- [Configuring other agent CLIs](#configuring-other-agent-clis)
  (Codex, OpenCode, OMP, Pi, Cursor, Claude Desktop, Gemini CLI, Antigravity CLI, Grok Build CLI, OpenClaw, VS Code Copilot)
- [Installing hook scripts with the curl installer](#installing-hook-scripts-with-the-curl-installer)
- [Running the server and building from source](#running-the-server-and-building-from-source)
- [LLM provider tiers + self-hosted Ollama](#llm-provider-tiers)
- [Subcommand reference](#subcommand-reference)
- [Managed routing snippets and Agent Skills](#managed-routing-snippets-and-agent-skills)
- [Operating without auth](#operating-without-auth) (local-only)
- [Keeping engram up to date](#keeping-engram-up-to-date)

> **Shorthand.** Most snippets use `$TOKEN` and `homelab:49374`. If
> you're following along verbatim:
> ```bash
> export TOKEN=$(engram generate-auth-token)
> ```
> and replace `homelab` with `localhost` if the server runs on the
> same machine as the agent CLI.

engram ships as a native binary for macOS on Apple Silicon
(`engram-macos-aarch64.tar.gz`) and Windows x86_64
(`engram-windows-x86_64.zip`). Download the archive for your platform,
extract it, and run the `engram` (or `engram.exe`) binary directly. The
`hooks/` bundle, `config.default.toml`, and docs ship alongside the
binary; on Windows the command is `engram.exe`.

---

## Server on a different machine

When the engram server runs on another box on your LAN (a spare Mac, a
Windows machine, a headless host) and you use Claude Code / Codex / etc.
on a laptop:

### Server side (the LAN host)

Run `serve` bound to a non-loopback address, with a bearer token and a
Host-header allowlist. Set them in the environment (or in the data dir's
`config.toml`) before starting the server:

```bash
export ENGRAM_AUTH_TOKEN="$TOKEN"
export ENGRAM_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1"
export ENGRAM_LLM_PROVIDER=anthropic
export ANTHROPIC_API_KEY=sk-ant-...

engram serve --transport http --bind 0.0.0.0:49374
```

Keep the process alive with your platform's service manager (launchd on
macOS, a Windows service or Task Scheduler entry on Windows) so it
survives logout and reboots.

See [Security](../README.md#security) in the README for why
`ENGRAM_AUTH_TOKEN` and `ENGRAM_ALLOWED_HOSTS` are both
required for any non-loopback bind.

### Client side (the laptop)

```bash
export ENGRAM_SERVER_URL="http://<server-ip>:49374"
export ENGRAM_AUTH_TOKEN="$TOKEN"

engram install-mcp   --client claude-code --apply
engram install-hooks --agent  claude-code --apply
```

The CLI commands (`bootstrap`, `status`, `search`, `lint`, `auto-improve`,
`curator`, `pending-writes`, etc.) inherit the two env vars automatically. So do
`install-mcp`, `install-hooks`, and
`setup-agent`: with `ENGRAM_SERVER_URL` set, `install-mcp` derives the
`/mcp` endpoint and `install-hooks` uses the bare server origin.

After upgrading engram, refresh the managed routing package in existing
projects so Claude Code/OpenCode/Codex/Gemini pick up new tool guidance and
proactive retrieval rules. From an agent, ask "refresh the engram routing in
this project"; from the terminal, run `engram install-instructions` (or pass
`--target AGENTS.md` for non-Claude prompt files). The update is idempotent:
legacy long snippets between `<!-- engram:start -->` /
`<!-- engram:end -->` are replaced in place with the slim snippet, and
managed Agent Skills are installed or updated alongside it.

---

## Configuring the CLI URL and auth

The `engram` binary is a thin HTTP client. It never opens the wiki
or SQLite directly; state-touching commands go through the running
server, which is the sole writer.

Configuration is two optional environment variables:

| Variable | Default | When to set it |
|---|---|---|
| `ENGRAM_SERVER_URL` | `http://127.0.0.1:49374` | When the server runs somewhere other than the same machine, such as `http://192.0.2.10:49374`. |
| `ENGRAM_AUTH_TOKEN` | unset | When the server has bearer auth enabled. |

For a single-laptop loopback server, set neither variable. For a
remote or homelab server, put both in your shell rc or direnv file:

```bash
export ENGRAM_SERVER_URL="http://192.0.2.10:49374"
export ENGRAM_AUTH_TOKEN="<token>"
```

Explicit `--server-url` and `--auth-token` flags on `install-mcp`,
`install-hooks`, and `setup-agent` override the environment. That is
useful when you are generating config for a client that talks to a
different server than your default CLI target.

If you run `install-mcp --apply` first and later run `install-hooks --apply`
without env vars or flags, hooks reuse the existing engram MCP entry for
that agent when possible. This keeps remote MCP config and lifecycle capture
pointed at the same server instead of falling back to loopback.

`init`, `serve`, and `generate-auth-token` do not need these env vars because
they either create local files or start the server itself.

### Default project resolution (`--project-strategy`)

By default each session files memory under `basename(cwd)`. Because an agent
shell keeps its working directory between tool calls, a single
`mkdir sub && cd sub` reparents the rest of the session into a phantom project
named `sub`. To make every session for an install resolve its project from the
git repo root instead — collapsing subdirectories and worktrees — bake the
strategy into the hooks:

```bash
engram install-hooks --apply --agent claude-code --project-strategy repo-root
```

`--project-strategy` accepts `basename` (the default; bakes nothing, so existing
installs are unchanged) or `repo-root`. It works for every agent and delivery
path. A per-repo `.engram.toml` marker's own `project_strategy` / `project`
still take precedence — see
[the marker-file reference](marker-file.md#install-wide-default-no-marker).

---

## Configuring other agent CLIs

> `install-mcp --server-url` takes the MCP endpoint **including** `/mcp`
> (e.g. `http://homelab:49374/mcp`) — the rendered client config expects the
> full MCP URL. `install-hooks --server-url` takes the bare server **origin**
> (e.g. `http://homelab:49374`) — hook scripts append `/hook`, `/handoff`,
> etc. themselves.

Each agent CLI needs two things:

1. **MCP registration** - so the agent can call `memory_query`,
   `memory_recent`, `memory_handoff_accept`.
2. **Lifecycle hooks** - so the server auto-captures session events.
   Without this, the agent can still query memory but capture
   becomes manual.

Claude Desktop is MCP-only today. Claude Code, Codex, OpenCode, OMP,
Cursor, Gemini CLI, Antigravity CLI, Grok Build CLI, and OpenClaw have lifecycle capture paths through
`install-hooks`.

> **How hooks are wired.** Claude Code, Codex, Cursor,
> Gemini CLI, Antigravity CLI, and Grok Build CLI use shell/PowerShell hook
> scripts. `install-hooks --apply` stages the bundled scripts from the `hooks/`
> directory beside the binary into the engram data dir
> (`<data-dir>/hooks/<agent>/`) and writes the matching config in one step.
> On native Windows, Claude Code is the exception to the PowerShell default:
> it uses Claude exec form (`command` = real `engram.exe`, `args` = argv
> tokens for `hook --event ...`) by default. Set
> `ENGRAM_HOOK_PLATFORM=windows-bash` before `install-hooks` to opt back into
> Git Bash `bash -c` commands for the `.sh` scripts, including for older Claude
> Code builds that do not support exec form.
> OpenClaw, OpenCode, and OMP are different: they use generated
> TypeScript plugin/extension files, so no shell-script extraction is
> needed for those clients.

### OpenAI Codex

```bash
# MCP config (writes ~/.codex/config.toml):
engram install-mcp --client codex --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Hooks — stages the bundled scripts + writes the config:
engram install-hooks --agent codex --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Codex still does not expose a reliable true session-end hook. Its `Stop` hook is
captured as a turn/stop observation only; engram does **not** treat it as
SessionEnd. When you need the final session summary, handoff, and
auto-improvement eligibility for the current project, run:

```bash
engram finalize-session
# add --all to close every matching open Codex session in this workspace/project
```

### OpenCode

```bash
engram install-mcp --client opencode --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Plugin — `--apply` writes ~/.config/opencode/plugins/engram.ts directly.
# Drop `--apply` to print the plugin instead if you want to place it yourself.
engram install-hooks --agent opencode --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Restart OpenCode after installing or changing the plugin; plugins are
loaded at startup.

### Oh My Pi / OMP

```bash
engram install-mcp --client omp --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Extension — `--apply` writes ~/.omp/agent/extensions/engram.ts directly.
engram install-hooks --agent omp --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Restart OMP after installing or changing the extension; extensions are
loaded at startup. The engram CLI accepts `--client omp` (or
`--client oh-my-pi`) for MCP and `--agent omp` (or `--agent oh-my-pi`)
for hooks; both target OMP's native `.omp` integration surface.

### Pi

Pi does not read a native `mcp.json`. engram supports Pi through one
generated TypeScript extension at `~/.pi/agent/extensions/engram.ts`; the
same file captures lifecycle events and bridges engram's HTTP MCP tools into
Pi with `pi.registerTool`.

```bash
engram install-hooks --agent pi --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# `install-mcp --client pi` prints this guidance instead of writing mcp.json:
engram install-mcp --client pi --server-url "http://homelab:49374/mcp"
```

Restart Pi after installing or changing the extension. OMP / Oh My Pi remains
separate and continues to use `.omp` paths.

### One-shot setup with `setup-agent`

`install-hooks --apply` is the usual path. When you would rather stage the
scripts to an explicit directory and print the config in one shot, use
`setup-agent`:

```bash
engram setup-agent --agent claude-code --to ~/.engram/hooks \
    --host-prefix ~/.engram/hooks \
    --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

`--to` is where the bundled scripts are extracted; `--host-prefix` is the path
written into the rendered hook commands (set it when the extraction path and the
path the agent will execute differ).

### Cursor, Gemini CLI, Claude Desktop, OpenClaw, Antigravity CLI, Grok Build CLI, VS Code Copilot

See [**`docs/mcp-install.md`**](mcp-install.md) for the per-client MCP
config file path and snippet, or wire them up directly:

```bash
engram install-mcp --client cursor          --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

engram install-hooks --agent cursor         --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

engram install-mcp --client claude-desktop  --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

engram install-mcp --client gemini-cli      --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

engram install-hooks --agent gemini-cli     --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

engram install-mcp --client antigravity-cli --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

engram install-hooks --agent antigravity-cli --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

engram install-hooks --agent grok            --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

engram install-mcp --client openclaw        --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

engram install-hooks --agent openclaw       --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

engram install-mcp --client vscode-copilot  --apply --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"
```

Cursor, Gemini CLI, Antigravity CLI, and OpenClaw support both `install-mcp` and
`install-hooks`. Grok Build CLI is hook-only in engram's installer today:
`install-hooks --agent grok` captures lifecycle events, but Grok ignores
`SessionStart` stdout, so handoffs must be accepted through MCP with
`memory_handoff_accept` when resuming. Claude Desktop and VS Code Copilot are MCP-only here,
so you'll need to nudge the model to call `memory_query` /
`memory_handoff_accept` itself.
For clients with `install-hooks` support, the capture path handles
handoff injection at session start or the client's closest equivalent, except
for Grok's no-stdout SessionStart behavior (Antigravity CLI uses `PreInvocation`).

---

## Installing hook scripts with the curl installer

The release archive already ships the full `hooks/` bundle beside the
binary, so `install-hooks --apply` normally has everything it needs. When
you only use engram *from* a machine that doesn't run the server, and you
want just the shell hook scripts without extracting the whole archive, the
curl installer pulls them straight from GitHub for shell-hook agents:

```bash
curl -sSL https://raw.githubusercontent.com/semantic-craft/engram/main/scripts/install-hooks.sh \
    | bash -s -- --agent claude-code

# Then render the JSON config with a local `engram` binary, pointing
# --hooks-dir at the scripts the installer just staged:
engram install-hooks --agent claude-code \
    --hooks-dir "$HOME/.engram/hooks" \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

The curl script installer supports
`--agent claude-code|codex|cursor|gemini-cli|antigravity-cli|grok|opencode|openclaw|omp|oh-my-pi|pi`
and `--to <dir>`; `--help` prints the full flag list. OpenCode,
OpenClaw, OMP / Oh My Pi, and Pi do not need script extraction because
`install-hooks` generates TypeScript plugin/extension files for them
instead. For Pi, the generated extension also provides the MCP bridge.

This path is friction-free when you have curl + bash, don't want to keep
the full release tree around, and are a client of a homelab/remote engram
rather than running a local server.

---

## Running the server and building from source

Most users just download the release archive for their platform
(`engram-macos-aarch64.tar.gz` on Apple Silicon,
`engram-windows-x86_64.zip` on Windows), extract it, and run the binary:

```bash
cd ~/Applications/engram          # wherever you extracted the archive
./engram init                     # one-time data-dir setup
./engram serve --transport http \
    --bind 127.0.0.1:49374          # MCP + hook HTTP server
```

Build from source only when hacking on engram itself:

```bash
git clone https://github.com/semantic-craft/engram
cd engram
cargo build --release --workspace
./target/release/engram init                       # one-time
./target/release/engram serve --transport http \
    --bind 127.0.0.1:49374                            # MCP + hook HTTP server
```

Data dir defaults to `~/Library/Application Support/engram` on macOS and
the platform local-data directory on Windows, typically
`%LOCALAPPDATA%\engram`. Override with `ENGRAM_DATA_DIR=/path`.
To require bearer-token auth, set `ENGRAM_AUTH_TOKEN` in the
server's environment.

#### Optional serve flags

The `serve` subcommand also accepts:

| Flag | Env var | What it does |
|---|---|---|
| `--enable-web` | `ENGRAM_ENABLE_WEB=true` | Mount the read-only web browser + `/api/v1` JSON API. |
| `--base-path /wiki` | `ENGRAM_BASE_PATH` | Host the entire HTTP surface (`/mcp`, `/hook`, `/admin/*`, `/api/v1`, `/web`) under a configurable subpath — useful behind a reverse proxy sharing a hostname. `.` and `..` segments are rejected; unsafe chars cause a fallback to root with a warning. See [`docs/https-via-proxy.md`](https-via-proxy.md#hosting-under-a-subpath). |
| `--web-slug /web` | `ENGRAM_WEB_SLUG` | Where the web UI mounts within the base-path. Default `/web`; set to `/` to mount the UI at the base-path root. |
| `--web-ui-dir <path>` | `ENGRAM_WEB_UI_DIR` | Serve a custom SPA from `<path>` instead of the built-in browser. engram injects `<base href>` and `<meta name="engram-base-path">` so the SPA can build relative URLs and API calls under the configured prefix. |
| `--cors-allow-origin <origin>` | `ENGRAM_CORS_ALLOW_ORIGINS` (CSV) | Allow listed origins to call `/api/v1`. Layer is scoped only to that route — `/mcp`, `/hook`, `/admin`, and `/web` remain origin-locked. |

On macOS, see [`docs/macos.md`](macos.md) for the Apple Silicon
`aarch64` archive. On Windows, see [`docs/windows.md`](windows.md) for the
`engram-windows-x86_64.zip` archive and source-build path.
The short version: run the install commands from the same environment that
launches the agent. Native Claude Code uses Claude exec form with a real
`engram.exe` by default; other native Windows script-hook agents use
PowerShell `.ps1` defaults.

When run from source, `install-hooks` finds the bundled scripts in
the repo's `hooks/` automatically. Extracted release archives also
auto-discover the sibling `hooks/` bundle beside the `engram` binary:

```bash
./target/release/engram install-hooks --agent claude-code --auth-token "$TOKEN"
```

(No need for `setup-agent` in this case - the scripts already live
at the right host path.)

---

## LLM provider tiers

engram works in three intensity tiers:

| Tier | What you get | Env vars | Cost |
|---|---|---|---|
| **Zero-LLM** (default) | FTS5 search, rule-based session summaries, auto-handoffs from prompt + tool-call history | (none) | $0 |
| **+ LLM consolidation** | LLM rewrites session pages as coherent narratives; PreCompact checkpoints; LLM-driven contradiction lint | `ENGRAM_LLM_PROVIDER=anthropic` + `ANTHROPIC_API_KEY` | ~$0.01–0.05 / session |
| **+ Anthropic via subscription** | Same LLM features using a Claude Pro/Max subscription instead of an API key | `ENGRAM_LLM_PROVIDER=anthropic-oauth` + `ANTHROPIC_OAUTH_TOKEN` | Uses your Claude subscription |
| **+ ChatGPT/Codex OAuth** | Same LLM features using a ChatGPT Pro/Plus login instead of an OpenAI Platform key | `ENGRAM_LLM_PROVIDER=openai-oauth` + `engram auth login openai-oauth` | Uses your ChatGPT subscription |
| **+ GitHub Copilot** | Same LLM features using a GitHub Copilot subscription | `ENGRAM_LLM_PROVIDER=copilot` + `engram auth login copilot` or `COPILOT_GITHUB_TOKEN` | Uses your Copilot subscription |
| **+ Hybrid retrieval** | RRF over FTS5 + vector cosine similarity. Better recall on paraphrased queries | `ENGRAM_EMBEDDING_PROVIDER=openai` + `OPENAI_API_KEY` | ~$0.0001 / page on backfill |

### Recommended models (chosen as defaults)

If you set only the provider, engram picks a sensible default:

| Setting | Default | Why |
|---|---|---|
| `ENGRAM_LLM_PROVIDER=anthropic` | `claude-haiku-4-5` | **Recommended default.** Best balance of speed, restraint, and classification quality. Not a reasoning model. Consistently classifies durable project rules as `kind: rule`. |
| `ENGRAM_LLM_PROVIDER=anthropic-oauth` | `claude-sonnet-4-6` | Anthropic via Claude subscription. Run `claude setup-token` once; set `ANTHROPIC_OAUTH_TOKEN` (or `CLAUDE_CODE_OAUTH_TOKEN`). No `ANTHROPIC_API_KEY` needed. Same `/v1/messages` endpoint, Bearer token auth. |
| `ENGRAM_LLM_PROVIDER=openai` | `gpt-5.4-mini` | Cheaper + faster alternative. Same parse reliability; mild over-classification on thin sessions. |
| `ENGRAM_LLM_PROVIDER=openai-oauth` | `gpt-5.5` | ChatGPT/Codex backend. Run `engram auth login openai-oauth` once; engram stores the refresh token in `<data_dir>/auth.json` and refreshes access tokens automatically. |
| `ENGRAM_LLM_PROVIDER=copilot` | `gpt-5.5` | GitHub Copilot Chat backend. engram stores a GitHub user token in `<data_dir>/auth.json`, exchanges it for a short-lived Copilot API token, and refreshes before expiry. |
| `ENGRAM_LLM_PROVIDER=gemini` | `gemini-2.5-flash` | Google's hosted option with a generous free tier. engram disables Gemini 2.5 Flash's default dynamic thinking so hidden thought tokens do not truncate strict JSON. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| `ENGRAM_LLM_PROVIDER=opencode` | `claude-sonnet-4-6` | [OpenCode Zen/Go](https://opencode.ai) cloud API — OpenAI-compatible endpoint at `opencode.ai/zen/go/v1`. Set `OPENCODE_API_KEY` (key from `opencode.ai/auth`). Alias: `opencode-zen`. |
| `ENGRAM_EMBEDDING_PROVIDER=openai` | `text-embedding-3-small` (1536-dim) | 5× cheaper than `-3-large` with marginal recall loss. |
| `ENGRAM_EMBEDDING_PROVIDER=openai` + `ENGRAM_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1` | `openai/text-embedding-3-small` via [OpenRouter](https://openrouter.ai) | Reuses `LLM_API_KEY` or `OPENAI_API_KEY` with the OpenAI-compatible embedding client. |
| `ENGRAM_EMBEDDING_PROVIDER=voyage` | `voyage-3` (1024-dim) | Voyage's current general-purpose recommendation. |
| `ENGRAM_EMBEDDING_PROVIDER=google` / `gemini` | `gemini-embedding-001` (768-dim) | Google-hosted embeddings via `embedContent`. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |

> **What we don't recommend:** reasoning-mode models (Claude with extended
> thinking, GPT-o3, Gemini "thinking" variants) — they burn token budget on
> internal reasoning and hang or emit empty responses with the strict-JSON
> consolidation prompt. Turn reasoning off if you must use one.

### Anthropic via Claude subscription (OAuth)

> [!WARNING]
> **Unofficial and against Anthropic's usage policies — use at your own risk.**
> Anthropic provides no public OAuth API for the Claude Pro/Max subscription;
> this reuses the `claude setup-token` credential against `/v1/messages`, which
> is **not a supported or sanctioned integration**. Anthropic's terms reserve
> subscription (Claude Code) access for interactive use, and using it as an
> automated API backend may breach those terms and **could get your account
> rate-limited, flagged, or banned**. The header recipe is also undocumented
> and can change without notice. If you want a supported path, use the
> `anthropic` provider with a real Platform API key. We ship this purely as an
> opt-in convenience and make no guarantees about it.

`anthropic-oauth` is for Claude Pro/Max subscribers who want to use their
existing subscription instead of an Anthropic Platform API key. It hits the
**same** `/v1/messages` endpoint as the `anthropic` provider — only the auth
headers differ (Bearer token + `anthropic-beta: oauth-2025-04-20`).

```bash
# Obtain a token once using the Claude Code CLI:
claude setup-token

# Then export it (the CLI may also write CLAUDE_CODE_OAUTH_TOKEN automatically):
export ANTHROPIC_OAUTH_TOKEN=<paste token here>
export ENGRAM_LLM_PROVIDER=anthropic-oauth
engram serve --transport http --bind 127.0.0.1:49374
```

Both `ANTHROPIC_OAUTH_TOKEN` and `CLAUDE_CODE_OAUTH_TOKEN` are accepted;
engram checks `ANTHROPIC_OAUTH_TOKEN` first.

> [!TIP]
> **Pick a small, fast model.** engram's LLM work — session
> consolidation, lint, and explore — is summarisation/extraction, not hard
> reasoning, so a Haiku-class model is plenty: faster, cheaper, and far easier
> on subscription rate limits than Sonnet/Opus. Set e.g.
> `ENGRAM_LLM_MODEL=claude-haiku-4-5`. Save the high-effort thinking models
> for your actual coding agent.

### OpenAI OAuth / Codex

`openai-oauth` is for ChatGPT Pro/Plus/Codex accounts. It does **not** use
`OPENAI_API_KEY` and it does **not** call `api.openai.com`; requests go to the
ChatGPT/Codex Responses backend with a refreshable OAuth token.

Run the login once, then start the server with the provider set. `auth login`
writes the refresh token into the server's data dir, so run it against the same
data dir the server uses (pass `--data-dir` if you override the default):

```bash
engram auth login openai-oauth
export ENGRAM_LLM_PROVIDER=openai-oauth
engram serve --transport http --bind 127.0.0.1:49374
```

For a server on another machine, run the login on that host against the same
data dir.

Use `engram auth status` to check whether a token is present and
`engram auth logout openai-oauth` to remove it.

> [!TIP]
> **Pick a small, fast model.** Consolidation / lint / explore are
> summarisation tasks, not hard reasoning — a mini-class model is plenty and
> is much easier on subscription rate limits. Set e.g.
> `ENGRAM_LLM_MODEL=gpt-5-mini` (the `gpt-5.5` default works but is
> overkill for this workload). Reserve the high-effort reasoning models for
> your coding agent.

### GitHub Copilot

`copilot` uses a GitHub user token, then exchanges it for a short-lived Copilot
API token through `https://api.github.com/copilot_internal/v2/token`. The raw
GitHub token is never sent to `api.githubcopilot.com`.

Run the login once against the server's data dir, then start the server with the
provider set:

```bash
engram auth login copilot
export ENGRAM_LLM_PROVIDER=copilot
engram serve --transport http --bind 127.0.0.1:49374
```

For a server on another machine, run the login on that host against the same
data dir.

Non-interactive deploys can set `COPILOT_GITHUB_TOKEN` instead. engram also
accepts `GH_TOKEN` and `GITHUB_TOKEN`; prefer the explicit
`COPILOT_GITHUB_TOKEN` so you do not pass a broad token by accident.
Advanced users with a pre-minted Copilot API token can set
`GITHUB_COPILOT_API_TOKEN` and optionally `COPILOT_API_URL`.

`auth login copilot` defaults to GitHub Copilot's public device-flow client id.
Pass `--client-id` or set `ENGRAM_COPILOT_CLIENT_ID` if you operate your own
OAuth app.

### Self-hosted LLMs (Ollama / vLLM / LM Studio / OpenRouter)

Point the `openai-compat` provider at the local endpoint before starting the
server:

```bash
export ENGRAM_LLM_PROVIDER=openai-compat
export ENGRAM_LLM_BASE_URL=http://localhost:11434/v1
export ENGRAM_LLM_MODEL=qwen2.5-coder:14b
engram serve --transport http --bind 127.0.0.1:49374
```

There is no safe default model for `openai-compat`; the env var is
required. For OpenRouter (Kimi, DeepSeek, etc.):

```bash
export ENGRAM_LLM_PROVIDER=openai-compat
export ENGRAM_LLM_BASE_URL=https://openrouter.ai/api/v1
export ENGRAM_LLM_MODEL=moonshotai/kimi-k2.6
export LLM_API_KEY=sk-or-v1-...
```

Modern Ollama, vLLM, LM Studio, llama.cpp, and gateway endpoints may honour
OpenAI-style `response_format=json_schema`. If the tolerant default parser fails
with errors such as `did not contain a JSON object` or `serde: unknown variant`,
try strict compat mode:

```bash
export ENGRAM_LLM_COMPAT_STRICT=true
```

Strict mode is opt-in. engram sends the schema-constrained request first and
falls back to the tolerant parser only when that raw strict call fails.

---

## Subcommand reference

Most subcommands are thin HTTP clients of the running server, so start
`engram serve` first. A handful are pure-stdout or file-writing helpers that
need no server at all:

```bash
# Stateful — talk to the running server (status, search, backup, checkpoints,
# restore-page, audit-contamination, forget-sweep, lint, embed).
engram status --json
engram search "karpathy"
engram backup --to ~/engram-snapshot.tar.gz

# Server-free — pure-stdout / file-writing helpers
# (generate-auth-token, install-mcp, install-hooks, setup-agent, llm-test).
# Auth login writes into the data dir, so run it against the server's data dir.
engram generate-auth-token
engram install-mcp --client cursor
engram --help     # full subcommand tree
```

| Subcommand | Needs a running server? | What it does |
|---|---|---|
| `serve` | it *is* the server | Run the HTTP MCP server |
| `status` | yes | Counts, paths, derived-index diagnostics, and passive LLM/embedding provider health |
| `search "<query>"` | yes | Wiki search with FTS5 + graph/vector RRF |
| `write-page` | yes | Manual page write (atomic + indexed) |
| `backup --to` / `restore --from` | yes | Snapshot or restore the data dir |
| `checkpoints` / `restore-page` | yes | List wiki git checkpoints or restore one markdown page and reindex it |
| `audit-contamination` | yes | Read-only structural audit for likely cross-project contamination |
| `forget-sweep` / `lint` / `embed` | yes | Manual maintenance; sweep + lint also run on the server schedule by default |
| `commit -m "…"` | yes | Stage + commit the wiki tree |
| `reset --confirm` | yes | Wipe data (refuses while siblings alive) |
| `generate-auth-token` | no | Print a random hex bearer token |
| `auth login openai-oauth` | no (writes the server's data dir) | Store a ChatGPT/Codex OAuth refresh token for the optional `openai-oauth` LLM provider |
| `auth login copilot` | no (writes the server's data dir) | Store a GitHub token for the optional `copilot` LLM provider |
| `auth login oidc-device` | no (writes the developer data dir used by native hooks and thin-client CLI commands) | Store a per-developer OIDC device token for native hook authentication and HTTP CLI fallback auth |
| `install-mcp --client` | no | MCP-config snippet per client |
| `install-hooks --agent` | no | Stage hook scripts + write the hook config |
| `setup-agent --agent --to --host-prefix` | no | Extract bundled scripts + print config (one-shot) |
| `install-instructions [--target] [--print] [--no-skills]` | no (writes the agent prompt files) | Install or update the slim CLAUDE.md / AGENTS.md routing block and, by default, the managed engram Agent Skills |
| `install-skills [--scope] [--agent]` | no (writes the agent skill dirs) | Install or update only the managed engram Agent Skills |
| `uninstall --apply` | no (edits the local install) | Remove only engram-owned hooks, MCP entries, instruction blocks, managed skill files, and generated plugin files after content/marker validation. Use `--mcp-url` for custom MCP endpoints and `--mcp-name` only to narrow removal. |
| `llm-test --provider …` | no | Smoke-test an LLM provider |

### Managed routing snippets and Agent Skills

engram's routing install is agent-facing prompt packaging. It does not add a
runtime skill router, and `SKILL.md` files are not durable memory pages. The
wiki remains the durable source of truth.

`engram install-instructions` now writes two managed prompt artifacts by
default:

1. A slim instruction block in `CLAUDE.md`, `AGENTS.md`, or the file passed with
   `--target`. The block is bounded by `<!-- engram:start -->` and
   `<!-- engram:end -->`.
2. Managed engram Agent Skills containing the detailed tool-routing guidance.

Re-running the command is safe. If a project still has the old long engram
block between those markers, the refresh replaces that block in place with the
slim snippet, leaves unrelated instructions before and after it alone, and
writes a timestamped `.bak-*` backup before changing an existing file.
Managed skill files contain an engram ownership marker; same-name user skills
without that marker are preserved unless you explicitly force replacement.
`install-instructions --print` previews only the instruction snippet; run
`install-skills --print` when you want to preview the managed skill payloads.

`install-instructions` flags for skills:

| Flag | Meaning |
|---|---|
| `--no-skills` | Refresh only the markered instruction block. |
| `--skills-scope <scope>` | Choose project-local or user-global skill roots. Values: `project`, `global`. Defaults to `project`. |
| `--skills-agent <agent>` | Choose `.claude/skills`, `.agents/skills`, or both. Values: `claude-code`, `agents`, `both`. By default, `CLAUDE.md` targets imply `claude-code`, `AGENTS.md` targets imply `agents`, and both instruction files imply `both`. |
| `--skills-target-dir <dir>` | Write managed skill directories below an explicit root instead of inferring from scope and agent. |
| `--skills-force` | Replace unmanaged same-name skills during `install-instructions`; without it, they are left untouched and the command exits with an actionable error. |

Use `install-skills` when the instruction block is already right and only the
Agent Skill files need a refresh:

```bash
engram install-skills
engram install-skills --scope global --agent agents
engram install-skills --agent both --print
engram install-skills --target-dir .custom/skills --force
```

`install-skills` flags:

| Flag | Meaning |
|---|---|
| `--scope <scope>` | Install into this project or the current user's global skill roots. Values: `project`, `global`. Defaults to `project`. |
| `--agent <agent>` | Install into Claude Code's skill root, the cross-agent skill root, or both. Values: `claude-code`, `agents`, `both`. Defaults to `claude-code`. |
| `--target-dir <dir>` | Write managed skill directories below an explicit root; `--scope` and `--agent` are ignored. |
| `--print` | Print target paths and `SKILL.md` contents without writing files. |
| `--force` | Replace unmanaged same-name skills; without it, user-authored same-name skills are preserved. |

Default skill target roots:

| Scope | `--agent claude-code` | `--agent agents` |
|---|---|---|
| `project` | `.claude/skills` | `.agents/skills` |
| `global` | `~/.claude/skills` | `~/.agents/skills` |

Each managed skill is written as `<root>/<skill-name>/SKILL.md`.

`engram uninstall --only skills --apply` removes managed skill files only
from the default project/global roots shown above, after validating the
engram ownership marker. If you installed with `--target-dir` or
`--skills-target-dir`, clean up that custom root manually.

The data dir defaults to `~/Library/Application Support/engram` on macOS
and `%LOCALAPPDATA%\engram` on Windows; override it with
`ENGRAM_DATA_DIR=/path`. `config.toml` lives inside that data dir.

Scheduled maintenance is configured in `[maintenance]` in `config.toml`.
By default, rule-based lint and forget sweep run daily outside hook
latency. Embedding backfill is supported but defaults to off because it
can call a paid provider; enable it with
`embedding_backfill_interval_secs` after configuring an embedder.

---

## Bootstrap mid-project {#bootstrap-mid-project}

When you adopt engram in a project that's already been around for
a while, the wiki starts empty. `engram bootstrap` ingests the
project's existing history into seed pages so the first session has
warm context.

```bash
cd /path/to/project
engram bootstrap
```

`bootstrap` is a thin HTTP client of the running server. With a local
loopback server on `127.0.0.1:49374` it needs no configuration. Set
`ENGRAM_SERVER_URL=http://<server>:49374` only when the server is
remote or uses a custom host/port.

**What gets ingested by default:**

| Source | Priority (dropped first when over budget) |
|---|---|
| `CLAUDE.md` / `AGENTS.md` (project rules) | never dropped |
| `README.md` at the repo root | very-late |
| `docs/**/*.md` | late |
| Substantive git commits (body >120 chars OR conventional-commit prefix) | mid |
| Module-level `//!` doc-comments in `**/*.rs` | first to drop |

**Flags:**

```
--repo-path <PATH>         (default: git rev-parse --show-toplevel)
--workspace <NAME>         (default: "default")
--project <NAME>           (default: derived from cwd — main repo root's
                            basename via `git rev-parse --show-toplevel`,
                            or basename(cwd) when no repo is found.
                            "scratch" only as a defensive fallback for
                            hook events with no usable cwd.)
--max-input-tokens N       (default: 150000; total source budget after prune)
--chunk-input-tokens N     (default: 24000; per LLM call; 0 = single call)
--since "30 days ago"      (git log filter; supports "N days/months/years ago" + YYYY-MM-DD)
--exclude-git              (skip commit history)
--exclude-readme           (skip README)
--exclude-docs             (skip docs/**/*.md)
--exclude-code             (skip Rust module headers)
--dry-run                  (collect + estimate but don't call LLM or write)
--force                    (re-bootstrap, overwrites the prior manifest)
```

**Cost.** With Kimi 2.6 via OpenRouter ($0.73/$3.49 per M):
- 50k input tokens cap → ~$0.04 worst case input
- 1-2k generated tokens → ~$0.007 output
- Total: well under $0.20 per run.

**Idempotency.** The first run produces a per-project `bootstrap.md`
manifest (at `<wiki>/<workspace>/<project>/bootstrap.md`) listing every
page generated + a one-paragraph rationale. Re-running without `--force`
errors out. Delete the manifest (and the generated pages) if you want a
clean re-bootstrap.

**Dry-run first.** Always worth doing before the real call to see
which sources would actually be sent + how many tokens that
represents. Output is JSON to stdout.

```bash
engram bootstrap --dry-run
{
  "sources_collected": 117,
  "sources_sent": 22,
  "sources_dropped": 95,
  "estimated_input_tokens": 48760,
  "pages_written": [],
  "rationale": "(dry-run; LLM not invoked)",
  "dry_run": true,
  "llm_chunks": 1
}
```

Large repos (e.g. years of git history) are pruned client-side before
POST, then processed in sequential LLM chunks so provider context limits
are not exceeded. The CLI logs `llm_chunks` in dry-run and the final
outcome.

**Caveat: LLM-fabricated detail.** A bootstrap run can produce
plausible-but-wrong pages (the LLM doesn't know your project, it's
inferring from git history). The wiki is git-versioned precisely so
this is recoverable: review what landed with `engram checkpoints` (or
`git -C <data-dir>/wiki diff HEAD~1`), and use `engram restore-page`
or a git revert if it's off.

## Operating without auth

For local-only / single-machine setups you can skip the bearer
token — just bind to loopback:

```bash
engram serve --transport http --bind 127.0.0.1:49374
```

Notice the bind: `127.0.0.1:49374`, not `0.0.0.0:49374`. This is the
critical pairing - **no bearer token AND loopback only** is the only
safe combination. The startup log will warn loudly if you bind to a
LAN address without setting `ENGRAM_AUTH_TOKEN`.

Then wire up the agent CLI. Both commands default to no auth and
`http://127.0.0.1:49374` - no extra flags needed for the local case:

```bash
engram install-mcp   --client claude-code --apply
engram install-hooks --agent  claude-code --apply
```

The thin-client commands (`engram status`, `engram search`,
`engram bootstrap`, …) default to `http://127.0.0.1:49374`, the same URL
the generated agent config uses, so they work against a local loopback
server with no extra flags.

---

## Keeping engram up to date

To upgrade, download the new release archive
(`engram-macos-aarch64.tar.gz` or `engram-windows-x86_64.zip`) and
replace the extracted binary — and its sibling `hooks/` bundle — in place
at the same path, then:

1. Stop and restart the server (`engram serve …`) so the new binary is
   running.
2. Re-run `install-hooks --apply` for each configured agent to refresh the
   staged hook scripts under `<data-dir>/hooks/<agent>/`. This is
   idempotent: engram replaces only the hook entries it owns and leaves
   unrelated hooks alone.

```bash
# Example: refresh hooks after replacing the binary.
engram install-hooks --agent claude-code --apply
```

When the upgraded server starts, it applies SQLite schema migrations and
pending wiki-structure migrations automatically. No manual database
reset or wiki rewrite is required for normal upgrades.

If the server runs on another machine, upgrade that machine's binary and
restart its server separately; re-running `install-hooks --apply` on a
client only refreshes local hook scripts.

Source builds upgrade with `git pull && cargo build --release --workspace`,
then the same restart + `install-hooks --apply` steps.

---

## See also

- [`docs/macos.md`](macos.md) / [`docs/windows.md`](windows.md) -
  per-platform native install walkthroughs
- [`docs/https-via-proxy.md`](https-via-proxy.md) - terminating TLS in
  front of engram (engram never terminates TLS itself)
- [`docs/usage.md`](usage.md) - handoffs, proactive querying, web UI, slim
  routing snippet + managed Agent Skills, migration from other memory tools, and raw-wiki inspection
- [`docs/mcp-install.md`](mcp-install.md) - per-client MCP config
  reference for Cursor, Claude Desktop, Gemini CLI, Antigravity CLI, OpenClaw, OMP, VS Code Copilot
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) - what's actually
  running inside engram
