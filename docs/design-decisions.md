# engram - Design Decisions (Synthesis)

> Distills the four research reports (`research-*.md`) and three issue-tracker
> reports (`issues-*.md`) into the concrete decisions this project will make.
> Read this first; the research files are the receipts.

## 1. Product shape

A self-contained Rust binary that:

1. Runs as an **MCP server** (stdio + HTTP/SSE) for coding-agent CLIs (Claude Code, OpenAI Codex, Cursor, Gemini CLI, Antigravity CLI, OpenClaw, OpenCode, OMP, and MCP-capable clients).
2. Captures the agent's session **automatically** - no `write_note` ceremony - via hook scripts or generated extensions that the agent CLIs invoke (Claude Code / Codex / Cursor / Gemini CLI / Antigravity CLI lifecycle hooks, OpenClaw / OpenCode / OMP TypeScript integrations). Optional transcript-tail fallback for agents without hook APIs.
3. Maintains a **Karpathy-style wiki**: incrementally-compiled markdown pages with cross-links, supersession, an `index.md` and a `log.md`.
4. Serves retrieval via the MCP `tools/list` to coding agents: a handful of *narrow* tools, not 50.
5. Ships as **two native prebuilt archives** - macOS on Apple Silicon (`engram-macos-aarch64.tar.gz`) and Windows x86_64 (`engram-windows-x86_64.zip`); users extract and run the binary directly. The data directory is portable, so memory moves between machines by copying/`rsync`ing it. (Docker/Linux/AUR were dropped.)
6. Is *self-healing*: schema migrations on startup, vector-index dim/provider check, write-ahead durability, periodic integrity audit, single-writer queue to avoid `database is locked`.

## 2. Hard requirements (extracted from the prompt)

- Rust, clean architecture, modular, unit-tested.
- Cargo-format clean.
- Native prebuilt distribution (macOS aarch64 + Windows x86_64), easy backup, easy move between machines.
- MCP server for coding agents.
- **Automatic** memory capture/fetch - minimal manual tool invocations.
- Differentiates **short-term** vs **long-term** memory temporally (like agentmemory).
- Self-healing memory management.
- Helps with handoffs between agent CLIs (resume from Codex where Claude Code left off).
- Iteratively planned - each feature working before the next starts. No dead code.

## 3. Storage model - the biggest architectural decision

Three options surveyed:

