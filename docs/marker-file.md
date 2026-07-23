# Marker file: `.engram.toml`

Declare which workspace (and optionally which project) an agent's
`cwd` belongs to, without depending on the directory's basename.

## Why

engram namespaces every wiki page by `(workspace, project)`. By
default, `workspace = "default"` and `project = basename($cwd)`. That
works for a solo developer in `~/projects/<repo>` but breaks down
for the cases this marker file is built for:

- **Multi-client consultancies** with `~/projects/<client>/<repo>` —
  every client should land in a dedicated workspace, not "default".
- **Work / personal / open-source separation** for solo developers
  who want isolation by life context.
- **Mono-repos** where you'd like all packages under one project
  (instead of basename-of-each-package buckets) — or each package
  under its own project, your call.

The marker file lets you declare these mappings without forking
engram or running CLI commands per directory.

## Where to put it

`.engram.toml` in **any ancestor** of your `cwd`. Lifecycle hooks
walk up from `cwd` toward `$HOME` (or `/` if `$HOME` is unset) and
use the **first** marker found. Closer markers override outer ones. When
a marker is found, hook scripts also forward the current `cwd` so
workspace-only markers can still resolve `project = basename(cwd)` for
handoff lookups.

The marker path is shared by the POSIX/PowerShell hook scripts and the
generated OpenCode / OMP / OpenClaw TypeScript integrations. In all cases,
hook capture and handoff lookup send the same `cwd`, `workspace`, `project`,
`project_strategy`, `drop_subagent`, `briefing`, and
`briefing_budget` query params to the server when a marker declares them;
handoff lookup also sends `cwd` when no marker exists so the default
`project = basename(cwd)` route works consistently.

## Schema

```toml
# Required.
workspace = "movvia"

# Optional. When present, forces project = "pe-portais" for every
# cwd inside this marker's tree. Omit it to let basename(cwd) drive
# the project name.
project = "pe-portais"

# Optional. Omit it to preserve project = basename(cwd). Set it to
# "repo-root" to derive project from the main git repository root, so
# linked worktrees and subdirectories share one project. Ignored when
# `project` is present.
project_strategy = "repo-root"

# Optional. Opt this project into drop_subagent_captures: set it to "true"
# and the server accepts but does NOT store this project's subagent-session
# captures. A multi-agent harness fans one goal out to many subagent
# sessions whose per-event captures can flood a small instance; scoping the
# opt-in here keeps the drop from affecting other projects on the same
# server. Off by default (absent / "false").
drop_subagent_captures = "true"

# Optional. Inject a compiled project brief at session start (and after a
# context clear — Claude Code re-fires SessionStart on /clear): the
# session-start handoff fetch also returns this project's pinned /
# `_rules/` / `_slots/` wiki pages (bodies included) plus recently-updated
# page titles, so the agent starts with the architecture context instead
# of re-exploring the codebase. Appended AFTER any pending handoff, and
# unlike the handoff it is not consumed — it is recomposed every opted-in
# session start. Only agents whose session-start hook injects stdout as
# context benefit (Claude Code, Codex, OpenCode, …). Off by default: the
# brief costs tokens on EVERY session start, so opt in per repo.
[briefing]
inject_on_session_start = "true"

# Optional. Char budget for the brief (~4 chars per token). Bodies over
# budget are truncated with a visible note; crowded-out core pages are
# listed by path so the agent can `memory_query` them. Clamped
# server-side to [500, 20000]; defaults to 4000.
max_chars = 4000
```

**Naming rules** for `workspace` and `project`, validated server-side:

- Lowercase ASCII, digits, dots, dashes, underscores
- Regex: `^[a-z0-9][a-z0-9._-]*$`

Anything else is rejected at `get_or_create_workspace` / `_project`
time, surfacing as a hook warning. The shell helper URL-encodes
defensively but the server's regex is the source of truth.

`project_strategy` accepts `repo-root` (or `repo_root`) only. Unknown
values are ignored and behave like the default `basename(cwd)` strategy.

`inject_on_session_start` accepts a truthy value
(`true` / `1` / `yes` / `on`, quoted or bare — section-style keys are
parsed leniently); anything else behaves as absent. `max_chars` is a
plain integer.

`drop_subagent_captures` accepts a truthy string (`"true"` / `"1"` /
`"yes"` / `"on"`); any other value, or its absence, leaves this project's
subagent captures stored as usual. Top-level (non-subagent) sessions are
always stored regardless. This is per-project on purpose: there is no
server-global switch, so opting one noisy project in never sheds subagent
captures for the others on a shared instance.

## Four canonical examples

### Multi-client

```
~/projects/movvia/.engram.toml     → workspace = "movvia"
~/projects/cliente-x/.engram.toml  → workspace = "cliente-x"
~/personal/.engram.toml            → workspace = "personal"
```

Outcome:

- `~/projects/movvia/pe-api-core` → workspace = `movvia`, project = `pe-api-core`
- `~/projects/cliente-x/api`      → workspace = `cliente-x`, project = `api`
- `~/personal/blog`               → workspace = `personal`, project = `blog`

