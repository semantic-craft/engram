# Changelog

All notable changes to Engram are documented here.

Engram is a hard fork of
[akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory) at
v1.8.0. History up to the fork point is preserved verbatim in
[`docs/upstream-changelog.md`](docs/upstream-changelog.md); entries
below start from the fork.

## Unreleased

### Added

- A WorkItem can now cross many agents and Runs without overwriting earlier
  transfers. `memory_handoff_begin` publishes a *successor* Handoff for an
  existing WorkItem, requiring the caller's `expected_work_item_revision` and
  `expected_checkpoint_revision` (the `work_item_revision` of the latest
  Checkpoint); either being stale is a conflict that mutates nothing, and
  concurrent successors at the same revision produce exactly one successor and
  one explicit conflict. A successor records its predecessor Handoff, the exact
  Checkpoint it was built from, and its source Run/Session, and atomically
  supersedes only the older transfer still sitting at `open`. Handoff state
  gains `superseded` and splits `cancelled` out of `expired`, so source
  cancellation, claim release, lease expiry, and supersession are four
  distinct, separately audited outcomes; `memory_handoff_cancel` now leaves a
  Handoff `cancelled`. `memory_handoff_discover` never delivers a superseded,
  cancelled, or expired transfer and returns `latest_checkpoint` — the
  outstanding acceptance criteria and the revision a successor must assert —
  plus `chain`: the WorkItem's ordered predecessor-to-successor history with
  source and receiving Run/Session provenance. A `completed` or `abandoned`
  WorkItem rejects further Handoffs — follow-up work creates a distinct
  WorkItem through the explicit `depends_on` / `derived_from` / `child_of`
  contract — and nothing deletes a predecessor that a successor, Checkpoint,
  ArtifactRef, ContextRef, or audit record still refers to. Terminal WorkItems
  are irreversible: a checkpoint can no longer move one back to `active` or
  `blocked`, reaching a terminal state retires any transfer still `open` in the
  same transaction, and claiming a transfer against finished work fails closed.

- The SessionStart continuation envelope now renders the *remaining*
  acceptance criteria (from the WorkItem's latest Checkpoint rather than the
  original wish list), a non-`active` WorkItem state, the transfer's
  ArtifactRef evidence, its explicit WorkItem relationships, and the
  predecessor Handoff plus Checkpoint revision it continues.

- Recoverable task continuity now models stable WorkItems separately from
  Runs/Sessions, revisioned Handoffs, lease-bound Claims, append-only
  Checkpoints, caller-supplied retry Attempts, and BackgroundJob identities.
  The MCP surface adds read-only `memory_handoff_discover`, compare-and-set
  `memory_handoff_claim`, explicit `memory_handoff_release`, and
  `memory_checkpoint_write`; identical lost-response retries replay the
  original result while mismatched Attempt reuse fails closed.

- `memory_context_read` resolves an exact ContextRef from `memory_query` to its
  full evidence through existing-scope, fail-closed reads. ContextRefs support
  the currently shipped wiki-page, session-page, and observation sources and
  never contain absolute storage paths.

- Handoffs and Checkpoints can carry typed ArtifactRefs (file, git, worktree,
  external) and explicit WorkItem relationships (`depends_on`, `derived_from`,
  `child_of`). Both write responses expose ArtifactRefs and relationships with
  stable identities and revisions. Artifact identity uses repository or project
  coordinates plus a repository-relative locator, or repository identity plus
  revision; dirty worktrees at the same commit stay distinct; absolute cwd is
  only a local-path hint and is rejected as a worktree locator. Observation
  metadata (source Run, timestamp, provenance, dirty, local-path hint, git
  ref, content hash, tree hash) is stored per attachment, so a later observer
  does not inherit the first writer's branch or hashes. Shared identity rows
  have no project-level CASCADE: deleting the first writer's project leaves
  other scopes' attachments and observations intact. The first receiving
  checkpoint with a live Claim may attach artifacts and relationships.
  SessionStart pending-handoff markdown lists those ArtifactRefs and
  relationships. Delivery facts such as changed, verified, committed, pushed,
  reviewed, merged, released, deployed, submitted, and approved are recorded
  independently and never inferred from one another. Related work creates a
  new WorkItem and does not inherit the prior claim or blockers. Relationship
  creation rejects unauthorized actors. A child can return structured evidence
  for its parent but cannot complete, abandon, claim, or supersede it. Engram
  records observed status and performs no Git or external mutation.