| Option | Source-of-truth | DB used for | Pros | Cons |
|---|---|---|---|---|
| **A. DB-primary** | SQLite | Everything | Single transaction boundary, fast search, no FS race conditions | Opaque to humans; harder backup story |
| **B. Markdown-in-git** primary | Files in repo | Derived index | Diff-able, grep-able, portable, Karpathy-faithful | Watcher correctness (basic-memory #580/#758/#798), inode races (#765), startup cost |
| **C. DB-primary with on-demand export** | SQLite | Everything | Best of both | Two formats to keep coherent; user must remember to export |

**Decision: Option B - markdown in a git repo is source of truth, SQLite is derived index.**

**Why:**
- Backup/move story is trivial - `git clone` or `rsync` a directory. The user explicitly asked for this.
- Karpathy's pattern *is* the wiki on disk. Faking it with an export step loses the inspect-in-Obsidian property.
- DB is rebuildable from files - corruption is recoverable.
- Cross-tool compatibility for free: any agent that reads `~/.engram/wiki/*.md` works without an MCP integration.

**How we avoid basic-memory's watcher pain:**
- Watcher has a heartbeat + reconciliation pass (full diff every 30s to catch missed events).
- We *own* writes through the MCP server's `wiki_write` path; the watcher is a *safety net* for external edits, not the primary input.
- Inode-locking advisory + psutil-style live-process check before destructive ops (`reset`, `purge`). Lesson from basic-memory #765/#776.
- Hidden-directory paths handled explicitly (basic-memory #798).

**How we avoid the "files-and-DB drift" overhead:**
- DB stores `(path, mtime, size, sha256, indexed_at, provider, model, dim)` per page. On startup, fast scan vs. cached SHAs; only changed files re-parsed.
- Embeddings keyed by `sha256(content) + provider + model + dim`. Re-embed only when content changes.

**Consistency contract:** markdown is primary and SQLite is derived. There is no
real cross-resource transaction between the filesystem and SQLite. Wiki writes
must go through `Wiki::write_page`, `Wiki::apply_batch`, or the existing
destructive helpers so sanitization, admission, attribution, rollback, and
store updates stay together. Runtime store failures roll installed files back
best-effort; crash windows are resolved by the existing markdown reindex path.
Handlers must not write wiki files directly.

## 4. Database choice - single SQLite file

**Decision: one SQLite file with FTS5, packed-vector embeddings, and SQL tables for graph edges.**

Why not Postgres/pgvector? Cognee's #2717 and basic-memory's #830/#831 show Postgres is a real-deployment-only pain. v1 ships embedded.

Why not LanceDB/Qdrant/Kuzu/CozoDB/SurrealDB?
- LanceDB: cognee #2702/#2720 (file-format drift, filter propagation failures). Pyarrow underneath.
- Kuzu / Ladybug: cognee #2098/#2768 (upstream archived, fork-risk realized).
- CozoDB: small bus factor.
- SurrealDB: heavy, multi-mode storage; we'd inherit a lot of surface we don't need.
- Packed vectors in SQLite keep v1 dependency-light; `sqlite-vec` remains the scale-up path once brute-force cosine stops being enough.

**The graph is just SQL tables.** A `wiki_pages` table, a `wiki_links (from_id, to_id, link_type)` table, optional `wiki_concepts (page_id, concept)`. Graph queries are recursive CTEs in SQLite. Petgraph in-memory for batch traversals. Avoids the entire "embedded graph DB" footgun cognee fell into.

**Crates** (research-backed picks):
- `rusqlite` for embedded SQLite access. `bundled-sqlcipher` if we want encryption later.
- `refinery` for SQL migrations.
- `tantivy` *not* used initially - sqlite FTS5 is sufficient at the corpus sizes we expect (hundreds to low-thousands of pages per project). Revisit only if FTS5 ranking proves inadequate.
- `petgraph` for in-memory graph algorithms during consolidation.

## 5. Embedding & LLM

**Embeddings:**
- Default: **local model via `ort` (ONNX Runtime) crate or `fastembed-rs`** running `bge-small-en-v1.5` (384 dim) or `bge-small-en-v1.5-q` quantized. Same model basic-memory uses.
- Persist `{provider, model, dim}` next to every vector. On mismatch, warn and ignore stale vectors until `engram embed --force` or scheduled backfill re-embeds them (agentmemory #469 lesson, without blocking startup).
- Cache path: `<data_dir>/models/`, never `/tmp` (basic-memory #741).
- Trait-based: `trait Embedder { ... }` with implementations `LocalOrtEmbedder`, `OpenAIEmbedder`, `VoyageEmbedder`. User configures one.

**LLM for consolidation passes:**
- **Off by default**, behaves like agentmemory after #138's fix. Without a provider, the system still works: synthetic compression (rule-based), no LLM-generated summaries, no `memory_consolidate` page-rewrite.
- With a provider, LLM consolidation runs on PreCompact, on demand via `memory_consolidate`, and at session end only when `ENGRAM_CONSOLIDATE_ON_SESSION_END=true` (off by default — session end always writes a rule-based summary page + handoff regardless). Optional 6h maintenance timer.
- Provider trait `LlmProvider { complete(...); complete_structured(...) }`. Implementations: `AnthropicProvider`, `OpenAIProvider`, `GeminiProvider`, `OpenAICompatProvider` (the latter covers Ollama / vLLM / LM Studio and supersedes the earlier `OllamaProvider`).
- **Native HTTP per provider** - no LiteLLM-equivalent. The cognee tracker (#2412/#2430/#2537/#2608/#2749/#2782/#2840/#2842) showed silent-kwarg-drop in a generic gateway is the #1 source of provider bugs. Each provider's typed JSON, errors on unknown fields. Hand-coded but correct.
- **Structured output via JSON schema, not XML, not Instructor-style wrapping.** Use each provider's native JSON-mode where available; for Anthropic, request a tool-use response with a typed schema. Validate with `serde_json` + `schemars`-derived schemas.

## 6. Capture model - auto, never `write_note`

Three capture surfaces, in priority order:

1. **Lifecycle hooks/extensions** (Claude Code, Codex, Cursor, Gemini CLI, Antigravity CLI, OpenClaw, OpenCode, OMP). These are fast, reliable, structured. We ship hook scripts or generated TypeScript integrations the user installs once. Lessons from agentmemory:
  - Hooks must be **fire-and-forget** (#221). No `await fetch()` blocking session start.
  - Sub-second hard timeouts on the writer side (`tokio::time::timeout`).
  - All hooks → single HTTP/Unix-socket POST → server queues → returns 202
    immediately, or 429 when saturated.
  - Privacy strip at the hook boundary, not later (agentmemory `stripPrivateData`).

2. **Transcript tail** (universal fallback). Watch `~/.claude/projects/`, `~/.codex/`, `~/.config/opencode/sessions/`. Lossier but works for any agent. Required for the basic-memory #669/#687/#730 demand the tracker has been asking for.

3. **Manual MCP tool** (`memory_remember`) - only for ad-hoc explicit captures from the user ("remember this"). Not the primary path; not what the agent reaches for by default.

## 7. Memory model (temporal)

Adopt agentmemory's tier model **but** keep the surface narrow:

| Tier | What it is | Lifetime | Decay |
|---|---|---|---|
| **Working** | Current session: last N observations, last user prompt, current files | Until session end | Drop on session end (kept in DB for forensics, but excluded from default recall) |
| **Episodic** | Per-session summaries with concept tags, files-touched, decisions made | 30 days hot, 180 days cold, then evict if cold-score < threshold | `salience · exp(-λ · age_days) + σ · log(1 + access_count) · exp(-μ · days_since_access)`. Code of record: [`crates/engram-store/src/decay.rs`](../crates/engram-store/src/decay.rs). |
| **Semantic** | Distilled facts/preferences/architecture notes - the wiki pages themselves | Indefinite, supersedeable | Versioned in place: old `is_latest=false`, new `supersedes=old_id` |
| **Procedural** | Repeated patterns extracted from episodic clusters (`pattern` type with frequency ≥ 2) | Indefinite | Frequency-decay if not re-observed in N days |

**Implementation note:** the four tiers map to one `pages` table with a `tier` enum column + an `observations` table for raw working/episodic, not four separate tables. Keeps schema migrations sane.

## 8. Consolidation (the Karpathy bit)

Three scheduled MCP operations:

- **`memory_ingest`** (auto-called by hooks): one observation → write-fan-out to ~5–15 wiki pages. New page if no match; supersede + version if the page already exists. No-LLM fallback: append to a per-day digest page if no provider configured.
- **`memory_query`** (called by agent on demand): hierarchical - search `index.md` first, then page-level FTS+vector, then optional graph-walk expansion. RRF-fused. Agentmemory hit 95.2% R@5 with this pattern.
- **`memory_lint`** (scheduled hourly + on session-end): scans for contradictions, orphan pages, broken links, stale claims, low-confidence + zero-reinforcement entries. Pure LLM with strict JSON output.

Decay/forget runs as a separate `memory_forget_sweep` job: applies the retention formula; soft-deletes via `is_latest=false` + `superseded_at`; hard-deletes only after 180 days *and* zero accesses. Never silently destroys anything user-pinned.

Auto-improvement work stays separate from normal session consolidation. The
reviewer notes and staged design are in
[`docs/auto-improvement-loop.md`](auto-improvement-loop.md). The short version:
learning review is scheduled for newly completed sessions in every project when
an LLM provider is configured, and manual CLI/admin/MCP runs remain available for
catch-up or targeted reruns. Scheduling and approval are separate: `[auto_improve.scheduler]`
controls background review, while `[auto_improve] require_approval = true` keeps
scheduled and manual proposals pending for human approval instead of applying
them automatically. All writes remain scoped through the shared resolver/auth
paths and must not mutate the active agent context mid-turn.

## 9. Cross-agent handoff

A first-class typed protocol, shared state:

```rust
WorkItem       stable user objective + acceptance criteria + explicit state
Run / Session execution identity, separate from authenticated actor identity
Handoff        revisioned transfer offer for one WorkItem
Claim          opaque receiver capability with a bounded lease
Checkpoint     append-only ordered progress; may acknowledge one Claim
Attempt        caller identity for replay-safe claim/release/checkpoint retries
ArtifactRef    typed file/git/worktree/external evidence; cwd is never identity
Relationship   explicit depends_on / derived_from / child_of between WorkItems
```

`memory_handoff_begin` creates a WorkItem plus open Handoff, or publishes a
successor for an exact existing WorkItem owned by the same authenticated
actor and source Run. `memory_handoff_discover` is read-only. The receiver
uses compare-and-set `memory_handoff_claim` with a caller `context_budget`;
the claim returns the same ContextPackage and assembly trace as
`memory_query`. Its first
`memory_checkpoint_write` acknowledges the Claim and Handoff transactionally
and may attach typed ArtifactRefs and explicit WorkItem relationships on that
ack checkpoint. Acknowledgement is not completion: only an explicit checkpoint
state can block, complete, or abandon the WorkItem. `memory_handoff_release`
reopens a live Claim, and expired leases are claimable by another receiver.
Claim, release, and checkpoint Attempts bind actor, scope, exact identities,
expected revisions, Run, and canonical request digest so a lost-response retry
returns the original result while changed reuse fails closed. Claim secrets and
checkpoint payloads are excluded from audit detail. Handoffs and Checkpoints
may carry typed ArtifactRefs: files use repository or project coordinates plus
a repository-relative locator, Git refs use repository identity plus revision,
worktrees distinguish dirty checkouts at the same commit, and absolute paths
stay local-path hints (they are rejected as worktree locators). Observation
metadata (source Run, timestamp, provenance, dirty, local-path hint, git ref,
content hash, tree hash) is stored per attachment so a later observer does not
inherit the first writer's facts. Shared artifact identity has no project-level
CASCADE; attachments follow the observing project's lifecycle. SessionStart
continuation markdown lists artifacts and relationships without copying claim
ids or treating cwd as identity. Both Handoff and Checkpoint responses expose ArtifactRefs and
relationships with stable identities and revisions.
Changed, verified, committed, pushed, reviewed, merged, released, deployed,
submitted, and approved facts are stored independently and never inferred from
one another. Related work uses `depends_on`, `derived_from`, or `child_of`
instead of inheriting a prior WorkItem's claim or blockers; a child may return
structured `parent_result` evidence but cannot complete, abandon, claim, or
supersede its parent. Cross-project links require a complete existing
workspace/project pair and fail closed. Engram records these observations; it
does not check out, commit, push, merge, release, deploy, submit, or approve
Git or external systems.

A WorkItem outlives any one transfer, so continuation *appends* rather than
overwrites. A successor Handoff asserts the state it was built from — the
caller's expected WorkItem revision and the `work_item_revision` of the latest
Checkpoint — and a stale value aborts before any mutation. On success it stores
its predecessor Handoff, that exact Checkpoint, and its own source Run/Session,
and atomically supersedes only the transfer still sitting at `open`. Claimed and
terminal transfers are immutable history: `acknowledged`, `expired`,
`cancelled`, and `superseded` all stay readable, and `memory_handoff_discover`
returns the WorkItem's chain ordered oldest-to-newest with source and receiving
provenance while only ever offering the newest non-superseded eligible
transfer. Cancellation was split out of `expired` for exactly this reason: the
four ways a transfer ends — source cancellation, claim release back to `open`,
lease expiry, and supersession — must stay distinguishable in the state machine
and in `audit_log`, or a chain cannot be explained after the fact. A
`completed` or `abandoned` WorkItem refuses further Handoffs, checkpoints, and
claims rather than being silently reopened — reaching a terminal state retires
every still-live transfer, open or claimed, and resolves its lease in the same
transaction, so nothing is left discoverable but unresolvable against finished
work — and follow-up work is a new WorkItem joined by
`depends_on`, `derived_from`, or `child_of`. Discovery reads the whole envelope
(transfer, WorkItem, latest Checkpoint, chain) from one snapshot, so a
successor published mid-read can never hand a receiver a superseded transfer
beside a chain that already describes its replacement. Nothing deletes a predecessor: retention operates
on wiki pages, and Handoff rows are only ever removed by an explicit project
purge that removes the whole scope.

The cwd is matched by path-boundary: a Handoff left in `/repo` is discovered by
a session in `/repo/api`, but never `/repo-other`. Manual project-wide Handoffs
remain preferred over auto SessionEnd Handoffs, then the most specific cwd, then
the most recent. SQLite is the operational coordination source of truth behind
the single writer actor; Markdown remains the durable knowledge source of truth.

agentmemory has this informally (`/handoff` skill); we make it explicit from day one because every research report flagged cross-agent as the v0.1 weak spot. The retired single-use `memory_handoff_accept` contract is not current behaviour: a read never consumes a Handoff, and the automatic two-adapter SessionEnd → SessionStart journey continues one WorkItem.

### 9a. Why SessionStart claims instead of reading

The original session-start injection was a read: it rendered the open Handoff
and told the agent to claim it over MCP. In practice the agent had already been
handed the context, so the claim was a second, easily-skipped step, and two
agents starting in the same directory could each be shown the same "pending"
work.

Automatic recovery therefore takes the same claim as the on-demand path. The
part that made a claim unsafe to automate is capability, not policy: claiming
for a harness that then discards the rendered output consumes a transfer nobody
read and leaves a lease no Run can acknowledge. So the decision is expressed as
a typed **Agent Adapter contract** in `engram_core::adapter` — one capability
(does this harness deliver SessionStart output?) gating one behaviour (may it
claim automatically?). Harnesses that discard it, and harnesses engram has not
established, do nothing automatically at all; that is why an unknown adapter
degrades to a no-op read rather than a best-effort claim.

Three consequences follow deliberately:

- **The claim binds to the real Run.** The `/handoff` request carries the
  harness's own session id, mapped through the same
  `SessionId::from_agent_session` the capture path uses. Without a reported Run
  the server renders read-only instead of claiming, because a lease bound to no
  Run can never be acknowledged.
- **Rendering never accepts.** Injection leaves the transfer `claimed`. A
  session that dies before its first checkpoint leaves recoverable work, not
  lost work, and the lease expiry rule is shared with every other claim path.
- **One assembler, two callers.** Continuation assembly moved into
  `engram_store::continuation` so the hook path and the MCP tool cannot drift
  on quotas, priority, deduplication, omission reporting, or scope resolution.
  The hook path passes no embedding — SessionStart must make no synchronous
  model call — which is the same BM25 degradation `memory_query` takes with no
  provider configured.

Adapter identity dimensions stay distinct throughout: the authenticated actor
authorizes, the harness kind and delivery capability decide behaviour, the Run
owns the lease, and the WorkItem/cwd/target values are selection hints that
narrow inside an already-resolved scope and grant nothing.

## 10. MCP tool surface - narrow on purpose

basic-memory has ~25 tools, agentmemory has 53. Both have user confusion as a result. The current v1 surface is still deliberately narrow:

| Tool | Purpose | Annotation |
|---|---|---|
| `memory_query` | Existing hybrid retrieval plus caller-budgeted ContextPackage assembly | read-only |
| `memory_context_read` | Resolve an exact revisioned ContextRef to full evidence | read-only |
| `memory_recent` | Most-recently-updated `is_latest=1` pages for the project | read-only |
| `memory_status` | Health, counts, last-consolidation-at | read-only |
| `memory_briefing` | Structured zero-LLM snapshot: 7d/30d windows, pending handoffs, recent pages, `_rules/` | read-only |
| `memory_explore` | LLM-composed prose digest over `memory_briefing`; degrades to JSON without a provider | read-only |
| `memory_handoff_begin` | Create or continue a WorkItem and publish an open Handoff with a bounded brief and revisioned ContextRefs | write |
| `memory_handoff_discover` | Read a claimable Handoff without mutation | read-only |
| `memory_handoff_claim` | Claim an exact revision under a bounded lease and return a budgeted ContextPackage | write |
| `memory_handoff_release` | Reopen an exact live Claim | write |
| `memory_checkpoint_write` | Append progress and optionally acknowledge the receiving Claim | write |
| `memory_handoff_cancel` | Source-owner cancellation of an exact open revision | write |
| `memory_consolidate` | LLM-driven page rewrite (`multi_page=true` for atomic fan-out) | destructive |
| `memory_auto_improve` | Manual learning review for a completed session; the server also schedules review for new sessions, and manual-review opt-in keeps proposals pending | write |
| `memory_write_page` | Write durable wiki knowledge on explicit user request | destructive |
| `memory_read_page` | Read a full page body by exact path or top search hit | read-only |
| `memory_delete_page` | Delete a single exact-path page with admission hooks | destructive |
| `memory_forget_sweep` | Retention sweep (M8); soft-delete below cold threshold; `dry_run=true` previews | destructive |
| `memory_lint` | Rule-based + optional LLM contradiction findings → `wiki/_lint/<date>.md` | destructive |
| `memory_install_self_routing` | Returns the canonical slim CLAUDE.md / AGENTS.md routing block, managed Agent Skill payloads, target hints, and overwrite guidance | read-only |

Tool param aliases stay narrow: shipped aliases cover `query|q|search` and
`limit|n|top_k`; project and cwd parameters use canonical names unless the
code adds a concrete alias.

Context assembly is deliberately downstream of candidate generation. FTS,
optional vector search, link-neighbour expansion, and their RRF scores remain
the only ranking stack. The assembler owns breadth-before-depth selection,
three representation tiers, SHA-256 equivalence deduplication, per-kind
quotas, already-used ContextRefs, deterministic ordering, and UTF-8-byte budget
accounting. The only shipped ContextKinds are backed by current query sources:
wiki pages, session pages, and raw observations. Provider enrichment may alter
derived candidate material, but cannot write canonical markdown.

Managed engram Agent Skills are prompt packaging for this tool-routing
guidance only. They are installed as ordinary `SKILL.md` files so agents can
progressively load detailed instructions, but engram does not store durable
memory in them and does not include a runtime skill router. The managed
`engram-project-instruction-maintenance` Skill extends that same packaging to
the existing CLI/admin instruction workflow without extending the MCP list:
the always-loaded block carries only a short trigger, and the Skill requires
doctor → propose → review → explicit human approval → local apply. Proposal
state remains server-side, repository authority remains local, and Git stage,
commit, push, and merge are outside the workflow.

Project-instruction semantic assistance deliberately remains an authenticated
CLI/admin bridge rather than an MCP repository-write tool. Its provider is advisory:
the server supplies bounded authoritative evidence, requires exact citations,
and accepts at most one pending proposal. It has no repository handle. The
single writer actor persists review data only, and the human approval hash path
remains separate from Wiki auto-approval. This preserves the narrow tool
surface and prevents scheduler or model output from acquiring repository-write
authority.

Local project-instruction apply is an optimistic, fail-closed executor rather
than a Git reconciliation engine. Human approval binds one base snapshot and
ownership boundary; apply may write only when the canonical target, relevant
Git state, instruction graph, managed-marker structure, encoding, and approval
binding still agree. It never repairs with force, merge, rebase, or staging.
Routing and approved-rules are separate ownership domains, safe adapters resolve
to one canonical file, and failures are durable typed audit outcomes rather than
permission to retry against new bytes. Repository content and write authority
remain local; the server stores only application or diagnostic metadata through
the single writer actor.

## 11. Identity & project scoping (3-tuple from day one)

Lesson from basic-memory's v0.20 trauma: `(workspace, project, page_path)`. Even if v1 ships single-workspace, the schema and every API/tool param encodes the full 3-tuple. No retrofits.

Project resolution chain: explicit param → server's default → cwd-based heuristic (match repo root) → error.

**Install-time `project_strategy` default (#128).** `basename(cwd)` stays the v1 default, but an agent shell that `cd`s into a subdirectory and stays there silently forks the rest of the session into a phantom project named after the subdir. A `.engram.toml` marker with `project_strategy = "repo-root"` fixes this (#16, #23, #111) but needs a marker in (or above) every repo; a runtime env-var fallback that the *user* sets was deliberately rejected in #16. `install-hooks --project-strategy repo-root` instead **bakes** the strategy into the generated hook command (and the OpenCode / OMP / OpenClaw plugins) at install time — the same status as the already-baked `ENGRAM_AUTH_TOKEN` / `ENGRAM_HOOK_URL` / `--data-dir`, not a user runtime override. This is a client/install-time-only change: the server already parses `project_strategy=repo-root`. A marker's own `project_strategy` / `project` still win, and the default stays `basename` (baking nothing) so existing installs are byte-identical.

## 12. Operability

- **Single binary**, statically-linked where possible, shipped as native prebuilt archives for macOS aarch64 and Windows x86_64. **Absolute data path** by default (`dirs::data_local_dir().join("engram")`); log it loudly on startup (agentmemory #303 lesson).
- **Atomic config**: one `Config::load()` → typed struct, every reader takes `&Config`. No `process.env` double-read paths (agentmemory #456/#469).
- **Write durability**: accepted hook work awaits the SQLite write and appends a
  `log.md` line before that background task finishes. Indexes still commit in
  the same transaction as the data; no detached indexing task runs after the
  write ack (basic-memory #763/#578/#839).
- **Migrations**: `sqlx::migrate!` runs on startup; never inline DDL (basic-memory #727).
- **Schema versioning**: one source of truth for the schema; derived clients/docs. No "update 7 files" checklists (agentmemory AGENTS.md smell).
- **Backup/move**: `engram export <dir>` dumps wiki/ + sqlite snapshot. `engram import <dir>` consumes. Default data dir is portable. Optional: `auto_git_commit = true` config flag → commits the wiki directory on every `memory_lint` run.
- **Self-healing**: startup checks (`memory_diagnose`): vector dim/provider drift, FTS index corruption, orphan pages, broken links, zombie sessions. `memory_heal` auto-fixes the safe subset.
- **Logging**: structured `tracing` with rotating files, capped at N MB. No feedback loops (agentmemory #519).

## 12a. Task-continuity retention and accepted vs completed

Lease expiry is evaluated only at transition time (`claim_handoff`). There is
no sweeper and no unbounded background task that collects lapsed claims. A
claimed transfer whose live lease has elapsed becomes claimable again on the
next claim; `lapsed_awaiting_recovery` on the briefing snapshot reports that
count without mutating it.

Nothing deletes `handoffs`, `work_items`, `checkpoints`, or `handoff_claims`
rows except project purge. `forget_sweep` and `hard_delete_decayed` reach
wiki pages only. Historical transfers stay readable while their WorkItem is
retained, including `acknowledged`, `expired`, `cancelled`, and `superseded`
states.

An *accepted transfer* (`handoffs.acknowledged`) is the receiver's first
checkpoint on that Handoff. It says nothing about whether the underlying
WorkItem finished. Completed work is `work_items.completed`, written only by
an explicit checkpoint.

## 13. What we are explicitly NOT doing in v1

To stay scoped:

> Historical v1 boundary: later releases added the companion Tauri desktop
> workbench (project-scoped memory management). The headless engine remains
> usable without it, and the built-in web surface is still a read-only
> browser rather than an administrative dashboard.

- No multi-tenant auth/RBAC (single-user homelab).
- No web UI / dashboard (use `sqlite3` + `glow`/Obsidian).
- No Postgres backend (revisit if a real homelab user hits scale walls).
- No remote/cloud sync (use git remote on the wiki dir).
- No alternative embedded vector backends (sqlite-vec only).
- No alternative graph DB (SQL recursive CTEs only).
- No multimodal (text only).
- No general "skills" / slash-command bundle in v1 (agentmemory plugin format). The narrow exception is the managed engram Agent Skills that package routing guidance for agents; hooks + MCP remain the product surface.
- No LongMemEval-style benchmark harness in v1 - add in v0.4.

## 14. Mistakes-to-avoid checklist (from issue research)

Top-line rules carved into the codebase:

1. One config-read path (agentmemory #456/#469).
2. Indexes in the same txn as the source-of-truth row (agentmemory #204/#309, basic-memory #763/#578).
3. JSON-schema structured outputs, no XML (agentmemory #492/#539; cognee #2840).
4. Hooks fire-and-forget (agentmemory #221, #143).
5. No background-task index-after-return; either sync or `index_status: pending` (basic-memory #763).
6. 3-tuple identity from day one (basic-memory #783/#834).
7. Vector index records `{provider, model, dim}`; ignore stale vectors and warn on mismatch (agentmemory #469).
8. Embedding cache path absolute, not `/tmp` (basic-memory #741).
9. Watcher heartbeat + reconciliation pass (basic-memory #580/#758/#798).
10. Live-process check before destructive ops (basic-memory #765).
11. Per-provider typed HTTP client; no LiteLLM equivalent (cognee #2840).
12. Idempotent ingest with deterministic id derivation (cognee #2510/#2557/#2633).
13. Single transactional boundary; no implicit graph/vector/relational sync (cognee Section B).
14. Filter propagation tests (cognee #2720 was a recall correctness bug).
15. Default data dir is an absolute canonical platform path (agentmemory #303).
16. No `lru_cache` on configs (cognee #2228/#2853).
17. Datasets/projects are query-time filters, not orchestration-mode-conditional (cognee #2867).
18. LLM has off by default; opt-in via env (agentmemory #138/#143).
19. `cargo deny` for transitive license audits (cognee #2807 - FastEmbed removed for license).
20. Pin upstream native deps; ship a lockfile (agentmemory #555/#540).
