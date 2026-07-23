<!-- engram:start -->
## Long-term memory (engram)

This project uses [engram](https://github.com/semantic-craft/engram)
for cross-session continuity.

**Default to the current project — always.** Every engram tool
auto-scopes to the project resolved from your session's working
directory. **Do NOT pass `project`, `workspace`, or `cwd` arguments unless the user
explicitly references a *different* project by name** (e.g. "what did we
decide in the `other-app` project?"). Phrases like "this project",
"here", "we", "our work", "where did we leave off" all mean the *current*
project — call the tool with no scoping args. If the user asks about a
handoff and the SessionStart auto-fetched block is already in your
context, just answer from it; do not re-call the tool to "find it again"
in another project.

**Lifecycle hooks already capture every prompt + tool call
automatically.** You never need to manually write routine notes; the
SessionStart hook auto-fetches pending handoffs, and on session end
engram writes a session-summary page and a handoff.
LLM consolidation (compiling observations into topical wiki pages) runs
on PreCompact, on demand via `memory_consolidate`, and at session end
only when the server sets `ENGRAM_CONSOLIDATE_ON_SESSION_END`. Only
write a durable wiki page when the user explicitly asks to remember or
annotate something permanently.

### When to reach for each tool

The user can express any of the intents below in plain English —
match the intent to the tool. They do not need to name the tool.

| User says / situation | Tool |
|---|---|
| "have we discussed X?" / "search memory for Y" / before proposing architecture | `memory_query` (current project; `scopes` for named siblings; `global=true` to search every project) |
| "what's been going on" / "show recent activity" (light) | `memory_recent` |
| "is engram healthy?" / "how big is the wiki?" | `memory_status` |
| "give me the stats" / structured snapshot for the agent to consume | `memory_briefing` (read-only; never creates handoffs) |
| "catch me up" / "I've been away" / "what's important right now?" / open-ended exploration | `memory_explore` |
| "where did we leave off?" — and you see a `📥 engram: pending handoff` block in your context | already done — answer from that block; do NOT re-call `memory_handoff_accept` |
| "where did we leave off?" — and no such block is visible | `memory_handoff_accept` (rare; the SessionStart hook usually got there first; pass `workspace` + `project` together only for a named sibling workspace/project) |
| "save context for the next session" / wrapping up / ending this session | `memory_handoff_begin` (session-end only; do **not** use for status/briefing; single-use handoff; terse summary; put detail in `open_questions` + `next_steps` bullets; pass `workspace` + `project` together only for a named sibling workspace/project) |
| "discard that handoff" / "I created a handoff by mistake" | `memory_handoff_cancel` (requires exact `handoff_id` from `memory_handoff_begin`; marks it expired before the next session sees it) |
| "consolidate this session" / "compile what we learned" (also runs on PreCompact; at session end only if `ENGRAM_CONSOLIDATE_ON_SESSION_END` is set) | `memory_consolidate` |
| "what did we learn from this session?" / "what memory should we add?" / explicit wrap-up learning review | `memory_auto_improve` (manual learning review for a completed session; omit `session_id` for latest completed session; the server also schedules background review for newly completed sessions in every project when configured) |
| "remember this permanently" / "save a note" / "add an annotation" / durable project knowledge | `memory_write_page` (write a wiki page; do **not** use handoff for permanent notes; put the title as a `# H1` on the first line of `body` and omit the `title` arg — engram derives it from the H1) |
| "read the page about X" / "show me the full content of Y" / "open the page on Z" | `memory_read_page` (full body; pass a query to search or `path` for a direct lookup; pass `workspace` + `project` together only for a named sibling workspace/project) |
| "delete the page X" / "remove that note" | `memory_delete_page` (by exact `path`; idempotent; pass `workspace` + `project` together only for a named sibling workspace/project) |
| "audit the wiki" / "find contradictions" / "what rules should we add?" | `memory_lint` |
| "prune old pages" / "memory cleanup" | `memory_forget_sweep` |

`memory_explore` is the right default for the "I want to know what's
going on" use case — it returns a prose digest whose verbosity
scales automatically to how long it's been since the last activity
(< 1 h → one line; > 30 days → full catchup).

### When the current project comes up empty — broaden the search

`memory_query` searches only the **current** project by default. If a
search comes back empty or thin, the knowledge may live in a **sibling
project** — shared `infra`, `ops`, or a related app. Don't conclude
"we never recorded it" after a single project misses; broaden instead:

- **Know which projects to check?** Re-run with explicit `scopes`, e.g.
  `scopes: [{ "workspace": "default", "project": "infra" }]`.
- **Don't know where it lives?** Pass `global=true` to search every
  project in every workspace at once. Each hit is annotated with its
  workspace + project so you can tell where it came from. `global=true`
  cannot be combined with `scopes`/`project`/`workspace`.

`memory_query` returns **snippets, not full page bodies** — an empty or
short snippet does **not** mean the page is empty (a large page can
match outside the snippet window). To read the whole page, use
`memory_read_page` (by `path`, or pass a `query` to fetch the top hit's
full body; add `workspace` + `project` together only when the user names
a sibling workspace/project).

### Use Retrieved Memory As Operating Guidance

When `memory_query` or `memory_recent` returns `_rules/`, `gotchas/`,
`procedures/`, or `decisions/` pages that match the current task, treat
them as actionable context, not trivia:

- Read full pages with `memory_read_page` when the snippet looks relevant.
- Apply `_rules/` as constraints.
- Check `gotchas/` as preflight warnings before editing the same subsystem.
- Follow `procedures/` as checklists for releases, PR reviews, deploys,
  migrations, and other repeatable workflows.
- Use `decisions/` as prior architecture unless the user explicitly asks
  to revisit them.

Before non-trivial coding, debugging, deployment, release, auth, scope,
migration, PR-review, or data-preservation work, search memory for the
subsystem and task type first. If the first query is thin, broaden or
query specific error/subsystem terms before designing a fix.

### Learning Review

The server schedules background auto-improvement for newly completed sessions in
every project when an LLM provider is configured. `memory_auto_improve` is the manual version:
use it when the user asks what durable lessons this session suggests, or at
explicit wrap-up when reviewing proposed memory would be useful. Scheduled and
manual runs apply or stage validated edits through the auto-improvement approval
path. Admins can turn off scheduling with `[auto_improve.scheduler] enabled =
false`, or opt into manual proposal approval with `[auto_improve]
require_approval = true`, in which case scheduled and manual proposals stay in
pending-writes until approved.

### When you write a project rule, write it here

If you're about to write a durable project rule ("always X", "never
Y", "all PRs must …"), write it in the project's canonical agent
instruction file. Many projects use CLAUDE.md for Claude Code and
AGENTS.md for Codex / OpenCode / Cursor / Gemini CLI, but if the
project says one file is canonical, use that file. engram's lint
pass surfaces the same hint automatically when a `kind: rule` page
lands in `_rules/`.

### Refreshing this snippet

This block is maintained by engram. Two ways to refresh it with
the latest binary's recommended copy:

- **From the agent** (no terminal needed): ask "refresh the engram
  routing in this project" — the agent calls
  `memory_install_self_routing`, picks the right filename for itself
  (Claude Code → `CLAUDE.md`; Codex / OpenCode / Cursor / Gemini →
  `AGENTS.md`), and uses its Write / Edit tool to land the block.
- **From the CLI**: `engram install-instructions` (defaults to
  `CLAUDE.md`; pass `--target AGENTS.md` for non-Claude agents or
  projects that use `AGENTS.md` as the canonical instruction file).

Both are idempotent: re-runs replace the block bracketed by
`<!-- engram:start -->` / `<!-- engram:end -->` markers
without disturbing the rest of the file.
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

## Project Summary

engram is a self-contained Rust binary providing long-term memory for AI
coding agents over MCP and lifecycle hooks. Markdown-in-git is the wiki source
of truth; SQLite is the derived index for search, sessions, observations,
handoffs, users, audit, and embeddings. Capture is automatic through hooks;
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