### Mono-repo with grouped packages

```
~/projects/movvia/.engram.toml              → workspace = "movvia"
~/projects/movvia/pe-portais/.engram.toml   → workspace = "movvia"
                                                  project   = "pe-portais"
```

Outcome:

- `~/projects/movvia/pe/pe-api-core`        → workspace = `movvia`, project = `pe-api-core`
- `~/projects/movvia/pe-portais/apps/web`   → workspace = `movvia`, project = `pe-portais`
  (closer marker wins)

### Git worktrees / repo-root identity

```
~/projects/.engram.toml → workspace        = "oss"
                            → project_strategy = "repo-root"
```

Outcome:

- `~/projects/engram`                → workspace = `oss`, project = `engram`
- `~/projects/engram/crates/cli`     → workspace = `oss`, project = `engram`
- `~/projects/engram-feature-branch` → workspace = `oss`, project = `engram`

If the marker lives inside the main checkout instead (for example
`~/projects/engram/.engram.toml`), copy or commit it into each
out-of-tree worktree, or place a shared marker above the worktree parent
directory as shown here.

Without `project_strategy = "repo-root"`, those same paths keep the
default behavior and resolve by their current directory basename.

Resolution is host-side: lifecycle hooks and generated TypeScript
plugins follow the worktree's commondir pointer (`git rev-parse
--git-common-dir`, or the same Rust/libgit2 helper for native hooks) to
the main repository and send the resolved name as an explicit `project`.
This means it works even when the worktree directory lives **outside**
the main repo tree (some tools keep worktrees in a separate directory,
so the worktree has no `.engram.toml` ancestor of its own) and even
when the server runs in a container that cannot see the host checkout.
Put the marker anywhere on the walk-up path from the worktree — commonly
a single `~/.engram.toml` — to select the strategy.

### Single workspace, no per-repo overrides

```
~/.engram.toml → workspace = "home"
```

Every cwd under `$HOME` lands in workspace `home` with
`project = basename(cwd)`. Useful when you just want to opt out of
the `default` bucket entirely.

## Migrating existing projects

Projects already created under workspace `default` stay there. Move
one with the CLI:

```sh
engram rename-project \
    --workspace default --project foo \
    --new-workspace movvia
```

## Install-wide default (no marker)

`project_strategy = "repo-root"` normally lives in a marker, which means
dropping a `.engram.toml` in (or above) every repo. To get the same
repo-root resolution for a whole install **without** a per-repo marker, bake
it into the generated hooks at install time:

```sh
engram install-hooks --apply --agent claude-code --project-strategy repo-root
```

Every session for that install then resolves its project from the main git
repo root — so an agent that runs `mkdir sub && cd sub` and stays there no
longer forks the rest of the session into a phantom project named `sub`.

This is **install-time config**, written into the agent's hook command (and
the generated OpenCode / OMP / OpenClaw plugins) — the same status as the
`ENGRAM_AUTH_TOKEN` / `ENGRAM_HOOK_URL` it sits beside, *not* a user-set
runtime override (which was deliberately rejected in #16). The flag accepts
`basename` (the default — bakes nothing, behavior unchanged) or `repo-root`.

Precedence is unchanged: a marker's explicit `project_strategy` or `project`
still wins over the install default.

## What the marker file does NOT do

- ❌ No glob patterns. Walk-up by literal ancestry only.
- ❌ No merge of ancestor markers. Closest wins.
- ❌ No automatic migration of `default`-workspace projects.
- ❌ No automatic repo-root collapsing. Worktrees and subdirectories only
  share a project when `project_strategy = "repo-root"` is explicitly set
  (per marker, or baked install-wide — see above).
- ❌ No user-set env / auth / hook-url override. Use the existing env vars
  (`ENGRAM_AUTH_TOKEN`, `ENGRAM_HOOK_URL`) for those. (A repo-root
  *default* can still be baked into an install without a marker via
  `install-hooks --project-strategy repo-root`, but that is install-time
  config, not a runtime override the user sets in their shell.)

## Troubleshooting

**My marker isn't being picked up.** Walk through:

1. File is named exactly `.engram.toml` (note the leading dot).
2. File is in an **ancestor** of the cwd — not a sibling, not a
   descendant.
3. There isn't a closer marker overriding it. Run
   `find ~/projects -maxdepth 5 -name '.engram.toml'` to see all
   markers in your tree.
4. The workspace / project values match the regex above (lowercase
   alphanumerics, dots, dashes, underscores).
5. If you use `project_strategy`, it is exactly `repo-root`.

Hook scripts run fire-and-forget by design, so they don't log on
success. To see what's actually being sent, run a hook script by
hand:

```sh
printf '{"cwd":"%s"}' "$PWD" \
  | sh ~/.local/share/engram/hooks/claude-code/post-tool-use.sh
```

If the marker is being read, the curl line (visible with `set -x`
or in server logs) will include `&workspace=...` in the URL.