### Fixed

- Updated the `h2` crate to 0.4.19 to address RUSTSEC-2026-0258 (unbounded
  empty DATA frames). This unblocks `cargo-deny` and `cargo-audit` on main.

### Changed

- Handoff reads no longer consume or acknowledge work. The removed
  `memory_handoff_accept` contract is replaced by explicit claim and first
  receiver checkpoint acknowledgement; acknowledgement, Run end, Claim expiry,
  and Handoff expiry remain distinct from WorkItem completion. Existing
  open/accepted/expired Handoffs migrate without loss to
  open/acknowledged/expired histories.

- `memory_query` now requires a caller-supplied `context_budget` and returns a
  deterministic `ContextPackage` plus assembly trace instead of flat page and
  observation hit lists. Existing FTS, optional vector, and link-neighbour RRF
  remain candidate generation; the shared assembler deduplicates equivalent
  content, applies per-kind quotas, gives eligible candidates brief coverage
  before overview/full-evidence upgrades, and reports UTF-8-byte consumption,
  truncation, omissions, provenance, and stable revisioned ContextRefs without
  requiring an LLM provider. The desktop semantic-search client now supplies
  explicit budgets and maps navigable page entries from the package instead of
  parsing the removed flat hit arrays.

- `memory_handoff_begin` accepts a bounded brief and revisioned ContextRefs
  without copying canonical bodies into operational Handoff rows.
  `memory_handoff_claim` requires the same `context_budget` unit as
  `memory_query` and returns the continuation envelope plus that shared
  ContextPackage and assembly trace. Explicit refs carry a selection priority
  that puts them ahead of every retrieval candidate for quota and budget,
  whatever the retrieval rank; missing or unauthorized refs stay bounded
  diagnostics, and assembly never marks a Handoff accepted. The claim's
  assembly options are part of its Attempt identity, so only a byte-identical
  request replays. Compare-and-set failure does not
  return a package; assembly failure after a recorded claim leaves the live
  lease recoverable.

- Replaced the geometric `e` / node-mark branding with the original Paper
  Muse spark across the README, embedded web UI and favicon, and Tauri desktop
  bundles. The canonical SVG and its MIT provenance are preserved in `docs/`.

## Desktop 0.2.0 - 2026-08-01

### Added

- The desktop app is now a project-based memory hub with an island-style
  layout: a project switcher (default workspace), per-project dashboard
  (pending handoff, counts, health drill-down, core memory, recent
  pages), an L0–L3 memory-layers view, a kind-grouped knowledge base
  with distribution bars, a dedicated 全局记忆 area for the `_global`
  scope, machine-local instruction-file viewing (CLAUDE.md / AGENTS.md
  with engram managed-block highlighting), and scoped/global semantic
  search. The default window grew to 1360×880 with a global 1.12 zoom.
- A **会话与交接** (sessions & handoffs) view per project — a
  day-grouped session timeline (agent, duration, observation count, cwd,
  jump to the summary page; still-open sessions marked live) plus the
  handoff history with open/accepted/expired state. The handoff tab is a
  read-only audit view and never consumes a pending handoff. Against a
  daemon that predates the endpoints it explains that instead of raising
  an error.
- Pages can be moved between a project and the `_global` scope from the
  page view (copy-then-delete, so a failed write never loses the page),
  the dashboard's core-memory card now shows rule/slot bodies rather
  than titles alone, and the instruction-files view gained a
  **load-chain** panel listing the official precedence order (managed
  policy → user → project → project-local) with each layer marked present
  or unused on this machine.
- The instruction-files view can copy a paste-ready prompt scaffold for
  external agents (Claude Code / Codex / …) — per file or for the whole
  tab. The scaffold carries the files' absolute paths plus auto-generated
  guardrails (never touch the engram managed block; pointer CLAUDE.md
  routes edits to AGENTS.md) and ends with a "my request" slot the user
  fills in after pasting. The template is editable and stored locally; a
  copy-path button ships alongside.

### Fixed

