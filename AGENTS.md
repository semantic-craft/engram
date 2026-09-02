<!-- engram:start -->
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
full-evidence workflow. Handoffs chain: a receiver publishes a successor
from the latest Checkpoint, earlier transfers stay readable history, and a
finished WorkItem takes follow-up work as a new related WorkItem.

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
<!-- engram:end -->

## Canonical Agent Instructions

- `AGENTS.md` is the single canonical instruction file for this repository,
  including Claude Code. Keep `CLAUDE.md` as a short pointer to this file;
  do not duplicate project rules there.
- Read `docs/ARCHITECTURE.md` when you need the current operational map, and
  `docs/design-decisions.md` when you need historical rationale.
- Read `docs/auto-improvement-loop.md` before changing auto-improvement review,
  pending proposal storage, approval flows, or prompt routing for learning
  review.

## Agent skills

### Issue tracker

Issues and PRDs live in GitHub Issues for `semantic-craft/engram`. See
`docs/agents/issue-tracker.md`.

### Triage labels

Use the canonical engineering-skill triage roles and their repository label
mapping. See `docs/agents/triage-labels.md`.

### Domain docs

Use the single-context domain-document layout. See `docs/agents/domain.md`.

## Project Summary

engram is a self-contained Rust binary providing long-term memory for AI
coding agents over MCP and lifecycle hooks. Markdown-in-git is the wiki source
of truth; SQLite is the derived index for search, sessions, observations,
WorkItems, Handoffs, Claims, Checkpoints, Attempts, users, audit, and embeddings. Capture is automatic through hooks;
durable retrieval follows the Karpathy-style LLM Wiki pattern.

## Stack And Layout

- Runtime: Rust edition 2024, `tokio`, workspace resolver 3.
- MCP/HTTP: `rmcp` plus `axum` for MCP HTTP, hooks, admin, and web routes.
- Store: `rusqlite`, `refinery` migrations, FTS5, sqlite-vec-compatible
  embeddings, one SQLite file, one writer actor, read pool.
- Wiki: markdown on disk, `notify-debouncer-full` watcher, atomic writes,
  `git2` checkpoints.
- LLM: typed providers in `engram-llm`; provider-specific behavior belongs
  there, not in CLI/admin handlers.
- Config: `figment`; runtime behavior resolves config once and threads typed
  settings through call sites.
- Crates: `engram-core`, `engram-store`, `engram-wiki`,
  `engram-mcp`, `engram-hooks`, `engram-llm`,
  `engram-consolidate`, `engram-cli`, plus `engram-web` and `evals/`.

## Workflow Rules

- Keep changes small and scoped. Do not start adjacent feature work unless the
  current task requires it.
- No dead code or half-built public surface. If something is future work,
  document it in docs/design notes rather than shipping unreachable stubs.
- Document constraints and incidents, not line-by-line mechanics.
- Add focused regression tests for bug fixes and behavior changes.

## Project Maintenance Rules

- Keep `CLAUDE.md` as a pointer to `AGENTS.md`; this avoids split-brain
  instructions between Claude Code and AGENTS-aware harnesses.
- Any change affecting user-visible behavior, installation, supported
  platforms, supported agents, providers, deployment, env/config, or public
  tool/admin surfaces must update `CHANGELOG.md` and the relevant README/docs
  references in the same commit.
- Do not bump crate/package versions, minor versions, or cut release tags
  automatically. Ask the user before any version bump or release tag; prefer no
  version change unless the user explicitly approves it.
- When asked to evaluate a PR, report the pros, cons, and recommended fix,
  then ask the user for approval before merging or pushing PR changes. Do not
  merge PRs during evaluation unless the user explicitly approves that action.
- When the MCP tool surface changes, update `MEMORY_INSTRUCTIONS`,
  `engram_core::SNIPPET_BODY`, README/docs tool references, and regression
  tests that assert every tool appears in both prompt surfaces.

## Rust Engineering Rules

- Prefer small, behavior-preserving changes. Do not add compatibility
  branches, new abstractions, or new public surface unless a shipped caller,
  persisted data, or explicit requirement needs them.
- Optimize the real bottleneck class first: algorithm, query shape, batching,
  allocation count, IO boundaries, and container choice. Avoid clever
  micro-optimizations without evidence.
- Keep SQLite writes behind the single writer actor. For hot paths, batch work
  into one command/transaction instead of spawning many writer messages or
  opening per-row transactions.
- Number new store migrations strictly above the highest embedded migration
  (currently V106; next is V107). Never slot a migration into a historical
  gap: stores migrated by an earlier release sit above it and can only be
  opened again through the gap-backfill path in `engram-store::migrations`.
  The `no_new_migrations_below_released_high_water_mark` test enforces this;
  append newly shipped versions to its `RELEASED` list when cutting a
  release.
- Avoid N+1 store reads. Prefer reader methods that return the data shape the
  caller actually needs, and use cached/prepared statements for repeated
  queries.
- Keep hook ingestion fire-and-forget and bounded. Do not introduce unbounded
  `tokio::spawn` fan-out, unbounded queues, or synchronous agent-facing waits.
- Keep CLI commands thin: parse arguments, resolve config once, call typed
  library functions, render output. Provider-specific behavior belongs in the
  provider module, not in command handlers.
- Treat typed boundaries as load-bearing: IDs, `PagePath`, `AgentKind`,
  sanitization, workspace/project resolution, auth capability, and provider
  dialects should be parsed or normalized once and reused.
- Preserve workspace/project isolation through the shared scope framework.
  New MCP/admin/web routes must use `engram_store::ScopeResolver` or its
  explicit helpers (`lookup_existing_scope`, `create_explicit_scope`,
  `resolve_many_existing_scopes`) instead of hand-rolled workspace/project
  lookup chains. Read, search, embed, retention, and destructive paths must use
  no-create lookups and fail closed on partial or missing scope; only explicit
  write/create paths may create workspaces or projects. PRs touching scope
  resolution need table-driven tests for partial scope, missing explicit scope,
  active-project precedence, and cross-workspace isolation.
- Preserve auth boundaries through `AuthLevel::authorize(Capability::...)`.
  Do not open-code username comparisons or ad hoc root checks in handlers. In
  multi-user mode, every `/admin/*` route is root-only; DB-user tokens are for
  normal MCP/API read/write attribution and must not bypass admin gates or
  admission webhooks. PRs touching permissions need tests for root, DB-user,
  and anonymous behavior.
- Treat markdown as the source of truth and SQLite as the derived index. Wiki
  mutations must go through `Wiki::write_page`, `Wiki::apply_batch`, or the
  existing destructive helpers so sanitization, admission, attribution,
  rollback, and index updates stay together. Do not write wiki files directly
  from handlers; add recovery/rollback tests for any new disk+SQL mutation.
- Prefer explicit fallbacks over `unwrap`, `expect`, or `unreachable!` in
  runtime paths. Panics are acceptable in tests only.
- Do not use `unsafe` for performance work in this project unless profiling
  proves it is necessary and the safety argument is documented in the code.
- Add focused regression tests for bug fixes and behavior changes. For
  filesystem tests, use temp dirs or injected roots; never depend on the real
  user home directory being writable.
- Run the full local gate before claiming a Rust change is ready:
  `cargo fmt --check`, `git diff --check`,
  `TAILWIND_SKIP=1 cargo test --workspace`, and
  `TAILWIND_SKIP=1 cargo clippy --workspace --all-targets -- -D warnings`.
