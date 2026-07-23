# Changelog

All notable changes to Engram are documented here.

Engram is a hard fork of
[akitaonrails/ai-memory](https://github.com/akitaonrails/ai-memory) at
v1.8.0. History up to the fork point is preserved verbatim in
[`docs/upstream-changelog.md`](docs/upstream-changelog.md); entries
below start from the fork.

## 2.0.0 - 2026-07-19

### Changed

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