- macOS desktop release checksum sidecars now record the downloadable DMG
  filename instead of a repository-relative build path, so users can verify
  both downloaded assets directly with `shasum -a 256 -c`.

## 2.2.0 - 2026-08-01

### Added

- New read-only endpoints
  `GET /api/v1/workspaces/{ws}/projects/{p}/sessions` and `…/handoffs`
  list a project's session timeline (agent, cwd, start/end, observation
  count, summary-page path) and its full handoff history in every state.
  Reading the handoff list never consumes an open handoff. Documented in
  `docs/frontend-api.md` §4.9.
- `GET /api/v1/projects` now includes each project's `repo_path` (the
  on-disk repository root recorded at project creation, `null` when
  unresolved), so local clients such as the desktop app can locate
  per-project instruction files. Documented in `docs/frontend-api.md`.

### Fixed

- Path-derived `kind` in the pages listing (`GET
  /api/v1/workspaces/{ws}/projects/{p}/pages`) now covers the full
  canonical taxonomy: `_slots/` → `slot`, `concepts/` → `concept`,
  `procedures/` → `procedure`, `notes/` → `note`, alongside the existing
  `_rules/`, `decisions/`, and `gotchas/` prefixes. Previously pages under
  the newly covered prefixes fell through to `fact` when their frontmatter
  carried no explicit `kind`.

- Stores created by engram 2.0.0 can now be opened by 2.1.0+ builds. V28–V30
  (project-instruction proposals/review/applications) shipped in 2.1.0
  numbered below the already-released V100/V101, so refinery refused to open
  any store whose history reached 101 without them — failing startup with a
  misleading "schema is newer than this engram build" error that actually
  described the opposite direction. Store open now backfills embedded
  migrations that sit below the store's high-water mark but were never
  applied, recording them in the migration history exactly as refinery would;
  genuinely newer stores are still rejected with the accurate
  `DataSchemaAhead` error. A regression test reproduces the exact 2.0.0
  store, and a tripwire test fails CI if a future migration is ever numbered
  below the released high-water mark again.

- `initialize` no longer forces every client down to MCP `2024-11-05`. The
  server hard-pinned the launch revision in `get_info()`, so a Claude Code
  client asking for `2025-11-25` — or any other agent asking for anything
  newer — was answered with `2024-11-05` and lost every protocol feature added
  since. The server now negotiates against the revisions it actually
  implements (`2024-11-05` through `2026-07-28`) and echoes the client's
  request, falling back to the SDK default only for unknown versions.

### Changed

- The MCP server now speaks the current specification. Upgraded the Rust MCP
  SDK from `rmcp` 1.7 to 3.1, which adds MCP `2026-07-28` — the stateless
  core: no `initialize` handshake, no `Mcp-Session-Id`, protocol version and
  client capabilities carried in each request's `_meta`, and a `server/discover`
  RPC advertising the server's supported revisions. Agents that stay on the
  older lifecycle are unaffected; both paths are served side by side.
- `serve --transport http --http-stateful` now applies only to clients that
  negotiate a pre-`2026-07-28` protocol version. MCP `2026-07-28` removed
  protocol-level sessions, so those requests are always served statelessly
  regardless of the flag. The flag name and default (stateless) are unchanged.
- The desktop app's MCP client and the generated Grok/PI extension bridge now
  request `2025-11-25` instead of `2024-11-05` / `2025-03-26`. Both still use
  the `initialize` handshake, which `2026-07-28` replaced, so `2025-11-25` is
  the highest revision they can claim.

## Desktop 0.1.1 - 2026-07-29

### Changed

- Refreshed the desktop npm and Rust lockfiles to current compatible releases,
  including Tauri plugin/runtime patch updates, while retaining the explicit
  `cookie` security override. The frontend audit remains vulnerability-free;
  the Linux-only Tauri v2 GTK3/`glib 0.18.5` upstream advisory remains tracked
  separately until gtk-rs publishes a compatible fix.

### Fixed

- Desktop release builds now distinguish unsigned local bundles from public
  artifacts. The macOS release gate requires an available Developer ID identity
  and Keychain-hosted notary profile, verifies path sanitization and signatures,
  notarizes and staples the DMG, mounts it for Gatekeeper acceptance, and emits
  a SHA-256 sidecar. Two current-toolchain Clippy findings were also corrected
  without changing behavior.

## 2.1.0 - 2026-07-29

### Added

- The managed routing package now includes
  `engram-project-instruction-maintenance`, a progressively disclosed Agent
  Skill for the existing doctor → propose → review → explicit human approval →
  local-apply workflow. The slim always-loaded block gains only a maintenance
  trigger; the Skill keeps proposal storage on the server, repository apply on
  the local host, and Git stage/commit/push/merge outside the workflow. The MCP
  surface remains the same 16 tools. Compiled-CLI black-box coverage now spans
  generic cleanup with protected project knowledge, provenance, CAS, backup,
  audit, Git-index preservation, idempotency, an LLM-disabled durable-rule
  path, managed-Skill installation, and remote MCP denial of repository write
  authority. Before any scoped proposal, review, approval, rejection, or apply
  command, the Skill follows lifecycle capture precedence from the closest
  working-directory ancestor marker through explicit scope and project
  strategy, then passes the effective workspace and project explicitly.
- `engram instructions doctor` provides a deterministic, read-only audit of
  Claude Code and Codex project instruction loading. Human-readable and JSON
  reports identify canonical, adapter, tool-specific, path-scoped, and
  override sources; show effective load order and size estimates; and diagnose
  routing marker health and managed-asset drift without changing files or
  initializing Engram runtime state. Other recognized harness files are
  explicitly reported as best-effort.
- Instruction-doctor reports now include evidence-backed context-placement
  diagnostics for generic harness guidance, exact or normalized duplication,
  contradictory rules, stale repository paths or commands, missing referenced
  project Skills, context-budget pressure, and likely wrong-layer content.
  Recommendations protect non-inferable project knowledge and identify root,
  path-rule, Agent Skill, Wiki, enforcement, or no-change destinations; file
  length alone never produces a removal recommendation.
- `engram instructions propose` stages proposal-only project instruction
  changes from an explicitly selected `_rules/` Wiki page or deterministic
  doctor finding. These DB-backed `project_instruction` records preserve the
  target layer, operation, base hash, exact anchor or owned region, proposed
  content, unified diff, token delta, rationale, evidence provenance, actor,
  and timestamps in the existing pending-writes review surface. Reviewers can
  now edit the proposed wording with server-side diff/token/hash recomputation;
  append-only revisions retain the original proposal and each editing actor.
  Independent, optimistic-hash-bound approval records a separate deciding actor
  and advances only the DB row to `approved`/apply-ready. Staging, editing,
  approval, and rejection never change repository files, Wiki pages, or Git
  state, and Wiki scheduler/provider/auto-approval settings cannot bypass the
  human gate.
  Durable-rule no-change detection requires a complete normalized
  instruction block, so contradictory or larger statements are not mistaken
  for the selected rule. Existing Wiki proposals retain their sidecar and Wiki
  mutation path.
- `engram instructions propose --semantic` adds opt-in, provider-backed
  duplication/conflict/placement assistance over authoritative durable Engram
  evidence: explicit or approved rule pages, repeated project user
  corrections, durable lint findings, and bound doctor findings. It enforces
  exact citations plus fixed provider-call, evidence, input/output, proposal,
  final-body, and changed-character budgets; filters assistant echo, external
  web/issue instructions, transient state, secrets, and unsafe protected-context
  removal; and records rejected model candidates in the existing rejection
  buffer. Accepted output still becomes only a DB-backed pending
  `project_instruction` proposal requiring the independent human approval
  binding. The deterministic no-LLM doctor/proposal paths are unchanged, the
  scheduler gains no approval/apply/repository-write authority, and no new MCP
  tool or repository-writing surface is added.
- `engram instructions apply <proposal-id>` is the first local-only mutation
  path for an approved project-instruction proposal. The CLI re-resolves the
  canonical repository target, verifies the staging checkout and target's hashed identity,
  recomputes the approval binding, checks the approved boundary and base
  SHA-256 on the final read, preserves Engram's routing block, and writes
  through a sibling tempfile plus a no-clobber handoff. Single-target apply accepts
  add/update/stale-delete/no-change but rejects move proposals until their
  destination mutation exists.
  Changed files move into an unpredictable private sibling recovery directory;
  no-change and repeated applies are idempotent, and a retry can finish a
  missing audit record only when an HMAC-authenticated, proposal-bound local
  receipt and its exact base backup prove the prior update.
  The single writer actor records proposing,
  approving, and applying actors, approval/before/after hashes, outcome, and
  backup path without granting the server or MCP any repository write access.
- Local project-instruction apply now fails closed across target concurrency,
  relevant dirty Git state, active merge/rebase-like operations, every malformed
  or overlapping routing/approved-rules marker shape, ambiguous anchors,
  managed Skill targets, unresolved/cyclic/external imports, unsafe symlinks or
  escaped paths, unsupported encodings/newlines, and write races. Safe
  in-repository adapters resolve to one canonical write and cannot be retargeted
  after approval; successful updates use exclusive recovery paths and
  preserve unowned bytes, permissions, newline style, and the Git index.
  Rejected attempts atomically become typed `conflict` or `failed` audit events
  with stable repair diagnostics and never use force/merge/rebase/staging
  fallbacks or record a false application. Routing refreshes now enforce the
  same disjoint marker structure and preserve approved-rules bytes.
  The local receipt path is exclusively reserved before mutation; receipt
  finalization failure restores the approved base with the same no-clobber
  protocol and retains recovery artifacts.

- Desktop: `npm run build:release` builds the distributable bundle with
  `--remap-path-prefix` so release binaries carry no absolute build
  paths (username leak), and fails if sanitization misses anything.
  Release DMGs must be built through this script.

### Fixed

- Wiki atomic writes now retry transient Windows sharing violations with
  bounded backoff, preventing a brief lock on a staged tempfile from failing
  an otherwise valid concurrent write.
- Updated `plist` to 1.10.0 and `quick-xml` to 0.41.0, removing the
  `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` XML denial-of-service
  advisories from the CLI release dependency graph.
- The release script now moves this changelog's canonical `## Unreleased`
  section into the new version entry and includes it in the annotated tag,
  instead of silently expecting bracketed headings that the file does not use.
- GitHub Release installation notes now describe the shipped macOS Apple
  Silicon archive accurately instead of advertising an x86_64 artifact that
  the release workflow does not build.
- The desktop SvelteKit toolchain now resolves `cookie` 0.7.2 through npm's
  supported override mechanism, removing the vulnerable transitive 0.6.0
  release without requiring an unrelated framework major-version upgrade.
- Local project-instruction apply now compares canonical repository and target
  paths in the same Windows verbatim-path form. A target inside the repository
  is no longer misclassified as escaping it solely because `canonicalize`
  returned a `\\?\`-prefixed target path.

## 2.0.0 - 2026-07-19

### Changed

- Public CI and release workflows now use only standard GitHub-hosted
  runners, immutable action SHAs, read-only default permissions, and
  one-day artifact retention. Pull-request jobs do not read repository
  secrets; full-history secret scanning downloads a checksum-pinned
  Gitleaks binary.

- Forked from ai-memory v1.8.0 and rebranded as **Engram**: crates
  (`engram-*`), binary (`engram`), env prefix (`ENGRAM_*`), default
  data dir (`…/engram`), packaging/service/docker references, and new
  logo. MCP tool names (`memory_*`) and the wiki/data format are
  unchanged — existing ai-memory data migrates by moving the data
  directory (see `docs/migrate-from-ai-memory.md`).

### Added

- **Long-document chunked embeddings** (migration V101). Wiki pages
  beyond one embedding request used to be head-truncated (OpenAI path,
  ~8 KB) or rejected with a 400 (Voyage), so a 150K wiki page had only
  its first ~2600 CJK chars in the vector index. `write_page` now embeds
  one vector per markdown-aligned chunk (~6 KB each, heading/paragraph
  boundaries, capped at 64 chunks ≈ 384 KB; still synchronous — no
  fire-and-forget), `page_embeddings` stores one row per
  `(page_id, chunk_index)`, and both hybrid vector legs (scoped and
  global) max-pool chunk scores so a page ranks by its best-matching
  chunk and appears once.
  Existing single-vector rows migrate as chunk 0; plain `engram embed`
  (no `--reembed`) re-embeds exactly the legacy head-truncated long
  pages, as does the scheduled backfill. Raising `MAX_DOC_CHUNKS` later
  is the one case that still needs `--reembed`: the skip predicate reads
  chunk *count*, which cannot distinguish a cap-truncated document from
  a fully covered one.
  `GET /admin/status` and `engram status` gain a page-granular
  `embedded_pages` counter — with chunking, the existing
  `embedding_rows` count exceeds the number of embedded pages, so
  coverage is now reported as `N pages (M rows); K latest pages
  missing`.

- Desktop Phase 2.5: **pending-writes approval workbench** (#13). A
  「审批台」 view aggregates pending auto-improve proposals across every
  project (client-side fan-out over the per-project `/admin/pending-writes`
  surface), renders per-proposal rationale + server-generated diff, and
  approves or rejects with an optional free-text reason that feeds the
  rejection context. Curator-staged report pages flow through the same
  queue. The desktop API client now also reads `ENGRAM_SERVER_URL` /
  `ENGRAM_AUTH_TOKEN` from `~/.config/engram/engram.env` (bearer auth on
  every request) and defaults its scope to the reserved `_global` project
  — fixing the post-migration 401s and the stale `agent-memory` default.

- **`engram-importer` Obsidian source.** New `obsidian` subcommand in the
  standalone importer companion: imports one vault folder into a destination
  path prefix with a recursive walk, original (including non-ASCII) file
  names, verbatim frontmatter passthrough via the `/admin/write-page`
  `frontmatter` map, deterministic short-name wikilink rewriting to
  root-relative paths (ambiguous/unresolved names left verbatim with a
  warning), title fallback to the file stem, and a repeatable `--tag` flag.
  Same dry-run-by-default safety contract as the OMC source.

- **Chinese full-text search without embeddings** (#14). A trigram
  shadow FTS index (`pages_fts_cjk`, migration V100) plus a per-term
  query router: non-CJK terms keep unicode61 word semantics, CJK terms
  of ≥3 chars get trigram substring MATCH with bm25 + snippets, and 1–2
  char CJK terms (the most common Chinese query shape, which trigram
  cannot match by design) fall back to a LIKE leg with Rust-built
  highlighted snippets — a leading-wildcard scan over `pages` with no
  supporting index, which measures sub-10 ms at personal-wiki scale. Multi-leg results are RRF-fused
  (k=60); single-FTS-leg queries (pure non-CJK, or pure ≥3-char CJK)
  keep their raw FTS5 ranks and previous behavior, while a LIKE-only leg
  has no bm25 rank and is recency-ordered. Applies to `memory_query` (scoped, `scopes`, `global=true`,
  and the hybrid FTS leg), `/api/v1/search`, `/web`, and `engram
  search` — previously all of these returned zero FTS hits for any
  Chinese query because unicode61 tokenizes a CJK run as one token.
  Migration V100 opens the fork-local migration band (upstream owns
  V1–V99). A regression test guards that the bundled SQLite keeps
  shipping the trigram tokenizer.

  The raw-observation fallback (`raw_hits`, used when compiled pages miss
  entirely) is CJK-aware too, but deliberately WITHOUT a trigram shadow:
  measured on a 670 MB deployment the raw log is 114 M chars against the
  wiki's 7.6 M, so a shadow would cost ~190–340 MB to accelerate a path
  that rarely runs. Its CJK terms take the LIKE leg directly, which rides
  `idx_observations_project_created` — 48 ms scoped to a project versus
  1.0 s unscoped.

  Known limitation: `raw_hits` is gated on the page search returning
  nothing, and the hybrid vector leg returns its k nearest neighbours
  with no minimum-similarity floor — so once an embedder is configured
  the page hits are effectively never empty and this fallback rarely
  fires through `memory_query`. It is reachable through the FTS-only
  surfaces. Giving the vector leg a relevance floor is not a one-line
  change: measured on a CJK-heavy corpus the embedder separates by
  script more strongly than by meaning, so no single cosine cutoff
  cleanly divides noise from genuine hits.

- Ported from upstream ai-memory v1.9.0–v1.14.0 (cherry-picks with
  preserved authorship; `Upstream-Commit` trailers name the source):
  - **Reserved `_global` preferences scope** (upstream v1.9.0): write
    standing user/team context with `memory_write_page scope:"global"`
    (or `engram write-page --scope global`); default-scoped
    `memory_query` calls union it into every project as
    `global_scope_hits` via one extra scoped hybrid search (FTS +
    vector when an embedder is configured). Lifecycle hooks never
    create or attribute captures to `_global`.
  - **Marker-opted session-start project brief** (upstream v1.12.0):
    `.engram.toml` `[briefing] inject_on_session_start = "true"` (+
    optional `max_chars`, clamped 500–20000, default 4000) appends a
    compiled project brief — pinned / `_rules/` / `_slots/` bodies plus
    recent page titles — after any pending handoff at session start.
    Off by default. The upstream `[recall] default_global` marker was
    deliberately NOT ported; its forwarding hooks are stripped.
- V27 migration (upstream v1.11.0) repairs historical fragment
  attribution; migration numbering intentionally leaves V26 unused
  (upstream V26/V28 are agent-kind migrations engram does not need).

- Desktop app (`apps/desktop`, Tauri v2 + Svelte), Phase 1: local
  workbench — Chinese-capable semantic search via MCP `memory_query`,
  directory tree, page view with frontmatter and backlinks, daemon
  status bar. Read-only at this phase; Phase 2 below adds writes.

- Desktop Phase 2: **page editing and local daemon management.**
  `PageView` gains an in-place markdown editor (edit / save / cancel,
  two-step delete) and the sidebar footer a new-page form; both route
  through `/admin/write-page`, so a body edit keeps the page's kind,
  tier, tags, pin and custom frontmatter, and a save triggers an
  embedding backfill. A new `MachinesPanel` starts and stops the local
  daemon through launchd — probing both the `engram` and the pre-rename
  `ai-memory` launch-agent labels, so a migrated install is still
  managed — and surfaces admin status plus the health block with embed
  (dry-run then run), forget-sweep and backup; the backup tarball lands
  in the OS download directory under a timestamped name. Restore stays
  CLI-only: `engram restore` requires the daemon stopped and there is no
  full-restore admin endpoint. Errors surface in a dismissible global
  banner instead of the console only. Non-macOS builds get
  daemon-management stubs.
- `POST /admin/write-page` accepts an optional `frontmatter` object
  that becomes the authoritative frontmatter base (dedicated fields
  `title`/`kind`/`tags`/`pinned`/`tier` still override their keys).
  The desktop app gains the matching `write_page` command / `writePage`
  API that rounds the frontmatter read from a page back through it.

- **2.0.0 ships macOS (Apple Silicon) and Windows x86_64.** Linux, the Docker
  image and the two AUR packages were dropped as release targets — all three
  inherited from upstream ai-memory and unused here (`docker/`, `packaging/`
  were removed). Intel macOS is not built either: the fleet and the users this
  ships to are all Apple Silicon, and an Intel Mac can run the arm64 binary
  under Rosetta if ever needed.

  The CI and release workflows run on GitHub-hosted runners (macOS
  Apple Silicon + Windows x86_64); building and publishing a release
  needs no self-hosted infrastructure.

### Fixed

- Auto-improve staging no longer loses a whole review run to one bad
  proposal. `stage_run` records every proposal in one transaction, so a
  per-proposal guard failure — most often the one-pending-proposal-per-
  target unique index — aborted the entire run, discarded its valid
  proposals, and left the session already claimed (never retried). With
  `[auto_improve] require_approval = true` the queue holds proposals
  indefinitely, so a later review proposing the same page is routine and
  this fired on most scheduler ticks. Colliding or raced proposals are
  now skipped and reported (`skipped` on the staged run, in the
  scheduler log and the `memory_auto_improve` response) while their
  batch mates land. **Behavior change:** duplicate targets — within one
  batch or against an already-queued proposal — keep the first proposal
  instead of rolling the run back.
- `engram-importer` no longer abandons a vault when a page write times
  out. `Wiki::write_page` commits the page row and the on-disk file
  before it awaits the embedding, so a write that timed out mid-embed
  had still landed — but the importer recorded it `failed`, aborted the
  remaining pages, and wrote a manifest claiming the destination was
  never written, so the re-run needed `--overwrite` to get past it. On
  timeout the importer now re-probes the destination: page present → a
  new `uncertain` manifest status and the import continues; page absent
  → `failed` and abort, unchanged; the probe itself erroring → also
  `uncertain`, but the run bails, since nothing is known either way.
  Timeouts are classified through the anyhow source chain
  (`reqwest::Error::is_timeout`), so an explicit rejection is never
  mistaken for an unknown, and `uncertain` records neither page id nor
  checkpoint rather than fabricating the fields that would let it claim
  `written`. Write-page also moves off the shared 30 s client onto its
  own `--write-timeout-secs` budget (default 300 s); preflight, list and
  exists keep 30 s. The defect predates chunking — one slow embed call
  could always blow 30 s — but the server embeds synchronously, one
  provider call per markdown chunk, so a chunked page is dozens of
  round-trips inside a single request and raises the worst case by up to
  64x.

- `memory_write_page` and `engram write-page` preserve custom
  frontmatter and tier too. The `/admin/write-page` entry below covers
  only the HTTP surface; the MCP tool path — the one agents actually
  write through — still merged a body-only edit onto an empty
  frontmatter map, so custom keys on migrated pages (`source`, `status`,
  `type`, …) were dropped, and an omitted tier reset the page to
  `semantic`. Both now merge onto the existing page's frontmatter, the
  CLI `--tier` flag and the MCP `tier` argument became optional (absent
  → keep the current tier, `semantic` for new pages), and a stale
  `last_modified_by` block is dropped so attribution is re-stamped from
  the resolved actor.

- The auto-improve scheduler no longer loses whole ticks to a
  hallucinated base-body hash. `expected_base_body_sha256` is computed
  by engram in `validate_patch_proposal` from the target page, but it
  lived on `AutoImproveProposal`, which derives `JsonSchema` and is
  handed to the model as the structured-output schema — so the model was
  invited to supply a field it has no way to know, and on `full_page`
  proposals (which `validate_patch_proposal` never overwrites) the
  invented value survived into `scheduled_auto_improve_new_proposals`.
  There `hex_to_sha256` rejected it and the `?` failed the entire run
  for that scope, discarding every other valid proposal in the batch:
  42 of 446 production ticks (~11%) died this way with `invalid
  expected_base_body_sha256: expected 64 hex chars` or `invalid digit
  found in string`. The field is now `#[schemars(skip)]`, removing it
  from the schema the model sees, and `#[serde(skip_deserializing)]`, so
  a model that emits it anyway is ignored. The per-proposal skip above
  does not cover this: that guard runs at staging, this failure happens
  before it.

- `memory_query` with `global=true` now uses the same hybrid retrieval
  as scoped queries: when an embedder is configured, cross-project FTS5
  results are RRF-fused with cosine similarity over every latest page's
  stored embedding. Previously the global path was FTS-only, so any
  query the unicode61 tokenizer cannot match — notably every Chinese
  query — silently returned zero `global_hits` even with embeddings
  present (#10). Zero-embedder installs keep the previous FTS-only
  behavior.
- Upstream fixes ported alongside the batch: resumed sessions re-run
  the end path when they re-end with new observations (v1.9.0);
  mid-session events inherit the session's project instead of deriving
  a fresh one from per-event cwd, and the maintenance scheduler sweeps
  hollow zero-data project rows older than seven days (v1.11.0);
  auto-improvement's "never rewrite pinned pages" invariant is enforced
  in code at the apply point (v1.11.1); opening a store whose schema is
  newer than the running binary fails with an actionable error naming
  the offending migration (v1.14.0); native and POSIX hook helpers
  percent-encode query values with an RFC 3986 allow-list so Windows
  cwds survive, and session-start handoff fetch failures warn on stderr
  instead of masquerading as "no pending handoff" (v1.14.0).
- `POST /admin/write-page` no longer rebuilds frontmatter from scratch
  on every write: when the request carries no `frontmatter`, the
  existing page's frontmatter is reused as the base, so body-only edits
  preserve custom keys (`source`, `status`, `type`, `updated`, … on
  qmd-migrated pages) as well as the page's tier, pin, tags, and title.
  `tier` is now optional accordingly (absent → keep current tier, or
  `semantic` for new pages).
