//! axum router exposing `POST /hook`.
//!
//! Returns 202 immediately unless the in-flight hook limit is saturated,
//! in which case it returns 429. Heavy work (DB writes, session-page
//! synthesis) happens *after* the response is sent — but we still `await`
//! the writer ack to honour the cross-cutting invariant that "indexes commit
//! in the same transaction as the data" (no background-task-indexing-after-return,
//! basic-memory #763). The agent never blocks on us thanks to the
//! fire-and-forget client side.

use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use engram_consolidate::Consolidator;
use engram_core::{
    ActiveProject, ActorKey, AgentKind, DEFAULT_WORKSPACE_NAME, Handoff, NewHandoff,
    NewObservation, NewSession, ObservationKind, ProjectId, Sanitized, Sanitizer, SessionId,
    WorkspaceId,
};
use engram_store::WriterHandle;
use engram_wiki::Wiki;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::log;
use crate::payload::{
    HookEnvelope, HookEvent, HookQuery, ProjectStrategy, body_is_subagent, parse_agent,
};
use crate::synth::synthesize_session_page;

/// Default maximum number of hook events allowed to be processing at once.
///
/// This matches the writer queue order of magnitude and prevents unbounded
/// background tasks during tool-heavy bursts. Saturated servers return 429 so
/// callers can drop or retry instead of growing memory without bound.
pub const DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT: usize = 1024;

/// Maximum events accepted in one `POST /hook/batch` request. This matches the
/// client drain cap so a single request cannot monopolize ingest capacity or
/// allocate/process an unbounded vector of hook events.
pub const MAX_HOOK_BATCH_ITEMS: usize = 256;

/// Maximum cwd-resolution cache entries kept per server process. The cache is
/// an optimization only; evicted entries are re-resolved through the writer.
pub const DEFAULT_PROJECT_CACHE_MAX_ENTRIES: usize = 4096;

/// Cap on scoped session ids tracked as subagents for the
/// `drop_subagent_captures` tail-drop. Mirrors the project-cache order of
/// magnitude: enough for high fan-out harnesses, still bounded if a client never
/// sends a terminal `SessionEnd`.
const SUBAGENT_SESSIONS_MAX: usize = 4096;

/// Resolved-project cache key:
/// `(cwd, workspace_override, project_override, project_strategy)`.
pub type ProjectCacheKey = (String, String, String, String);

/// Shared bounded resolved-project cache.
pub type ProjectCache = Arc<tokio::sync::Mutex<ProjectCacheStore>>;

/// Bounded cwd-resolution cache used by the hook router.
#[derive(Debug)]
pub struct ProjectCacheStore {
    entries: HashMap<ProjectCacheKey, (WorkspaceId, ProjectId)>,
    order: VecDeque<ProjectCacheKey>,
    max_entries: usize,
}

impl Default for ProjectCacheStore {
    fn default() -> Self {
        Self::new(DEFAULT_PROJECT_CACHE_MAX_ENTRIES)
    }
}

impl ProjectCacheStore {
    #[must_use]
    fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            max_entries: max_entries.max(1),
        }
    }

    fn get(&mut self, key: &ProjectCacheKey) -> Option<(WorkspaceId, ProjectId)> {
        let ids = self.entries.get(key).copied()?;
        self.touch(key);
        Some(ids)
    }

    fn insert(&mut self, key: ProjectCacheKey, ids: (WorkspaceId, ProjectId)) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), ids);
            self.touch(&key);
            return;
        }
        self.entries.insert(key.clone(), ids);
        self.order.push_back(key);
        while self.entries.len() > self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn remove(&mut self, key: &ProjectCacheKey) {
        self.entries.remove(key);
        self.order.retain(|k| k != key);
    }

    #[must_use]
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    #[cfg(test)]
    fn contains_key(&self, key: &ProjectCacheKey) -> bool {
        self.entries.contains_key(key)
    }

    #[cfg(test)]
    fn values(&self) -> impl Iterator<Item = &(WorkspaceId, ProjectId)> {
        self.entries.values()
    }

    /// Retain only cache entries that match `keep`.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&ProjectCacheKey, &(WorkspaceId, ProjectId)) -> bool,
    {
        self.entries.retain(|key, ids| keep(key, ids));
        self.order.retain(|key| self.entries.contains_key(key));
    }

    fn touch(&mut self, key: &ProjectCacheKey) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
    }
}

/// Shared bounded set of scoped session keys known to belong to a SUBAGENT.
pub type SubagentSessions = Arc<tokio::sync::Mutex<SubagentSessionSet>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SubagentSessionKey {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
}

/// Tracks the scoped session keys of subagent (nested/spawned) sessions so that
/// the `drop_subagent_captures` gate can also drop the **unmarked tail** of those
/// sessions (`user_prompt_submit` / `stop` / `session_end`), which the
/// per-event marker (`subagentType` / `agent_type`) does not cover. A session
/// is seeded when a `SubagentStart` or any marker-bearing event arrives, and
/// forgotten on `SessionEnd` after the tail has been dropped. Bounded LRU so a
/// missed terminal event cannot leak memory.
#[derive(Debug)]
pub struct SubagentSessionSet {
    ids: HashSet<SubagentSessionKey>,
    order: VecDeque<SubagentSessionKey>,
    max: usize,
}

impl Default for SubagentSessionSet {
    fn default() -> Self {
        Self {
            ids: HashSet::new(),
            order: VecDeque::new(),
            max: SUBAGENT_SESSIONS_MAX,
        }
    }
}

impl SubagentSessionSet {
    /// Mark a scoped session id as a subagent (idempotent). Refreshes recency
    /// and evicts the oldest id once the cap is exceeded.
    fn insert(&mut self, key: SubagentSessionKey) {
        if self.ids.contains(&key) {
            self.touch(&key);
            return;
        }
        self.ids.insert(key);
        self.order.push_back(key);
        while self.ids.len() > self.max {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Whether this scoped session id is a known subagent.
    #[must_use]
    fn contains(&self, key: &SubagentSessionKey) -> bool {
        self.ids.contains(key)
    }

    /// Forget a scoped session id (after `SessionEnd`).
    fn remove(&mut self, key: &SubagentSessionKey) {
        if self.ids.remove(key) {
            self.order.retain(|k| k != key);
        }
    }

    fn touch(&mut self, key: &SubagentSessionKey) {
        self.order.retain(|k| k != key);
        self.order.push_back(*key);
    }
}

/// Shared state passed to the hook handler.
#[derive(Clone)]
pub struct HookState {
    /// Default workspace to use when a hook event lacks a `cwd` field.
    pub workspace_id: WorkspaceId,
    /// Default project to use when a hook event lacks a `cwd` field.
    pub project_id: ProjectId,
    /// Writer actor handle.
    pub writer: WriterHandle,
    /// Reader pool — needed for session-end synthesis.
    pub reader: engram_store::ReaderPool,
    /// Wiki handle — used to write the session-summary page.
    pub wiki: Wiki,
    /// Optional LLM-driven consolidator. When set, PreCompact uses it
    /// to refresh `sessions/<id>.md` before the agent loses its
    /// working context. When `None`, falls back to the deterministic
    /// rule-based synth (still useful, just lower-signal).
    pub consolidator: Option<Arc<Consolidator>>,
    /// Privacy strip applied to every observation before it lands in
    /// the store. Same handle is also held by the wiki and consolidator
    /// so scrubbing happens at every write boundary.
    pub sanitizer: Sanitizer,
    /// Cache of `(cwd, workspace_override, project_override, project_strategy) → ids`.
    /// The composite key avoids poisoning between callers that resolve
    /// the same `cwd` with and without an override during a hook-script
    /// upgrade window. Each tuple element defaults to the empty string
    /// when absent so missing overrides collapse into a single slot.
    pub project_cache: ProjectCache,
    /// Pointer shared with the MCP server. Every cwd-resolved event
    /// publishes its project here so the read tools (which have no cwd
    /// of their own) default to the project the user is actually in
    /// rather than the server's static `--project` (issue #2).
    pub active_project: ActiveProject,
    /// In-flight hook processing limiter. Requests acquire one permit before
    /// spawning work and return 429 immediately when saturated.
    pub ingest_semaphore: Arc<tokio::sync::Semaphore>,
    /// Opt-in (`ENGRAM_CONSOLIDATE_ON_SESSION_END`): when true and a
    /// `consolidator` is present, SessionEnd also runs LLM consolidation on
    /// top of the always-written heuristic session page. Off by default so
    /// session close stays cheap; the LLM checkpoint otherwise happens on
    /// PreCompact and via manual `memory_consolidate`.
    pub consolidate_on_session_end: bool,
    /// Scoped session keys known to be subagents (seeded by `SubagentStart` / any
    /// marker-bearing event). For a project that opted into
    /// `drop_subagent_captures` (via its `.engram.toml`, forwarded as the
    /// per-event `drop_subagent` flag), every event of a tracked session is
    /// dropped — closing the unmarked tail
    /// (`user_prompt_submit`/`stop`/`session_end`) the per-event marker misses.
    pub subagent_sessions: SubagentSessions,
    /// Operator home directory, sourced from `Config` once at startup. The
    /// cwd->project resolver never prefix-matches a stored `repo_path` equal
    /// to this, so `$HOME` cannot become a catch-all (issue #103). `None`
    /// disables the guard. Held here so the hooks crate makes no env reads.
    pub home_dir: Option<String>,
}

/// Build a router with `POST /hook` (event ingress) and `GET /handoff`
/// (synchronous handoff-fetch for session-start hooks).
pub fn hook_router(state: HookState) -> Router {
    Router::new()
        .route("/hook", post(handle_hook))
        .route("/hook/batch", post(handle_hook_batch))
        .route("/handoff", get(handle_handoff))
        .with_state(Arc::new(state))
}

async fn handle_hook(
    State(state): State<Arc<HookState>>,
    Query(query): Query<HookQuery>,
    actor_ext: Option<axum::Extension<engram_core::ActorContext>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let env = HookEnvelope::from_query_and_body(query, body);
    // Accept-but-drop subagent captures (incl. the unmarked tail of tracked
    // subagent sessions) when the operator opts in. Returning 202 (not an error)
    // means the client treats the event as delivered and never retries/spools
    // it. Runs before the semaphore so a dropped event consumes no capacity.
    // The auth middleware in front of `/hook` injects the request's
    // [`ActorContext`] (rung 1 root, rung 2 DB user, or anonymous). We
    // capture its `user` field NOW — before the spawn drops the request
    // extensions — so `process()` can key the `ActiveProject` map by the
    // authenticated identity when `[auto_scope] mode = per_actor` is on.
    let actor_user = actor_ext
        .map(|axum::Extension(ctx)| ctx.user)
        .unwrap_or_default();
    let actor_key = ActorKey {
        user: actor_user.clone(),
        session_id: env.session_id.clone(),
    };
    if should_drop_subagent(&state, &env, &actor_key).await {
        return (StatusCode::ACCEPTED, "subagent capture dropped");
    }
    let Ok(permit) = state.ingest_semaphore.clone().try_acquire_owned() else {
        warn!("hook ingest saturated; dropping event with 429");
        return (StatusCode::TOO_MANY_REQUESTS, "hook queue full");
    };
    tokio::spawn(async move {
        let _permit = permit;
        process_envelope(state, env, actor_user).await;
    });
    (StatusCode::ACCEPTED, "queued")
}

/// One event in a `POST /hook/batch` request — the same `{url, body}` pair a
/// single `POST /hook` would carry, so the server reuses the per-event query
/// parsing instead of inventing a second wire shape.
#[derive(Debug, Deserialize)]
pub struct HookBatchItem {
    /// Full hook URL including the `?event=…&agent=…` query (as the client
    /// spooled it); only the query is read here — the host/path are the
    /// client's record of where the event was bound.
    pub url: String,
    /// Raw JSON event payload.
    #[serde(default)]
    pub body: serde_json::Value,
}

/// Response to `POST /hook/batch`: the length of the contiguous leading prefix
/// of items that committed (equals the request length on full success).
#[derive(Debug, Serialize)]
pub struct HookBatchAck {
    /// Items committed, oldest-first. The client deletes exactly this many
    /// spool entries and re-sends the rest next pass.
    pub accepted: usize,
}

/// Batch sibling of [`handle_hook`]. Accepts many spooled events in ONE request
/// so a draining client amortizes the per-request cost (TLS + network RTT + the
/// edge auth hop) over the whole batch instead of paying it per event — the
/// dominant cost when a backlog drains to a remote, gated server, and the reason
/// a sequential per-event drain falls behind under parallel load.
///
/// Unlike `handle_hook` (which spawns and answers `202` immediately), the batch
/// is processed INLINE and FAIL-FAST: items run in order and the first error
/// stops the batch, returning `accepted = <committed prefix>`. Inline + fail-fast
/// keeps every item's side effects (a SessionEnd writes a session page + a
/// handoff) inside the response window, so a partially-applied batch never
/// commits beyond what `accepted` reports — the client deletes only that prefix.
async fn handle_hook_batch(
    State(state): State<Arc<HookState>>,
    actor_ext: Option<axum::Extension<engram_core::ActorContext>>,
    Json(items): Json<Vec<HookBatchItem>>,
) -> impl IntoResponse {
    if items.len() > MAX_HOOK_BATCH_ITEMS {
        warn!(
            items = items.len(),
            max = MAX_HOOK_BATCH_ITEMS,
            "hook batch too large; rejecting before processing"
        );
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(HookBatchAck { accepted: 0 }),
        );
    }
    // All items in a batch share the drain's single identity, so the actor is
    // captured once from the batch request (mirrors `handle_hook`).
    let actor_user = actor_ext
        .map(|axum::Extension(ctx)| ctx.user)
        .unwrap_or_default();
    let mut accepted = 0usize;
    for item in items {
        let query = parse_hook_query(&item.url);
        let env = HookEnvelope::from_query_and_body(query, item.body);
        let actor_key = ActorKey {
            user: actor_user.clone(),
            session_id: env.session_id.clone(),
        };
        // Accept-but-drop subagent captures (see `handle_hook`): count the item
        // as committed so the client clears it from its spool, but do not store
        // it. Keeps the contiguous-prefix ack contract intact.
        if should_drop_subagent(&state, &env, &actor_key).await {
            accepted += 1;
            continue;
        }
        // Mirror single `/hook`: subagent drops consume no ingest capacity, and
        // only events we will actually process acquire a permit. The batch loop
        // is sequential, so one permit held across this item preserves the
        // server-wide in-flight bound without making all-droppable batches 429
        // under saturation.
        let Ok(permit) = state.ingest_semaphore.clone().try_acquire_owned() else {
            warn!(accepted, "hook batch ingest saturated; rejecting with 429");
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(HookBatchAck { accepted }),
            );
        };
        let _permit = permit;
        if let Err(e) = process(&state, env, actor_user.clone()).await {
            warn!(error = %e, accepted, "hook batch item failed; stopping (fail-fast)");
            break;
        }
        accepted += 1;
    }
    (StatusCode::OK, Json(HookBatchAck { accepted }))
}

/// Decide whether to accept-but-drop this event under `drop_subagent_captures`,
/// maintaining the subagent-session set. Returns `true` to drop. Seeds the
/// session on `SubagentStart` and on any marker-bearing event; keeps it through
/// `SubagentStop`; and drops the **unmarked tail** (`user_prompt_submit` /
/// `stop` / `session_end`) of a session already known to be a subagent. No-op
/// (returns `false`) unless this event's project opted in via the per-event
/// `drop_subagent` flag (sourced from its `.engram.toml`).
async fn should_drop_subagent(state: &HookState, env: &HookEnvelope, actor: &ActorKey) -> bool {
    if !env.drop_subagent_requested {
        return false;
    }
    let Ok(session_id) = resolve_session_id(env) else {
        return false;
    };
    let Ok((workspace_id, project_id)) = resolve_project_ids(
        state,
        env.cwd.as_deref(),
        env.workspace_override.as_deref(),
        env.project_override.as_deref(),
        env.project_strategy,
        actor,
    )
    .await
    else {
        return false;
    };
    let key = SubagentSessionKey {
        workspace_id,
        project_id,
        session_id,
    };
    let marked = matches!(
        env.event,
        HookEvent::SubagentStart | HookEvent::SubagentStop
    ) || body_is_subagent(&env.raw);

    if marked {
        state.subagent_sessions.lock().await.insert(key);
        return true;
    }

    let tracked = state.subagent_sessions.lock().await.contains(&key);
    if tracked && matches!(env.event, HookEvent::SessionEnd) {
        state.subagent_sessions.lock().await.remove(&key);
    }
    tracked
}

/// Parse the `?event=…&agent=…` query of a spooled hook URL into [`HookQuery`],
/// mirroring axum's `Query` extractor (both use `serde_urlencoded`). A URL with
/// no query, or an unparseable one, yields the default query; downstream
/// fail-fast batch handling decides whether that default envelope can be stored.
fn parse_hook_query(url: &str) -> HookQuery {
    let qs = url.split_once('?').map_or("", |(_, q)| q);
    serde_urlencoded::from_str(qs).unwrap_or_default()
}

/// Query params for `GET /handoff`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HandoffQuery {
    /// Identifier of the agent fetching the handoff. Used to mark the
    /// handoff as accepted-by; defaults to `Other` if unrecognised.
    pub agent: Option<String>,
    /// Optional cwd filter. When provided, only handoffs whose stored
    /// cwd matches this string are returned. Note: the cwd string is
    /// not canonicalized; symlinked paths must match byte-for-byte.
    pub cwd: Option<String>,
    /// Workspace override (mirror of `HookQuery.workspace`). Lets the
    /// `session-start` hook fetch the handoff for the same `(workspace,
    /// project)` pair the marker file declared, without depending on
    /// the MCP `active_project` cache (which only populates after the
    /// first hook event of the session).
    pub workspace: Option<String>,
    /// Project override (mirror of `HookQuery.project`).
    pub project: Option<String>,
    /// Project strategy (mirror of `HookQuery.project_strategy`).
    pub project_strategy: Option<String>,
    /// Per-repo opt-in for the session-start project brief, forwarded by
    /// the host-side hook from `.engram.toml`'s
    /// `[briefing] inject_on_session_start`. A truthy value makes this
    /// endpoint append a compiled, char-budgeted brief of the project's
    /// pinned / `_rules/` / `_slots/` pages after any pending handoff, so
    /// the agent starts with the architecture context instead of
    /// re-exploring the codebase (#176). Off when absent.
    pub briefing: Option<String>,
    /// Char budget for the brief, forwarded from the marker's
    /// `[briefing] max_chars`. Clamped server-side to
    /// [`BRIEF_BUDGET_MIN`], [`BRIEF_BUDGET_MAX`]; defaults to
    /// [`BRIEF_BUDGET_DEFAULT`] when absent or unparsable.
    pub briefing_budget: Option<String>,
}

/// Synchronous endpoint used by `session-start.sh` to discover any
/// pending handoff from a previous agent. Returns plain text Markdown
/// (or an empty body when no handoff is open) with a 1-second cap on
/// the server side so the agent never blocks measurably on startup.
///
/// Side effect: when a handoff is found, it is *marked accepted* before
/// the response is sent. Two agents starting in parallel therefore
/// race; whichever arrives first wins. That is intentional — handoffs
/// are 1:1, not broadcast.
async fn handle_handoff(
    State(state): State<Arc<HookState>>,
    Query(query): Query<HandoffQuery>,
    actor_ext: Option<axum::Extension<engram_core::ActorContext>>,
) -> impl IntoResponse {
    let actor_user = actor_ext
        .map(|axum::Extension(ctx)| ctx.user)
        .unwrap_or_default();
    match fetch_and_accept_handoff(&state, query, actor_user).await {
        Ok(Some(markdown)) => (StatusCode::OK, markdown),
        Ok(None) => (StatusCode::OK, String::new()),
        Err(e) => {
            warn!(error = %e, "handoff fetch failed");
            (StatusCode::OK, String::new())
        }
    }
}

async fn fetch_and_accept_handoff(
    state: &HookState,
    query: HandoffQuery,
    actor_user: Option<String>,
) -> anyhow::Result<Option<String>> {
    let agent = query.agent.as_deref().map_or(AgentKind::Other, parse_agent);
    // `/handoff` has no session_id in the request — `per_session` mode
    // therefore falls back to the single slot (graceful degradation),
    // while `per_actor` keys by `user` alone.
    let actor_key = engram_core::ActorKey {
        user: actor_user,
        session_id: None,
    };
    let (ws, proj) = resolve_project_ids(
        state,
        query.cwd.as_deref(),
        query.workspace.as_deref(),
        query.project.as_deref(),
        ProjectStrategy::parse(query.project_strategy.as_deref()),
        &actor_key,
    )
    .await?;
    let handoff_md = {
        let handoff = state
            .reader
            .latest_open_handoff(ws, proj, query.cwd)
            .await?;
        match handoff {
            Some(h) => {
                state.writer.accept_handoff(h.id, agent, None).await?;
                Some(render_handoff_markdown(&h))
            }
            None => None,
        }
    };
    // The brief is additive and non-destructive: unlike the handoff (a
    // single-use slot consumed above), it is recomposed on every opted-in
    // session start — exactly what a Claude Code `/clear` needs (#176).
    let brief_md = if crate::payload::query_flag_truthy(query.briefing.as_deref()) {
        let budget = query
            .briefing_budget
            .as_deref()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(BRIEF_BUDGET_DEFAULT)
            .clamp(BRIEF_BUDGET_MIN, BRIEF_BUDGET_MAX);
        let (core, recent) = state
            .reader
            .session_brief_pages(ws, proj, BRIEF_CORE_PAGES_LIMIT, BRIEF_RECENT_PAGES_LIMIT)
            .await?;
        render_session_brief(&core, &recent, budget)
    } else {
        None
    };
    Ok(match (handoff_md, brief_md) {
        (Some(h), Some(b)) => Some(format!("{h}\n{b}")),
        (Some(h), None) => Some(h),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    })
}

/// Default char budget for the session-start brief (~1k tokens at the
/// usual ~4 chars/token) — enough for a few rules pages without taxing
/// every session start.
const BRIEF_BUDGET_DEFAULT: usize = 4_000;
/// Floor for the marker-supplied budget: below this the brief can't fit
/// even one meaningful page plus the headers.
const BRIEF_BUDGET_MIN: usize = 500;
/// Ceiling for the marker-supplied budget: the brief is injected into
/// EVERY opted-in session start, so an unbounded budget would let one
/// marker line quietly burn five figures of tokens per `/clear`.
const BRIEF_BUDGET_MAX: usize = 20_000;
/// How many core (pinned / `_rules/` / `_slots/`) pages the store fetches;
/// the char budget usually cuts earlier.
const BRIEF_CORE_PAGES_LIMIT: usize = 24;
/// How many recently-updated page titles the brief lists as follow-up
/// pointers.
const BRIEF_RECENT_PAGES_LIMIT: usize = 10;

/// Truncate to at most `max` bytes without splitting a UTF-8 char.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Render the marker-opted session-start project brief: core pages with
/// bodies (pinned first) under a char budget, then recently-updated titles
/// as follow-up pointers. Returns `None` for an empty project so the hook
/// injects nothing rather than an empty scaffold.
fn render_session_brief(
    core: &[engram_store::BriefPageBody],
    recent: &[engram_store::BriefingPage],
    budget_chars: usize,
) -> Option<String> {
    if core.is_empty() && recent.is_empty() {
        return None;
    }
    let mut buf = String::with_capacity(budget_chars.min(8_192));
    buf.push_str("> 🧭 **engram: project brief** (auto-injected — `.engram.toml [briefing]`)\n");

    let mut omitted: Vec<&str> = Vec::new();
    for page in core {
        let pin = if page.pinned { " 📌" } else { "" };
        let header = format!(
            "\n## {title}{pin} (`{path}`)\n",
            title = page.title,
            path = page.path,
        );
        // Reserve room for the footer sections so a single huge page
        // can't crowd out the recent-pages pointers entirely.
        let used = buf.len();
        if used + header.len() >= budget_chars {
            omitted.push(&page.path);
            continue;
        }
        let remaining = budget_chars - used - header.len();
        let body = page.body.trim();
        buf.push_str(&header);
        if body.len() > remaining {
            buf.push_str(truncate_at_char_boundary(body, remaining));
            buf.push_str("\n_[truncated by `[briefing] max_chars`]_\n");
        } else {
            buf.push_str(body);
            buf.push('\n');
        }
    }
    if !omitted.is_empty() {
        buf.push_str("\n**Core pages omitted by budget** (read via `memory_query` if needed)\n");
        for path in omitted {
            buf.push_str(&format!("- `{path}`\n"));
        }
    }
    if !recent.is_empty() {
        buf.push_str("\n**Recently updated pages** (titles only)\n");
        for page in recent {
            buf.push_str(&format!(
                "- {title} (`{path}`, {kind}, {ts})\n",
                title = page.title,
                path = page.path,
                kind = page.kind,
                ts = page.updated_at,
            ));
        }
    }
    buf.push_str(
        "\n_**To the receiving agent:** this brief is compiled from this \
         project's pinned / `_rules/` / `_slots/` wiki pages. Treat it as \
         current architecture and standing-rule context — do NOT re-explore \
         the codebase to rediscover what is already stated here. Call \
         `memory_query` for detail beyond this brief._\n",
    );
    Some(buf)
}

fn render_handoff_markdown(h: &Handoff) -> String {
    // Layout goal: TUI-renderable + agent-friendly. The previous
    // shape put a paragraph-long `## Summary` first, which made the
    // hook output look like a wall of text in Codex's "completed"
    // block AND let the agent miss that this *is* the answer to
    // "where did we leave off" questions. The new layout leads
    // with the actionable bullets (open questions, next steps) and
    // pushes the prose summary to the bottom; the agent-facing
    // footer explicitly tells the model how to interpret a follow-up
    // memory_handoff_accept = null.
    let mut buf = String::with_capacity(512);
    buf.push_str("> 📥 **engram: pending handoff from previous session**\n");
    buf.push_str(&format!(
        "> from `{from}` · created {ts}\n",
        from = h.from_agent.as_str(),
        ts = h.created_at,
    ));

    if !h.open_questions.is_empty() {
        buf.push_str("\n**Open questions**\n");
        for q in &h.open_questions {
            buf.push_str(&format!("- {q}\n"));
        }
    }
    if !h.next_steps.is_empty() {
        buf.push_str("\n**Next steps**\n");
        for s in &h.next_steps {
            buf.push_str(&format!("- {s}\n"));
        }
    }
    if !h.files_touched.is_empty() {
        buf.push_str("\n**Files touched**\n");
        for f in &h.files_touched {
            buf.push_str(&format!("- `{f}`\n"));
        }
    }

    // Summary last, as reference prose. Models reading top-down
    // see the action items first; the summary is detail.
    buf.push_str("\n**Summary**\n");
    buf.push_str(h.summary.trim());
    buf.push('\n');

    // Agent-facing reading instructions. This block is the
    // load-bearing UX fix — without it, agents call
    // memory_handoff_accept again, get `null` (single-use
    // already consumed by this hook), and conclude "no handoff"
    // *despite this content being right in their context*.
    buf.push_str(
        "\n---\n\
         _**To the receiving agent:** this content IS the pending \
         handoff — already consumed by the SessionStart hook. A \
         subsequent `memory_handoff_accept` call will return \
         `{ \"handoff\": null }` (single-use). When the user asks \
         \"where did we leave off?\" or \"any pending handoff?\", \
         answer from THIS content; do NOT re-call the tool. Call \
         `memory_query` / `memory_recent` only for additional \
         context beyond what's listed here._\n",
    );
    buf
}

/// Build the `project_cache` key from the resolved cwd, overrides, and
/// project strategy. Shared by `resolve_project_ids` (insert/lookup) and
/// `process` (eviction on the stale-cache retry) so the two always agree on
/// the slot.
fn cache_key_for(
    cwd_norm: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
) -> (String, String, String, String) {
    (
        cwd_norm.unwrap_or_default().to_string(),
        workspace_override.unwrap_or_default().to_string(),
        project_override.unwrap_or_default().to_string(),
        project_strategy.as_str().to_string(),
    )
}

fn normalize_project_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

/// Resolve the `(workspace_id, project_id)` pair for a hook event.
///
/// Precedence:
/// 1. `workspace_override` (typically declared by the agent's host-side
///    hook via a `.engram.toml` walk-up) OR `DEFAULT_WORKSPACE_NAME`.
/// 2. `project_override` OR marker-selected project strategy OR
///    `basename(cwd)` OR fallback to `state.project_id` (when `cwd` is
///    also unavailable).
///
/// Cache key is `(cwd, workspace_override, project_override,
/// project_strategy)` so the same `cwd` resolved with and without an
/// override (e.g. during a hook-script upgrade window) doesn't poison each
/// other's slot.
async fn resolve_project_ids(
    state: &HookState,
    cwd: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
    actor: &engram_core::ActorKey,
) -> anyhow::Result<(WorkspaceId, ProjectId)> {
    let cwd_norm = cwd
        .filter(|s| !s.is_empty())
        .map(normalize_project_path_key);

    // Without cwd AND without a project override, there's nothing to
    // resolve — fall through to the server defaults.
    if cwd_norm.is_none() && project_override.is_none() {
        return Ok((state.workspace_id, state.project_id));
    }

    let cache_key = cache_key_for(
        cwd_norm.as_deref(),
        workspace_override,
        project_override,
        project_strategy,
    );

    {
        let mut cache = state.project_cache.lock().await;
        if let Some(ids) = cache.get(&cache_key) {
            // Republish on every hit: a cache hit still means the agent
            // is active in this project *now*, which is exactly what the
            // MCP read tools need as their default. Keyed by the actor so
            // opt-in isolation modes (`per_session`/`per_actor`) keep
            // concurrent callers separated.
            state.active_project.set_for(actor, ids.0, ids.1);
            return Ok(ids);
        }
    }

    let workspace_name = workspace_override
        .unwrap_or(DEFAULT_WORKSPACE_NAME)
        .to_string();

    let (project_name, repo_path) = match (project_override, cwd_norm.as_deref()) {
        (Some(p), Some(c)) => (
            p.to_string(),
            repo_path_from_project_override(c, p, project_strategy),
        ),
        (Some(p), None) => (p.to_string(), None),
        (None, Some(c)) => match derive_project_from_cwd(c, project_strategy) {
            Some(resolved) => resolved,
            None => {
                state
                    .active_project
                    .set_for(actor, state.workspace_id, state.project_id);
                return Ok((state.workspace_id, state.project_id));
            }
        },
        (None, None) => {
            // The early-return at the top of the function guards
            // against this branch; the explicit fallback here keeps
            // the resolver panic-free if that guard ever moves or
            // gets refactored. Same effect as `unreachable!`, but
            // visible at compile time instead of inside the panic
            // message.
            state
                .active_project
                .set_for(actor, state.workspace_id, state.project_id);
            return Ok((state.workspace_id, state.project_id));
        }
    };

    // The reserved global preferences scope (issue #154) is written only
    // through explicit MCP `scope: "global"` requests — event capture must
    // never create it or leak observations into it, whether the name came
    // from a directory literally called `_global` or a marker-file
    // override. Fall back to the server-default project, same as a
    // cwd-less event.
    if project_name == engram_core::GLOBAL_SCOPE_PROJECT {
        debug!(
            cwd = ?cwd_norm,
            "hook router: refusing to attribute event capture to the reserved \
             global scope; using the server-default project"
        );
        state
            .active_project
            .set_for(actor, state.workspace_id, state.project_id);
        return Ok((state.workspace_id, state.project_id));
    }

    fn derive_project_from_cwd(
        cwd: &str,
        strategy: ProjectStrategy,
    ) -> Option<(String, Option<String>)> {
        // Delegate to the shared helper so the CLI's `resolve_project_name`
        // and this resolver agree on what "the project for this cwd"
        // resolves to. Map our wire-format `ProjectStrategy` onto the
        // shared library's `ProjectNameStrategy`.
        let path = std::path::Path::new(cwd);
        let strat = match strategy {
            ProjectStrategy::Basename => engram_consolidate::ProjectNameStrategy::Basename,
            ProjectStrategy::RepoRoot => engram_consolidate::ProjectNameStrategy::MainRepoRoot,
        };
        // `repo_path` is the project's git boundary and is used as a
        // longest-prefix match KEY for future cwds, so it must be a real
        // repo root or nothing -- never the bare cwd. Recording the bare
        // cwd turned any directory an agent merely opened a session in
        // (e.g. $HOME) into a catch-all that swallowed every project
        // nested beneath it (issue #103). The NAME still follows the
        // strategy.
        //
        // The `MainRepoRoot` strategy hands back the repo root in `root`
        // and names the project after it, so name and repo_path are
        // aligned -- keep it. Under `Basename` the project is named after
        // the cwd's leaf, so `root` is None and we may discover the
        // enclosing repo. Adopt that repo root as repo_path ONLY when the
        // cwd IS the repo root; for a subdir cwd the discovered root is a
        // PREFIX of the cwd whose basename differs from the project name,
        // so storing it would make a leaf project (e.g. `backend`) a
        // catch-all that swallows the repo root and every sibling subdir
        // (issue #103). A subdir cwd therefore stores None.
        engram_consolidate::derive_project_name(path, strat).map(|(name, root)| {
            let repo_path = root
                .map(|p| {
                    normalize_project_path_key(
                        &repo_root_in_cwd_namespace(path, &p).to_string_lossy(),
                    )
                })
                .or_else(|| repo_path_from_cwd(cwd));
            (name, repo_path)
        })
    }

    fn repo_path_from_cwd(cwd: &str) -> Option<String> {
        let path = std::path::Path::new(cwd);
        let repo_root = engram_consolidate::discover_repo_root(path).ok()?;
        cwd_is_repo_root(path, &repo_root).then(|| {
            normalize_project_path_key(
                &repo_root_in_cwd_namespace(path, &repo_root).to_string_lossy(),
            )
        })
    }

    fn repo_root_in_cwd_namespace(
        cwd: &std::path::Path,
        repo_root: &std::path::Path,
    ) -> std::path::PathBuf {
        // On macOS, temp paths often arrive from the host as `/var/...` while
        // libgit2 reports the same directory as `/private/var/...`. Prefix
        // matching later compares the stored `repo_path` against the raw hook
        // cwd, so keep the repo root in the same spelling/namespace as `cwd`
        // whenever canonical paths prove that `cwd` is inside `repo_root`.
        if let Ok(root_canon) = std::fs::canonicalize(repo_root) {
            for ancestor in cwd.ancestors() {
                if let Ok(ancestor_canon) = std::fs::canonicalize(ancestor)
                    && ancestor_canon == root_canon
                {
                    return ancestor.to_path_buf();
                }
            }
        }
        repo_root.to_path_buf()
    }

    fn repo_path_from_project_override(
        cwd: &str,
        project: &str,
        strategy: ProjectStrategy,
    ) -> Option<String> {
        if matches!(strategy, ProjectStrategy::RepoRoot) {
            let cwd_path = std::path::Path::new(cwd);
            if let Ok(root) = engram_consolidate::discover_main_repo_root(cwd_path) {
                let visible_root = repo_root_in_cwd_namespace(cwd_path, &root);
                if visible_root.file_name().and_then(|name| name.to_str()) == Some(project) {
                    return Some(normalize_project_path_key(&visible_root.to_string_lossy()));
                }
            }
        }
        repo_path_from_cwd(cwd)
    }

    fn cwd_is_repo_root(cwd: &std::path::Path, repo_root: &std::path::Path) -> bool {
        // git2's workdir may carry a trailing separator and resolves symlinks;
        // canonicalize both before comparing. Fall back to a trailing-slash
        // tolerant string compare if either path can't be canonicalized
        // (both should exist in practice).
        if let (Ok(a), Ok(b)) = (std::fs::canonicalize(cwd), std::fs::canonicalize(repo_root)) {
            return a == b;
        }
        let strip = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('/').to_string();
        strip(cwd) == strip(repo_root)
    }

    let ws = state
        .writer
        .get_or_create_workspace(workspace_name)
        .await
        .map_err(|e| anyhow::anyhow!("get_or_create_workspace: {e}"))?;

    // Prefix-match the cwd against any existing project's `repo_path`
    // BEFORE auto-creating a new project. Without this, a tool call
    // whose cwd was `/projects/manga-plus/reader/src/main.rs` would
    // get its observation attributed to a fresh `src`/`reader` project
    // instead of the existing `manga-plus` parent. The schema column
    // `projects.repo_path` was provisioned for exactly this match;
    // `find_project_by_cwd_prefix` returns the longest-matching parent
    // so a more-specific declared sub-project (via `.engram.toml`,
    // whose row has a longer `repo_path`) still wins over its outer
    // parent. Skipped when the operator passed an explicit
    // `project_override` (the override always wins) or when the cwd is
    // empty (cwd-less event already handled by the early returns above).
    // The match is keyed on the actual cwd (`cwd_norm`), not the stored
    // `repo_path`: `repo_path` is now the git root or None (issue #103),
    // whereas cwd->parent matching needs the full deep path.
    let proj = if project_override.is_none()
        && let Some(rp) = cwd_norm.as_deref().filter(|s| !s.is_empty())
        && let Some((parent_id, parent_name)) = state
            .reader
            .find_project_by_cwd_prefix(ws, rp.to_string(), state.home_dir.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("find_project_by_cwd_prefix: {e}"))?
        && parent_name != project_name
    {
        debug!(
            cwd = rp,
            derived = %project_name,
            parent = %parent_name,
            "hook router: cwd inside existing project — using parent instead of \
             creating fragment"
        );
        parent_id
    } else {
        state
            .writer
            .get_or_create_project(ws, project_name, repo_path)
            .await
            .map_err(|e| anyhow::anyhow!("get_or_create_project: {e}"))?
    };
    let ids = (ws, proj);
    state.project_cache.lock().await.insert(cache_key, ids);
    state.active_project.set_for(actor, ws, proj);
    Ok(ids)
}

/// Whether session-sticky attribution may apply: the event's cwd must sit
/// inside the session's own cwd subtree, and the session's cwd must be a
/// meaningful anchor — not missing, not the filesystem root, and not the
/// user's home directory (broad roots would fold every project beneath
/// them into one bucket, the #103 catch-all failure mode).
fn sticky_within_session_tree(
    session_cwd: Option<&str>,
    event_cwd: Option<&str>,
    home_dir: Option<&str>,
) -> bool {
    let Some(session_cwd) = session_cwd.filter(|s| !s.trim().is_empty()) else {
        return false;
    };
    let session_norm = normalize_project_path_key(session_cwd);
    if session_norm == "/" {
        return false;
    }
    if let Some(home) = home_dir
        && normalize_project_path_key(home) == session_norm
    {
        return false;
    }
    // A cwd-less event inside a known session still belongs to it — there
    // is no directory evidence to contradict the session (and per-event
    // resolution would only shrug it into the server default anyway).
    let Some(event_cwd) = event_cwd.filter(|s| !s.trim().is_empty()) else {
        return true;
    };
    let event_norm = normalize_project_path_key(event_cwd);
    // Component-wise containment: "/a/b" contains "/a/b/c" but not
    // "/a/bc" (Path::starts_with is component-based, not string-based).
    std::path::Path::new(&event_norm).starts_with(std::path::Path::new(&session_norm))
}

async fn process_envelope(state: Arc<HookState>, env: HookEnvelope, actor_user: Option<String>) {
    if let Err(e) = process(&state, env, actor_user).await {
        warn!(error = %e, "hook processing failed");
    }
}

async fn process(
    state: &HookState,
    env: HookEnvelope,
    actor_user: Option<String>,
) -> anyhow::Result<()> {
    let session_id = resolve_session_id(&env)?;
    // Build the actor key used to scope the in-process `ActiveProject`
    // pointer. `user` is whatever the auth middleware extracted from this
    // request; `session_id` is the RAW string from the payload (NOT the
    // resolved UUID) — agents that forward an opaque session id over
    // `X-Memory-Actor-Session-Id` on /mcp pass the same raw string, so set
    // and get land on the same map slot. The MCP server's
    // `actor_key_from_parts` mirrors this convention. Empty actor
    // (anonymous + no session) is allowed — `set_for` falls back to the
    // single slot.
    let actor_key = engram_core::ActorKey {
        user: actor_user.clone(),
        session_id: env.session_id.clone(),
    };
    // Session-sticky attribution: for an event whose session already
    // exists, the session's own scope is the source of truth (the same
    // rationale as the V19 repair). Per-event cwd derivation only decides
    // for session-CREATING events. Without this, a mid-session `cd subdir/`
    // inside a NON-GIT project scattered observations into basename
    // fragments ("sources", "desktop", …): the v0.12.2 prefix match keys on
    // `repo_path`, and #103 deliberately never records one for non-git
    // parents, so the match had nothing to anchor to.
    //
    // Stickiness is bounded so it can never become a catch-all:
    // - Explicit marker-file overrides still win — a `.engram.toml`
    //   naming a project is a deliberate rescope, not drift.
    // - The event's cwd must sit INSIDE the session's own cwd subtree;
    //   `cd`-ing out of the session's tree (into a different project)
    //   falls back to per-event resolution as before.
    // - A session rooted at a broad directory — the filesystem root or
    //   the user's home — never sticks, or a stray session started in
    //   `$HOME` would fold every project beneath it into one bucket
    //   (the same catch-all failure #103 healed for repo_path keys).
    let sticky_scope = if env.project_override.is_none() && env.workspace_override.is_none() {
        state
            .reader
            .find_session_scope(session_id)
            .await?
            .filter(|(_, _, session_cwd)| {
                sticky_within_session_tree(
                    session_cwd.as_deref(),
                    env.cwd.as_deref(),
                    state.home_dir.as_deref(),
                )
            })
    } else {
        None
    };
    let (mut ws, mut proj) = match sticky_scope {
        Some((session_ws, session_proj, _)) => {
            // Publish the pointer like resolve_project_ids does on a cache
            // hit: this session being active is exactly what the MCP read
            // tools should default to.
            state
                .active_project
                .set_for(&actor_key, session_ws, session_proj);
            (session_ws, session_proj)
        }
        None => {
            resolve_project_ids(
                state,
                env.cwd.as_deref(),
                env.workspace_override.as_deref(),
                env.project_override.as_deref(),
                env.project_strategy,
                &actor_key,
            )
            .await?
        }
    };

    if matches!(env.event, HookEvent::SessionEnd) {
        match state
            .reader
            .session_end_disposition(session_id, ws, proj, env.agent)
            .await?
        {
            engram_store::SessionEndDisposition::Open => {}
            engram_store::SessionEndDisposition::DropStale => {
                info!(
                    session = %session_id,
                    agent = %env.agent.as_str(),
                    "ignoring SessionEnd for missing, mismatched, or already-ended session"
                );
                return Ok(());
            }
            // The agent resumed an ended session under the same id and kept
            // working (issue #152). Run the full end path again — page
            // rewrite, ended_at bump, handoff, opt-in LLM consolidation — so
            // the resumed work reaches the compiled session page instead of
            // living only in raw observations.
            engram_store::SessionEndDisposition::ReEndWithNewWork => {
                info!(
                    session = %session_id,
                    agent = %env.agent.as_str(),
                    "SessionEnd re-ends a resumed session with new work; re-running end path"
                );
            }
        }
    }

    // Hooks are fire-and-forget and may arrive out of order. Begin the
    // session idempotently before every observation so a resumed agent
    // session, or a prompt racing ahead of SessionStart, cannot trip the
    // observations.session_id foreign key.
    let new_session = NewSession {
        id: session_id,
        workspace_id: ws,
        project_id: proj,
        agent_kind: env.agent,
        cwd: env.cwd.as_ref().map(std::path::PathBuf::from),
    };
    if let Err(e) = state.writer.begin_session(new_session).await {
        // The cached (workspace, project) may have been deleted out from
        // under us — e.g. `purge-project` on a live server drops the row
        // but leaves this in-memory cache pointing at the old id, so
        // begin_session trips the project foreign key. Evict the stale
        // slot, re-resolve (which recreates the project), and retry once.
        warn!(error = %e, "begin_session failed; evicting stale project cache and retrying");
        let cwd_norm = env
            .cwd
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(normalize_project_path_key);
        let key = cache_key_for(
            cwd_norm.as_deref(),
            env.workspace_override.as_deref(),
            env.project_override.as_deref(),
            env.project_strategy,
        );
        state.project_cache.lock().await.remove(&key);
        let refreshed = resolve_project_ids(
            state,
            env.cwd.as_deref(),
            env.workspace_override.as_deref(),
            env.project_override.as_deref(),
            env.project_strategy,
            &actor_key,
        )
        .await?;
        ws = refreshed.0;
        proj = refreshed.1;
        state
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: env.agent,
                cwd: env.cwd.as_ref().map(std::path::PathBuf::from),
            })
            .await?;
    }

    // Persist the observation row.
    let kind = env.event.to_observation_kind();
    let title = env
        .title_hint
        .clone()
        .unwrap_or_else(|| kind.as_str().to_string());
    let body = env.body_excerpt.clone().unwrap_or_default();
    let raw_obs = NewObservation {
        session_id,
        workspace_id: ws,
        project_id: proj,
        kind,
        extension: env.extension.clone(),
        source_event: env.source_event.clone(),
        title,
        body,
        importance: importance_for(env.event),
    };
    let sanitized = Sanitized::new(raw_obs, &state.sanitizer);
    let _ = state
        .writer
        .insert_observation(sanitized.inner().clone())
        .await?;

    // Append the log line to the per-project log.md.
    if let Err(e) = log::append_event(
        &state.wiki,
        ws,
        proj,
        Timestamp::now(),
        env.event,
        sanitized.inner().title.as_str(),
    ) {
        warn!(error = %e, "log.md append failed");
    }

    // On PreCompact, refresh `sessions/<id>.md` so the wiki captures
    // the working state before the agent's compaction throws it out
    // of context. Does NOT end the session and does NOT create a
    // handoff. The eventual SessionEnd supersedes this page.
    if matches!(env.event, HookEvent::PreCompact)
        && let Err(e) = consolidate_or_synth(state, session_id, ws, proj).await
    {
        warn!(error = %e, "PreCompact consolidation failed; continuing");
    }

    // On SessionEnd, synthesize the summary page, end the session, and
    // auto-create a handoff so the next agent can pick up.
    if matches!(env.event, HookEvent::SessionEnd) {
        let observations = state.reader.observations_for_session(session_id).await?;
        let new_page = synthesize_session_page(ws, proj, session_id, &observations);
        let page_id = state
            .wiki
            .write_page(engram_wiki::WritePageRequest {
                workspace_id: new_page.workspace_id,
                project_id: new_page.project_id,
                path: new_page.path.clone(),
                frontmatter: new_page.frontmatter_json.clone(),
                body: new_page.body.clone(),
                tier: new_page.tier,
                pinned: new_page.pinned,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: engram_core::ActorContext::anonymous(),
            })
            .await?;
        state.writer.end_session(session_id, Some(page_id)).await?;
        let handoff = build_auto_handoff(
            ws,
            proj,
            env.agent,
            session_id,
            env.cwd.clone(),
            &observations,
        );
        let handoff_id = state.writer.insert_handoff(handoff).await?;
        // Opt-in (ENGRAM_CONSOLIDATE_ON_SESSION_END): additionally run LLM
        // consolidation so the session's knowledge is compiled into topical
        // pages, not just the heuristic session record. The heuristic page
        // above is always written first, so an LLM failure here is non-fatal —
        // warn and keep the deterministic result. Runs before the commit so
        // the consolidated pages land in the same atomic snapshot.
        if state.consolidate_on_session_end
            && let Some(c) = state.consolidator.as_ref()
        {
            match c.consolidate_session(session_id, false).await {
                Ok(outcome) => info!(
                    session = %session_id,
                    path = %outcome.path,
                    "SessionEnd: LLM consolidation written (opt-in)",
                ),
                Err(e) => warn!(
                    error = %e,
                    "SessionEnd LLM consolidation failed; heuristic page already written",
                ),
            }
        }
        // Auto-commit the wiki tree so the session/handoff/log.md
        // changes land in git in one atomic snapshot.
        let commit_msg = format!(
            "session {}: {}",
            short_id(&session_id.to_string()),
            new_page.title.chars().take(60).collect::<String>(),
        );
        match state.wiki.commit_all(&commit_msg) {
            Ok(Some(oid)) => debug!(commit = %oid, "wiki auto-commit"),
            Ok(None) => debug!("wiki clean; no auto-commit"),
            Err(e) => warn!(error = %e, "auto-commit failed"),
        }
        info!(
            session = %session_id,
            page = %new_page.path,
            handoff = %handoff_id,
            "session ended; summary page + open handoff created",
        );
    }

    Ok(())
}

fn resolve_session_id(env: &HookEnvelope) -> anyhow::Result<SessionId> {
    if let Some(raw) = &env.session_id {
        // Accept either a UUID (canonical) or any string, hashing the
        // latter to a deterministic UUID v5 so each agent's session id
        // maps cleanly into our schema.
        if let Ok(id) = SessionId::from_str(raw) {
            return Ok(id);
        }
        let uuid = Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes());
        return Ok(SessionId(uuid));
    }
    if matches!(env.event, HookEvent::SessionStart) {
        return Ok(SessionId::new());
    }
    anyhow::bail!("hook payload missing session_id and event is not session-start")
}

fn build_auto_handoff(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    from_agent: AgentKind,
    session_id: SessionId,
    cwd: Option<String>,
    observations: &[engram_core::Observation],
) -> NewHandoff {
    // Prefer obs.body (the full prompt) over obs.title (first-line +
    // truncated to 80 chars for log/list display). When body is
    // empty fall back to title so we never produce an empty entry.
    fn pick_text(obs: &engram_core::Observation) -> &str {
        if !obs.body.is_empty() {
            obs.body.as_str()
        } else {
            obs.title.as_str()
        }
    }
    /// Cap so a single 10-page prompt doesn't blow up the handoff.
    /// The body is already scrubbed at insert time; this is just a
    /// length budget. 1500 chars ≈ 250 words ≈ a paragraph.
    fn cap(s: &str) -> String {
        const MAX: usize = 1500;
        if s.chars().count() <= MAX {
            s.to_string()
        } else {
            let truncated: String = s.chars().take(MAX).collect();
            format!("{truncated}…")
        }
    }
    let mut prompts: Vec<String> = Vec::new();
    let mut tools: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for obs in observations {
        match obs.kind {
            ObservationKind::UserPrompt => {
                let text = pick_text(obs);
                if !text.is_empty() {
                    prompts.push(text.to_string());
                }
            }
            ObservationKind::PostToolUse | ObservationKind::PreToolUse if !obs.title.is_empty() => {
                tools.insert(obs.title.as_str());
            }
            _ => {}
        }
    }
    let first_prompt = prompts.first().cloned();
    let last_prompt = prompts.last().cloned();
    let summary = match (&first_prompt, &last_prompt) {
        (Some(first), Some(last)) if first == last => format!("Session focused on: {}", cap(first)),
        (Some(first), Some(last)) => format!("Started: {}\n\nLast: {}", cap(first), cap(last),),
        (Some(first), None) => format!("Started: {}", cap(first)),
        _ => format!(
            "Session ended; {} observations recorded.",
            observations.len()
        ),
    };
    let open_questions = if let Some(last) = last_prompt {
        // Heuristic: last user prompt often *is* the open question.
        vec![format!("Continue from: {}", cap(&last))]
    } else {
        Vec::new()
    };
    let next_steps = if tools.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Tools used: {}",
            tools.into_iter().collect::<Vec<_>>().join(", ")
        )]
    };
    NewHandoff {
        workspace_id,
        project_id,
        from_session_id: Some(session_id),
        from_agent,
        to_agent: None,
        cwd: cwd.map(std::path::PathBuf::from),
        summary,
        open_questions,
        next_steps,
        files_touched: Vec::new(),
    }
}

/// Write a fresh `sessions/<id>.md` for the current session without
/// ending it. Used by the PreCompact branch to checkpoint state before
/// the agent's working context collapses.
async fn consolidate_or_synth(
    state: &HookState,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> anyhow::Result<()> {
    if let Some(c) = state.consolidator.as_ref() {
        let outcome = c.consolidate_session(session_id, false).await?;
        debug!(
            session = %session_id,
            path = %outcome.path,
            "PreCompact: LLM consolidation written",
        );
        let _ = state.wiki.commit_all(&format!(
            "pre-compact(session {}): checkpoint",
            short_id(&session_id.to_string()),
        ));
        return Ok(());
    }
    let observations = state.reader.observations_for_session(session_id).await?;
    if observations.is_empty() {
        return Ok(());
    }
    let new_page = synthesize_session_page(workspace_id, project_id, session_id, &observations);
    state
        .wiki
        .write_page(engram_wiki::WritePageRequest {
            workspace_id: new_page.workspace_id,
            project_id: new_page.project_id,
            path: new_page.path,
            frontmatter: new_page.frontmatter_json,
            body: new_page.body,
            tier: new_page.tier,
            pinned: new_page.pinned,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: engram_core::ActorContext::anonymous(),
        })
        .await?;
    let _ = state.wiki.commit_all(&format!(
        "pre-compact(session {}): checkpoint",
        short_id(&session_id.to_string()),
    ));
    debug!(session = %session_id, "PreCompact: rule-based checkpoint written");
    Ok(())
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

const fn importance_for(event: HookEvent) -> u8 {
    match event {
        HookEvent::SessionStart | HookEvent::SessionEnd => 7,
        HookEvent::UserPrompt => 8,
        HookEvent::PostToolUse | HookEvent::PreToolUse => 5,
        HookEvent::Stop | HookEvent::PreCompact => 6,
        HookEvent::Notification
        | HookEvent::Other
        | HookEvent::SubagentStart
        | HookEvent::SubagentStop => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use engram_core::Sanitizer;
    use engram_store::Store;
    use engram_wiki::Wiki;
    use tempfile::TempDir;

    use super::*;
    use crate::payload::HookQuery;

    /// Build a minimal `HookState` backed by a real on-disk store.
    async fn make_state(tmp: &TempDir) -> HookState {
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let sanitizer = Sanitizer::default();
        HookState {
            workspace_id: ws,
            project_id: proj,
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            consolidator: None,
            sanitizer,
            project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
            active_project: ActiveProject::new(),
            consolidate_on_session_end: false,
            subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
            home_dir: None,
            ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
            )),
        }
    }

    #[cfg(not(windows))]
    fn init_repo_with_commit(path: &std::path::Path) -> git2::Repository {
        std::fs::create_dir_all(path).unwrap();
        let repo = git2::Repository::init(path).unwrap();
        let sig = repo
            .signature()
            .unwrap_or_else(|_| git2::Signature::now("test", "test@test.com").unwrap());
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[cfg(windows)]
    fn init_repo_with_commit(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        let mut init = std::process::Command::new("git");
        init.args(["init", "-q", "-b", "main"]).arg(path);
        assert_command_success(init);

        let mut email = std::process::Command::new("git");
        email
            .arg("-C")
            .arg(path)
            .args(["config", "user.email", "test@example.com"]);
        assert_command_success(email);

        let mut name = std::process::Command::new("git");
        name.arg("-C")
            .arg(path)
            .args(["config", "user.name", "Test"]);
        assert_command_success(name);

        let mut commit = std::process::Command::new("git");
        commit
            .arg("-C")
            .arg(path)
            .args(["commit", "--allow-empty", "-m", "initial"]);
        assert_command_success(commit);
    }

    #[cfg(windows)]
    fn init_bare_repo(path: &std::path::Path) {
        let mut init = std::process::Command::new("git");
        init.args(["init", "--bare", "-q"]).arg(path);
        assert_command_success(init);
    }

    // Windows 11 + Git Bash support matters for regulated enterprise setups
    // where Git Bash is the approved shell available from the corporate
    // repository. Symlink creation can still be denied by Windows policy, so
    // the Windows path skips only when the OS reports the missing privilege.
    #[cfg(unix)]
    fn create_test_symlink_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn create_test_symlink_dir(target: &std::path::Path, link: &std::path::Path) -> bool {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(1314) => {
                eprintln!(
                    "skipping symlink repo-path assertion: Windows denied symlink creation privilege"
                );
                false
            }
            Err(e) => panic!("failed to create test symlink {}: {e}", link.display()),
        }
    }

    #[cfg(windows)]
    fn assert_command_success(mut command: std::process::Command) {
        let status = command.status().unwrap();
        assert!(status.success(), "command failed: {command:?}");
    }

    /// Two hook events with distinct cwds must land in two distinct projects.
    #[tokio::test]
    async fn process_with_cwd_creates_new_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Event from /home/user/project-alpha.
        let (ws_a, proj_a) = resolve_project_ids(
            &state,
            Some("/home/user/project-alpha"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        // Event from /home/user/project-beta.
        let (ws_b, proj_b) = resolve_project_ids(
            &state,
            Some("/home/user/project-beta"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // Projects must be distinct; workspace is the same (`default`).
        assert_ne!(proj_a, proj_b, "different cwds → different projects");
        assert_eq!(ws_a, ws_b, "same default workspace");

        // Neither should match the server-default scratch project.
        assert_ne!(proj_a, state.project_id);
        assert_ne!(proj_b, state.project_id);

        // The MCP-shared pointer reflects the most recently resolved
        // project (issue #2) — here, project-beta.
        assert_eq!(state.active_project.get(), Some((ws_b, proj_b)));
    }

    // Catch-all guards on stickiness: a session accidentally started at a
    // broad root (`/`, `$HOME`) must NOT fold everything beneath it into
    // one project, and `cd`-ing OUT of the session's tree must fall back
    // to per-event resolution.
    #[test]
    fn sticky_never_applies_to_broad_roots_or_out_of_tree_cwds() {
        // Session rooted at the filesystem root: never sticky.
        assert!(!sticky_within_session_tree(
            Some("/"),
            Some("/mnt/data/Projects/real-project"),
            Some("/home/user"),
        ));
        // Session rooted at $HOME: never sticky.
        assert!(!sticky_within_session_tree(
            Some("/home/user"),
            Some("/home/user/Projects/real-project"),
            Some("/home/user"),
        ));
        // Missing session cwd: no anchor, no stickiness.
        assert!(!sticky_within_session_tree(
            None,
            Some("/a/b/c"),
            Some("/home/user"),
        ));
        // Event cwd OUTSIDE the session tree: falls back to per-event
        // resolution (a real `cd` into a different project).
        assert!(!sticky_within_session_tree(
            Some("/a/b"),
            Some("/a/other"),
            Some("/home/user"),
        ));
        // Component-wise containment, not string-prefix: /a/bc is NOT
        // inside /a/b.
        assert!(!sticky_within_session_tree(
            Some("/a/b"),
            Some("/a/bc"),
            Some("/home/user"),
        ));

        // The intended case: subdirectory of a normal session cwd sticks.
        assert!(sticky_within_session_tree(
            Some("/a/b"),
            Some("/a/b/c"),
            Some("/home/user"),
        ));
        // Same dir sticks; cwd-less events inside a known session stick.
        assert!(sticky_within_session_tree(
            Some("/a/b"),
            Some("/a/b"),
            Some("/home/user"),
        ));
        assert!(sticky_within_session_tree(
            Some("/a/b"),
            None,
            Some("/home/user"),
        ));
    }

    // Session-sticky attribution: a mid-session `cd subdir/` inside a
    // NON-GIT project must keep observations in the session's project.
    // This is the exact production failure behind the fragment cleanup:
    // non-git parents have no repo_path, so the v0.12.2 prefix match
    // can't anchor subdir cwds, and per-event basename derivation minted
    // "sources"/"desktop"/… fragment projects daily.
    #[tokio::test]
    async fn mid_session_subdir_cwd_sticks_to_the_sessions_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let sid = "44444444-4444-4444-4444-444444444444";
        // Plain directory, deliberately NOT a git repo.
        let parent = tmp.path().join("tiktok_analysis");
        let subdir = parent.join("sources");
        std::fs::create_dir_all(&subdir).unwrap();
        let fire = |event: &str, cwd: std::path::PathBuf| {
            HookEnvelope::from_query_and_body(
                HookQuery {
                    event: event.into(),
                    agent: Some("claude-code".into()),
                    cwd: Some(cwd.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                serde_json::json!({
                    "session_id": sid,
                    "cwd": cwd.to_string_lossy(),
                    "tool_name": "Bash",
                }),
            )
        };

        // Session starts at the parent; a later tool event reports the
        // subdirectory cwd (agent shells keep their working directory).
        process(&state, fire("session-start", parent.clone()), None)
            .await
            .unwrap();
        process(&state, fire("post-tool-use", subdir.clone()), None)
            .await
            .unwrap();

        let parent_proj = state
            .reader
            .find_project(state.workspace_id, "tiktok_analysis".to_string())
            .await
            .unwrap()
            .expect("parent project exists");
        assert_eq!(
            state
                .reader
                .find_project(state.workspace_id, "sources".to_string())
                .await
                .unwrap(),
            None,
            "the subdir event must not mint a fragment project"
        );
        let session_id: SessionId = sid.parse().unwrap();
        let observations = state
            .reader
            .observations_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(observations.len(), 2);
        assert!(
            observations.iter().all(|o| o.project_id == parent_proj),
            "every observation must carry the session's project"
        );

        // An explicit marker override mid-session is a deliberate rescope
        // and must still win over stickiness.
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("claude-code".into()),
                cwd: Some(subdir.to_string_lossy().into_owned()),
                project: Some("declared-elsewhere".into()),
                workspace: Some("default".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": sid,
                "cwd": subdir.to_string_lossy(),
                "tool_name": "Bash",
            }),
        );
        process(&state, env, None).await.unwrap();
        assert!(
            state
                .reader
                .find_project(state.workspace_id, "declared-elsewhere".to_string())
                .await
                .unwrap()
                .is_some(),
            "marker-file overrides must still rescope"
        );
    }

    // Issue #154: event capture must never create or attribute to the
    // reserved `_global` preferences scope — not from a directory that
    // happens to carry the reserved name, and not from a marker-file
    // project override.
    #[tokio::test]
    async fn reserved_global_scope_is_never_auto_attributed() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // cwd whose basename is the reserved name.
        let (ws, proj) = resolve_project_ids(
            &state,
            Some("/home/user/_global"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            (ws, proj),
            (state.workspace_id, state.project_id),
            "cwd named _global must fall back to the server-default project"
        );

        // Explicit marker-file override naming the reserved scope.
        let (ws, proj) = resolve_project_ids(
            &state,
            Some("/home/user/some-project"),
            None,
            Some(engram_core::GLOBAL_SCOPE_PROJECT),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            (ws, proj),
            (state.workspace_id, state.project_id),
            "project override _global must fall back to the server-default project"
        );

        // Neither call may have materialised the reserved project row.
        let created = state
            .reader
            .find_project(
                state.workspace_id,
                engram_core::GLOBAL_SCOPE_PROJECT.to_string(),
            )
            .await
            .unwrap();
        assert_eq!(created, None, "event capture must not create _global");
    }

    #[tokio::test]
    async fn handle_hook_returns_429_when_ingest_saturated() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(0));

        let response = handle_hook(
            State(Arc::new(state)),
            Query(HookQuery {
                event: "session-start".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            }),
            None,
            Json(serde_json::json!({})),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn handle_hook_batch_acks_processed_count() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        // Two events sharing a session, carried in ONE batch request — the per
        // event `?event=…&agent=…` query is parsed from each item's `url`.
        let items = vec![
            HookBatchItem {
                url: "http://h/hook?event=session-start&agent=claude-code".into(),
                body: serde_json::json!({ "session_id": "batch-s1" }),
            },
            HookBatchItem {
                url: "http://h/hook?event=user-prompt-submit&agent=claude-code".into(),
                body: serde_json::json!({ "session_id": "batch-s1", "prompt": "hello" }),
            },
        ];

        let response = handle_hook_batch(State(Arc::new(state)), None, Json(items))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack["accepted"], 2, "both events committed, oldest-first");
    }

    /// `pre-tool-use` query+agent for building an env to recompute a SessionId.
    fn grok_tool_query() -> HookQuery {
        HookQuery {
            event: "pre-tool-use".into(),
            agent: Some("grok".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn handle_hook_batch_drops_subagent_events_when_enabled() {
        let tmp = TempDir::new().unwrap();
        let state = Arc::new(make_state(&tmp).await);

        // A grok subagent tool-use event (carries `subagentType`) alongside a
        // top-level event (no marker), in ONE batch.
        let sub_body = serde_json::json!({
            "sessionId": "sub-s1", "subagentType": "general-purpose", "toolName": "x"
        });
        let top_body = serde_json::json!({ "sessionId": "top-s1", "toolName": "x" });
        // The project opted in (`.engram.toml` → `drop_subagent=1`), so every
        // event carries the flag; only the actual subagent capture is dropped.
        let items = vec![
            HookBatchItem {
                url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                body: sub_body.clone(),
            },
            HookBatchItem {
                url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                body: top_body.clone(),
            },
        ];

        let response = handle_hook_batch(State(state.clone()), None, Json(items))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Accept-but-drop: BOTH are acked so the client clears its spool…
        assert_eq!(
            ack["accepted"], 2,
            "both acked so the client clears its spool"
        );

        // …but only the top-level event was persisted; the subagent left nothing.
        let sub_sid = resolve_session_id(&HookEnvelope::from_query_and_body(
            grok_tool_query(),
            sub_body,
        ))
        .unwrap();
        let top_sid = resolve_session_id(&HookEnvelope::from_query_and_body(
            grok_tool_query(),
            top_body,
        ))
        .unwrap();
        assert!(
            state
                .reader
                .observations_for_session(sub_sid)
                .await
                .unwrap()
                .is_empty(),
            "subagent capture must not be persisted"
        );
        assert_eq!(
            state
                .reader
                .observations_for_session(top_sid)
                .await
                .unwrap()
                .len(),
            1,
            "top-level capture is persisted as usual"
        );
    }

    #[tokio::test]
    async fn handle_hook_batch_keeps_subagent_events_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let state = Arc::new(make_state(&tmp).await);

        // No `drop_subagent` flag on the request → the project did not opt in,
        // so its subagent captures are stored as usual.
        let sub_body = serde_json::json!({
            "sessionId": "sub-s2", "subagentType": "general-purpose", "toolName": "x"
        });
        let items = vec![HookBatchItem {
            url: "http://h/hook?event=pre-tool-use&agent=grok".into(),
            body: sub_body.clone(),
        }];

        let response = handle_hook_batch(State(state.clone()), None, Json(items))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let sub_sid = resolve_session_id(&HookEnvelope::from_query_and_body(
            grok_tool_query(),
            sub_body,
        ))
        .unwrap();
        assert_eq!(
            state
                .reader
                .observations_for_session(sub_sid)
                .await
                .unwrap()
                .len(),
            1,
            "without the per-project opt-in, subagent captures are stored (default behavior)"
        );
    }

    #[tokio::test]
    async fn drop_subagent_captures_drops_unmarked_tail_of_tracked_session() {
        let tmp = TempDir::new().unwrap();
        let state = Arc::new(make_state(&tmp).await);

        // (1) marked subagent event seeds session "sub" (and is dropped);
        // (2) a later UNMARKED event on "sub" is the tail → dropped via tracking;
        // (3) an UNMARKED event on a never-seeded session "top" → kept.
        let marked = serde_json::json!({
            "sessionId": "sub", "subagentType": "general-purpose", "toolName": "x"
        });
        let tail = serde_json::json!({ "sessionId": "sub", "toolName": "y" });
        let top = serde_json::json!({ "sessionId": "top", "toolName": "z" });
        let items = vec![
            HookBatchItem {
                url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                body: marked,
            },
            HookBatchItem {
                url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                body: tail.clone(),
            },
            HookBatchItem {
                url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                body: top.clone(),
            },
        ];

        let response = handle_hook_batch(State(state.clone()), None, Json(items))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack["accepted"], 3, "all acked: 2 dropped + 1 processed");

        let sub_sid =
            resolve_session_id(&HookEnvelope::from_query_and_body(grok_tool_query(), tail))
                .unwrap();
        let top_sid =
            resolve_session_id(&HookEnvelope::from_query_and_body(grok_tool_query(), top)).unwrap();
        assert!(
            state
                .reader
                .observations_for_session(sub_sid)
                .await
                .unwrap()
                .is_empty(),
            "the unmarked tail of a tracked subagent session is dropped"
        );
        assert_eq!(
            state
                .reader
                .observations_for_session(top_sid)
                .await
                .unwrap()
                .len(),
            1,
            "an unmarked event on a non-subagent session is kept"
        );
    }

    #[tokio::test]
    async fn subagent_start_event_seeds_the_session_so_its_tail_drops() {
        let tmp = TempDir::new().unwrap();
        let state = Arc::new(make_state(&tmp).await);

        // SubagentStart seeds session "ss" BEFORE its first content event, so even
        // the leading unmarked user_prompt_submit is dropped.
        let start = serde_json::json!({ "sessionId": "ss" });
        let lead = serde_json::json!({ "sessionId": "ss", "prompt": "go" });
        let items = vec![
            HookBatchItem {
                url: "http://h/hook?event=subagent-start&agent=grok&drop_subagent=1".into(),
                body: start,
            },
            HookBatchItem {
                url: "http://h/hook?event=user-prompt-submit&agent=grok&drop_subagent=1".into(),
                body: lead.clone(),
            },
        ];

        let response = handle_hook_batch(State(state.clone()), None, Json(items))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let sid = resolve_session_id(&HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt-submit".into(),
                agent: Some("grok".into()),
                ..Default::default()
            },
            lead,
        ))
        .unwrap();
        assert!(
            state
                .reader
                .observations_for_session(sid)
                .await
                .unwrap()
                .is_empty(),
            "SubagentStart seeds the session so the leading unmarked event drops too"
        );
    }

    #[tokio::test]
    async fn drop_subagent_tracking_is_scoped_by_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let actor = engram_core::ActorKey::default();

        let marked_project_a = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("grok".into()),
                project: Some("project-a".into()),
                drop_subagent: Some("1".into()),
                ..Default::default()
            },
            serde_json::json!({
                "sessionId": "shared-session", "subagentType": "general-purpose"
            }),
        );
        assert!(should_drop_subagent(&state, &marked_project_a, &actor).await);

        let unmarked_project_b = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("grok".into()),
                project: Some("project-b".into()),
                drop_subagent: Some("1".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionId": "shared-session", "toolName": "kept" }),
        );
        assert!(
            !should_drop_subagent(&state, &unmarked_project_b, &actor).await,
            "a subagent session tracked in project-a must not drop same-id events in project-b"
        );

        let unmarked_project_a = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "pre-tool-use".into(),
                agent: Some("grok".into()),
                project: Some("project-a".into()),
                drop_subagent: Some("1".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionId": "shared-session", "toolName": "dropped" }),
        );
        assert!(
            should_drop_subagent(&state, &unmarked_project_a, &actor).await,
            "the originally tracked project's unmarked tail still drops"
        );
    }

    #[tokio::test]
    async fn subagent_stop_keeps_session_tracked_until_session_end_tail_drops() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let actor = engram_core::ActorKey::default();

        let query = |event: &str| HookQuery {
            event: event.into(),
            agent: Some("grok".into()),
            project: Some("tail-project".into()),
            drop_subagent: Some("1".into()),
            ..Default::default()
        };

        let start = HookEnvelope::from_query_and_body(
            query("subagent-start"),
            serde_json::json!({ "sessionId": "tail-session" }),
        );
        assert!(should_drop_subagent(&state, &start, &actor).await);

        let subagent_stop = HookEnvelope::from_query_and_body(
            query("subagent-stop"),
            serde_json::json!({ "sessionId": "tail-session" }),
        );
        assert!(should_drop_subagent(&state, &subagent_stop, &actor).await);

        let unmarked_stop_tail = HookEnvelope::from_query_and_body(
            query("stop"),
            serde_json::json!({ "sessionId": "tail-session" }),
        );
        assert!(
            should_drop_subagent(&state, &unmarked_stop_tail, &actor).await,
            "SubagentStop must not clear tracking before the unmarked stop tail"
        );

        let session_end_tail = HookEnvelope::from_query_and_body(
            query("session-end"),
            serde_json::json!({ "sessionId": "tail-session" }),
        );
        assert!(
            should_drop_subagent(&state, &session_end_tail, &actor).await,
            "SessionEnd tail is dropped and then clears tracking"
        );

        let after_session_end = HookEnvelope::from_query_and_body(
            query("pre-tool-use"),
            serde_json::json!({ "sessionId": "tail-session", "toolName": "kept" }),
        );
        assert!(
            !should_drop_subagent(&state, &after_session_end, &actor).await,
            "SessionEnd clears tracking for that scoped session"
        );
    }

    #[tokio::test]
    async fn handle_hook_batch_returns_429_when_saturated() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(0));

        let response = handle_hook_batch(
            State(Arc::new(state)),
            None,
            Json(vec![HookBatchItem {
                url: "http://h/hook?event=session-start&agent=claude-code".into(),
                body: serde_json::json!({}),
            }]),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn handle_hook_batch_drops_subagent_events_before_capacity_check() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(0));

        let response = handle_hook_batch(
            State(Arc::new(state)),
            None,
            Json(vec![HookBatchItem {
                url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                body: serde_json::json!({
                    "sessionId": "saturated-subagent", "subagentType": "general-purpose"
                }),
            }]),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            ack["accepted"], 1,
            "droppable subagent batch items should clear the spool even when ingest capacity is saturated"
        );
    }

    #[tokio::test]
    async fn handle_hook_batch_saturated_after_prefix_reports_accepted_prefix() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(0));

        let response = handle_hook_batch(
            State(Arc::new(state)),
            None,
            Json(vec![
                HookBatchItem {
                    url: "http://h/hook?event=pre-tool-use&agent=grok&drop_subagent=1".into(),
                    body: serde_json::json!({
                        "sessionId": "saturated-prefix", "subagentType": "general-purpose"
                    }),
                },
                HookBatchItem {
                    url: "http://h/hook?event=user-prompt-submit&agent=grok".into(),
                    body: serde_json::json!({
                        "sessionId": "saturated-prefix", "prompt": "retry later"
                    }),
                },
            ]),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack["accepted"], 1, "429 still reports the committed prefix");
    }

    #[tokio::test]
    async fn handle_hook_batch_rejects_over_client_item_cap() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let items: Vec<HookBatchItem> = (0..=MAX_HOOK_BATCH_ITEMS)
            .map(|i| HookBatchItem {
                url: format!("http://h/hook?event=user-prompt-submit&agent=claude-code&i={i}"),
                body: serde_json::json!({ "session_id": "too-many", "prompt": "nope" }),
            })
            .collect();

        let response = handle_hook_batch(State(Arc::new(state)), None, Json(items))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(ack["accepted"], 0);
    }

    #[tokio::test]
    async fn handle_hook_batch_processes_sequentially_with_one_permit() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let items = vec![
            HookBatchItem {
                url: "http://h/hook?event=session-start&agent=claude-code".into(),
                body: serde_json::json!({ "session_id": "bounded-batch" }),
            },
            HookBatchItem {
                url: "http://h/hook?event=user-prompt-submit&agent=claude-code".into(),
                body: serde_json::json!({ "session_id": "bounded-batch", "prompt": "hello" }),
            },
        ];

        let response = handle_hook_batch(State(Arc::new(state)), None, Json(items))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let ack: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            ack["accepted"], 2,
            "batch processing is sequential, so one permit is enough for processed items"
        );
    }

    #[test]
    fn parse_hook_query_reads_event_and_agent() {
        let q = parse_hook_query("http://h/hook?event=stop&agent=claude-code&cwd=%2Ftmp");
        assert_eq!(q.event, "stop");
        assert_eq!(q.agent.as_deref(), Some("claude-code"));
        assert_eq!(q.cwd.as_deref(), Some("/tmp"));
        // No query string at all → default (empty event), which `process` skips.
        assert_eq!(parse_hook_query("http://h/hook").event, "");
    }

    /// An event without a cwd must fall back to the server defaults.
    #[tokio::test]
    async fn process_with_missing_cwd_falls_back_to_state_defaults() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws, proj) = resolve_project_ids(
            &state,
            None,
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(ws, state.workspace_id);
        assert_eq!(proj, state.project_id);

        // Likewise for an empty string.
        let (ws2, proj2) = resolve_project_ids(
            &state,
            Some(""),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(ws2, state.workspace_id);
        assert_eq!(proj2, state.project_id);

        // A cwd-less event must NOT publish the scratch fallback as the
        // active project — that would re-introduce the issue #2 bug of
        // MCP reads defaulting to an empty scratch bucket.
        assert!(state.active_project.get().is_none());
    }

    /// Post-merge audit (the orphan-observation finding): a hook
    /// whose cwd sits INSIDE an existing project's tree must resolve
    /// to that parent — never auto-create a sibling project from
    /// `basename(cwd)`. Pre-fix: an agent's tool call reporting
    /// `cwd = /repo/manga-plus/reader` would create a separate
    /// `reader` project and dump observations there even though the
    /// real session was attributed to `manga-plus`.
    #[tokio::test]
    async fn resolve_uses_existing_parent_when_cwd_is_inside() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        // Seed the parent project at `/repo/manga-plus`.
        let ws: engram_core::WorkspaceId = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        let parent_id: engram_core::ProjectId = state
            .writer
            .get_or_create_project(
                ws,
                String::from("manga-plus"),
                Some(String::from("/repo/manga-plus")),
            )
            .await
            .unwrap();

        // Fire a hook with a cwd two levels deep into the parent.
        let (resolved_ws, resolved_proj) = resolve_project_ids(
            &state,
            Some("/repo/manga-plus/reader/src"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(resolved_ws, ws);
        assert_eq!(
            resolved_proj, parent_id,
            "cwd inside the parent's tree must resolve to the parent, not a \
             new `src` / `reader` fragment"
        );

        // And no fragment project was created — the resolver short-
        // circuited before `get_or_create_project`.
        let frag = state
            .reader
            .find_project(ws, String::from("src"))
            .await
            .unwrap();
        assert!(frag.is_none(), "no `src` fragment project should exist");
        let frag = state
            .reader
            .find_project(ws, String::from("reader"))
            .await
            .unwrap();
        assert!(frag.is_none(), "no `reader` fragment project should exist");
    }

    /// A more-specific declared sub-project (one whose `repo_path` is
    /// itself a child of an outer project's `repo_path`) must rank
    /// AHEAD of the outer parent. This is how `.engram.toml` markers
    /// keep working — the marker materialises a row with a longer
    /// `repo_path`, and `find_project_by_cwd_prefix`'s
    /// `ORDER BY length(repo_path) DESC` picks it.
    #[tokio::test]
    async fn resolve_prefers_more_specific_sub_project_over_outer_parent() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let ws = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        let _outer = state
            .writer
            .get_or_create_project(
                ws,
                String::from("manga-plus"),
                Some(String::from("/repo/manga-plus")),
            )
            .await
            .unwrap();
        let inner = state
            .writer
            .get_or_create_project(
                ws,
                String::from("reader-app"),
                Some(String::from("/repo/manga-plus/reader")),
            )
            .await
            .unwrap();

        let (_ws, resolved) = resolve_project_ids(
            &state,
            Some("/repo/manga-plus/reader/src"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            resolved, inner,
            "longer-prefix sub-project must win over outer parent"
        );
    }

    /// Boundary: prefix-match is workspace-scoped. A project in
    /// workspace A whose `repo_path` would otherwise match a cwd
    /// must NEVER be picked when the hook event resolves to workspace
    /// B (a `workspace_override` carried in the event's query string).
    #[tokio::test]
    async fn resolve_does_not_leak_across_workspaces_on_prefix_match() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let other_ws = state
            .writer
            .get_or_create_workspace(String::from("other"))
            .await
            .unwrap();
        // Parent project lives in `other`, not in the default workspace.
        let other_parent_id = state
            .writer
            .get_or_create_project(
                other_ws,
                String::from("manga-plus"),
                Some(String::from("/repo/manga-plus")),
            )
            .await
            .unwrap();

        // Hook fires WITHOUT `workspace` override, so it resolves to
        // the default workspace. The `other` project must not be picked.
        let (resolved_ws, resolved_proj) = resolve_project_ids(
            &state,
            Some("/repo/manga-plus/reader"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(resolved_ws, other_ws);
        assert_ne!(
            resolved_proj, other_parent_id,
            "must not pick a project from a foreign workspace"
        );
    }

    /// Boundary: a stored `repo_path` whose value is degenerate
    /// (empty, single slash, trailing slash) MUST NOT match every
    /// cwd. The WHERE filters reject each shape; this asserts the
    /// integrated behaviour end-to-end.
    #[tokio::test]
    async fn resolve_ignores_degenerate_repo_paths() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let ws = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        // Poison rows that would match too broadly without the safety filters.
        // New trailing-slash repo paths are normalized at the store write
        // boundary; legacy raw trailing separators are covered in store tests.
        for (name, repo) in [
            ("empty-repo", String::new()),
            ("root-repo", String::from("/")),
        ] {
            state
                .writer
                .get_or_create_project(ws, String::from(name), Some(repo))
                .await
                .unwrap();
        }

        // Resolve a cwd that the poison rows would each match
        // pre-fix. Expect: a NEW project created by basename.
        let (resolved_ws, resolved) = resolve_project_ids(
            &state,
            Some("/repo/foo/bar"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let by_name = state
            .reader
            .find_project(resolved_ws, String::from("bar"))
            .await
            .unwrap();
        assert_eq!(
            by_name,
            Some(resolved),
            "degenerate repo_paths must NOT match — fall through to create"
        );
    }

    /// Boundary: `/foo/bar` MUST NOT match a stored `/foo/ba` sibling
    /// (the `/` boundary on the descendant arm).
    #[tokio::test]
    async fn resolve_does_not_match_sibling_substring() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let ws = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        state
            .writer
            .get_or_create_project(
                ws,
                String::from("foo-ba"),
                Some(String::from("/repo/foo-ba")),
            )
            .await
            .unwrap();
        let (resolved_ws, resolved) = resolve_project_ids(
            &state,
            Some("/repo/foo-bar"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let by_name = state
            .reader
            .find_project(resolved_ws, String::from("foo-bar"))
            .await
            .unwrap();
        assert_eq!(
            by_name,
            Some(resolved),
            "sibling substring (`foo-ba` vs `foo-bar`) must not match"
        );
    }

    /// Boundary: a cwd containing dot-segments (`/foo/../bar`,
    /// `/./x`) is rejected by the canonicaliser so it can't be
    /// LIKE-matched against an unrelated parent.
    #[tokio::test]
    async fn resolve_ignores_cwds_with_dot_segments() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let ws = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        let parent_id = state
            .writer
            .get_or_create_project(
                ws,
                String::from("manga-plus"),
                Some(String::from("/repo/manga-plus")),
            )
            .await
            .unwrap();
        for cwd in [
            "/repo/manga-plus/../other",
            "/repo/./manga-plus/x",
            "/repo/manga-plus/./y",
        ] {
            let (_ws, resolved) = resolve_project_ids(
                &state,
                Some(cwd),
                None,
                None,
                ProjectStrategy::Basename,
                &engram_core::ActorKey::default(),
            )
            .await
            .unwrap();
            assert_ne!(
                resolved, parent_id,
                "cwd `{cwd}` contains a dot-segment — must NOT match the parent"
            );
        }
    }

    /// Boundary: a stored `repo_path` containing LIKE wildcards
    /// (`%`, `_`) MUST NOT widen the match set.
    #[tokio::test]
    async fn resolve_ignores_repo_paths_with_like_wildcards() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let ws = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        state
            .writer
            .get_or_create_project(
                ws,
                String::from("poison-percent"),
                Some(String::from("/repo/anything%/poison")),
            )
            .await
            .unwrap();
        state
            .writer
            .get_or_create_project(
                ws,
                String::from("poison-underscore"),
                Some(String::from("/repo/anyth_ng")),
            )
            .await
            .unwrap();
        let (resolved_ws, resolved) = resolve_project_ids(
            &state,
            Some("/repo/anything-foo/poison/sub"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let by_name = state
            .reader
            .find_project(resolved_ws, String::from("sub"))
            .await
            .unwrap();
        assert_eq!(
            by_name,
            Some(resolved),
            "stored repo_path with LIKE wildcards must NOT match"
        );
    }

    /// A real `repo_path` containing a `_` must prefix-match its literal child
    /// cwd (escaped, not rejected) AND must NOT match a path that differs only
    /// where the `_` sits — proving `_` is literal, never a single-char
    /// wildcard. Pre-fix, both the cwd `_` rejection and the repo_path `_`
    /// rejection made any `my_app`-style project silently un-matchable.
    #[tokio::test]
    async fn resolve_matches_literal_underscore_repo_path() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let ws = state
            .writer
            .get_or_create_workspace(String::from(DEFAULT_WORKSPACE_NAME))
            .await
            .unwrap();
        let parent = state
            .writer
            .get_or_create_project(
                ws,
                String::from("my_app"),
                Some(String::from("/repo/my_app")),
            )
            .await
            .unwrap();

        // Literal child → resolves to the existing `my_app` project.
        let (_, resolved) = resolve_project_ids(
            &state,
            Some("/repo/my_app/src"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            resolved, parent,
            "a repo_path with `_` must prefix-match its literal child"
        );

        // `/repo/myXapp/...` must NOT match `/repo/my_app` (the `_` is literal,
        // not a wildcard that would match the `X`).
        let (_, other) = resolve_project_ids(
            &state,
            Some("/repo/myXapp/src"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            other, parent,
            "`_` must be literal, not a single-character LIKE wildcard"
        );
    }

    /// Cold-start preservation: when NO existing project's `repo_path`
    /// prefix-matches the cwd, the resolver must fall through to the
    /// previous create-by-basename behaviour. This is the "first time
    /// you ever ran engram from this repo" path; auto-creation
    /// stays the default for new projects.
    #[tokio::test]
    async fn resolve_falls_through_to_create_when_no_prefix_matches() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let (ws, resolved) = resolve_project_ids(
            &state,
            Some("/repo/brand-new"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        // Look the resolved project up by id via the inverse — find by
        // expected name and assert it's the same id.
        let by_name = state
            .reader
            .find_project(ws, String::from("brand-new"))
            .await
            .unwrap();
        assert_eq!(
            by_name,
            Some(resolved),
            "no parent match → fall through to create-by-basename"
        );
    }

    /// Regression for #103: a session first opened in a non-git ancestor
    /// directory (e.g. $HOME) must not become a catch-all `repo_path` that
    /// swallows real git projects nested beneath it. The ancestor stores a
    /// NULL repo_path (not the bare cwd), so a later cwd inside a real repo
    /// resolves to its own project.
    #[tokio::test]
    async fn nongit_ancestor_does_not_become_repo_path_catch_all() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let home = tmp.path().join("home"); // non-git ancestor
        std::fs::create_dir_all(&home).unwrap();
        let (_ws_h, proj_home) = resolve_project_ids(
            &state,
            Some(home.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        let app = home.join("projects").join("app"); // real git repo under it
        init_repo_with_commit(&app);
        let (ws_app, proj_app) = resolve_project_ids(
            &state,
            Some(app.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(
            proj_app, proj_home,
            "a cwd inside a real repo must not resolve to the non-git ancestor it sits under"
        );
        assert_eq!(
            state
                .reader
                .find_project(ws_app, "app".to_string())
                .await
                .unwrap(),
            Some(proj_app),
            "nested repo cwd must resolve to its own 'app' project",
        );
    }

    /// Regression for the explicit project override path of #103: a marker or
    /// query override in a non-git ancestor must not persist that ancestor as a
    /// catch-all `repo_path`.
    #[tokio::test]
    async fn project_override_nongit_ancestor_does_not_become_repo_path_catch_all() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let (_ws_h, proj_home_override) = resolve_project_ids(
            &state,
            Some(home.to_str().unwrap()),
            None,
            Some("home-override"),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        let app = home.join("projects").join("app");
        init_repo_with_commit(&app);
        let (ws_app, proj_app) = resolve_project_ids(
            &state,
            Some(app.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(
            proj_app, proj_home_override,
            "a non-git override cwd must not capture nested real repos via repo_path prefix"
        );
        assert_eq!(
            state
                .reader
                .find_project(ws_app, "app".to_string())
                .await
                .unwrap(),
            Some(proj_app),
            "nested repo cwd must resolve to its own 'app' project",
        );
    }

    /// Under the default `Basename` strategy, the first hook fired from a
    /// repo *subdirectory* must store its repo_path as the subdir (or NULL),
    /// never the whole repo root. Storing the repo root would turn the leaf
    /// project into a catch-all whose prefix swallows the repo root itself
    /// and every sibling subdir (issue #103).
    #[tokio::test]
    async fn basename_subdir_first_does_not_capture_whole_repo() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let repo = tmp.path().join("myrepo");
        init_repo_with_commit(&repo);

        // First visit is a subdir, so the leaf project is created first.
        let backend = repo.join("backend");
        std::fs::create_dir_all(&backend).unwrap();
        let (_ws_b, proj_backend) = resolve_project_ids(
            &state,
            Some(backend.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // A sibling subdir must become its own project, not be captured by
        // the first-visited subdir's project via prefix-match.
        let frontend = repo.join("frontend");
        std::fs::create_dir_all(&frontend).unwrap();
        let (_ws_f, proj_frontend) = resolve_project_ids(
            &state,
            Some(frontend.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            proj_frontend, proj_backend,
            "a sibling subdir must not be captured by the first-visited subdir's project",
        );

        // The repo root itself must not be captured by a leaf subdir project.
        let (_ws_r, proj_root) = resolve_project_ids(
            &state,
            Some(repo.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            proj_root, proj_backend,
            "the repo root must not be captured by a leaf subdir project",
        );
    }

    #[tokio::test]
    async fn process_with_root_cwd_falls_back_to_state_defaults() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws, proj) = resolve_project_ids(
            &state,
            Some("/"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(ws, state.workspace_id);
        assert_eq!(proj, state.project_id);
        assert_eq!(state.active_project.get(), Some((ws, proj)));
    }

    #[test]
    fn resolve_session_id_hashes_agent_ids_deterministically() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("opencode".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "opencode-session-123" }),
        );

        let first = resolve_session_id(&env).unwrap();
        let second = resolve_session_id(&env).unwrap();
        assert_eq!(first, second);
    }

    /// A second call for the same cwd must hit the in-memory cache — no
    /// additional `get_or_create_project` writes happen, proven by
    /// inspecting the cache after both calls.
    #[tokio::test]
    async fn project_cache_hits_on_second_event() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let cwd = "/home/user/cached-project";

        // First call — populates the cache.
        let (_, proj_first) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // Inspect the cache: should have exactly one entry.
        {
            let cache = state.project_cache.lock().await;
            assert_eq!(cache.len(), 1, "cache has one entry after first call");
            let key = (
                cwd.to_string(),
                String::new(),
                String::new(),
                ProjectStrategy::Basename.as_str().to_string(),
            );
            assert!(
                cache.contains_key(&key),
                "cache keyed by (cwd, ws_override, proj_override, project_strategy)"
            );
        }

        // Second call — must return the same IDs from the cache.
        let (_, proj_second) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(proj_first, proj_second, "cache must return identical IDs");

        // Cache must still have exactly one entry (no duplicate insert).
        {
            let cache = state.project_cache.lock().await;
            assert_eq!(cache.len(), 1, "no duplicate cache entries");
        }
    }

    #[test]
    fn project_cache_store_evicts_oldest_untouched_entry() {
        let mut cache = ProjectCacheStore::new(2);
        let key_a = ("/a".into(), String::new(), String::new(), "basename".into());
        let key_b = ("/b".into(), String::new(), String::new(), "basename".into());
        let key_c = ("/c".into(), String::new(), String::new(), "basename".into());

        cache.insert(key_a.clone(), (WorkspaceId::new(), ProjectId::new()));
        cache.insert(key_b.clone(), (WorkspaceId::new(), ProjectId::new()));
        assert!(
            cache.get(&key_a).is_some(),
            "touch key_a so key_b is oldest"
        );
        cache.insert(key_c.clone(), (WorkspaceId::new(), ProjectId::new()));

        assert!(cache.contains_key(&key_a));
        assert!(!cache.contains_key(&key_b));
        assert!(cache.contains_key(&key_c));
        assert_eq!(cache.len(), 2);
    }

    /// If the cached project is deleted out from under the router (e.g.
    /// `purge-project` on a live server), the next event must self-heal:
    /// evict the stale slot, recreate the project, and ingest — instead of
    /// failing forever on the `sessions.project_id` foreign key.
    #[tokio::test]
    async fn process_self_heals_when_cached_project_purged() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/user/heal-project";

        // 1) First event creates + caches the project (and a session).
        let env1 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-sess-1" }),
        );
        process(&state, env1, None).await.unwrap();
        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // 2) Purge the project — the DB row is gone but the cache still
        //    points at it (exactly the purge-on-live-server scenario).
        state
            .writer
            .purge_project(ws, proj, "default/heal-project")
            .await
            .unwrap();
        assert!(
            state
                .project_cache
                .lock()
                .await
                .values()
                .any(|ids| *ids == (ws, proj)),
            "cache still holds the now-deleted project id"
        );

        // 3) Next event with the same cwd must NOT error on the stale FK —
        //    the router evicts, recreates, and ingests.
        let env2 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-sess-2" }),
        );
        process(&state, env2, None)
            .await
            .expect("self-heal: stale cached project must be recreated, not FK-fail");

        // 4) The project was recreated (fresh id) and the event landed.
        let (_, proj_new) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            proj_new, proj,
            "purged project must be replaced by a fresh one"
        );
        let counts = state.reader.status_counts().await.unwrap();
        assert!(counts.sessions >= 1, "recreated session must be persisted");
    }

    /// The move-project hazard the (workspace_id, project_id) pairing trigger
    /// exists for: when a cached project is MOVED to another workspace out from
    /// under the router, the same `project_id` now belongs to a new workspace.
    /// The next event must NOT silently write a split-brain row with the stale
    /// workspace id — the trigger aborts that write, and the router evicts +
    /// re-resolves into a consistent pair (exactly like the purge self-heal).
    #[tokio::test]
    async fn process_self_heals_when_cached_project_moved_workspaces() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/user/move-project";

        // 1) First event creates + caches the project (in the default workspace).
        let env1 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "move-sess-1" }),
        );
        process(&state, env1, None).await.unwrap();
        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // 2) Move the project to another workspace (re-stamp workspace_id, same
        //    project_id) — the cache still points at (default_ws, proj), now a
        //    cross-workspace stale pair.
        let dst_ws = state
            .writer
            .get_or_create_workspace("archive".to_string())
            .await
            .unwrap();
        state
            .writer
            .move_project_workspace(proj, ws, dst_ws)
            .await
            .unwrap();
        assert!(
            state
                .project_cache
                .lock()
                .await
                .values()
                .any(|ids| *ids == (ws, proj)),
            "cache still holds the moved project's stale (workspace, project) pair"
        );

        // 3) Next event with the same cwd must NOT create a split-brain row: the
        //    stale (default_ws, proj) write trips the pairing trigger, the router
        //    evicts + re-resolves, and the event lands cleanly.
        let env2 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "move-sess-2" }),
        );
        process(&state, env2, None)
            .await
            .expect("self-heal: stale cross-workspace pair must re-resolve, not write split-brain");

        // 4) The moved project stayed in `dst_ws`; the cwd re-resolved to a
        //    FRESH project back in the default workspace (never the stale pair).
        assert_eq!(
            state
                .reader
                .find_project(dst_ws, "move-project".to_string())
                .await
                .unwrap(),
            Some(proj),
            "moved project keeps its id in the destination workspace"
        );
        let (ws_new, proj_new) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_eq!(ws_new, ws, "re-resolved back into the default workspace");
        assert_ne!(
            proj_new, proj,
            "a fresh project replaced the moved one for this cwd"
        );
    }

    #[tokio::test]
    async fn process_self_heal_evicts_project_strategy_cache_slot() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let repo_dir = tmp.path().join("repo-root-project");
        init_repo_with_commit(&repo_dir);
        let app_dir = repo_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let cwd = app_dir.to_str().unwrap();

        let env1 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                project_strategy: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-repo-root-1" }),
        );
        process(&state, env1, None).await.unwrap();
        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        state
            .writer
            .purge_project(ws, proj, "default/repo-root-project")
            .await
            .unwrap();

        let env2 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                project_strategy: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-repo-root-2" }),
        );
        process(&state, env2, None)
            .await
            .expect("repo-root cache slot must be evicted and recreated");

        let (_, proj_new) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(proj_new, proj);
    }

    /// A hook event fires end-to-end through `process`. Validates that
    /// the session + observation rows land in the resolved project, not
    /// the server-default scratch project.
    #[tokio::test]
    async fn process_routes_observation_to_cwd_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "session-start".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "test-session-cwd-routing",
                "cwd": "/home/user/my-project",
            }),
        );

        process(&state, env, None).await.unwrap();

        // The observation must be in the project derived from the cwd,
        // not in the server-default `scratch` project.
        let (_, expected_proj) = resolve_project_ids(
            &state,
            Some("/home/user/my-project"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            expected_proj, state.project_id,
            "routing must not use server-default project"
        );
    }

    /// SessionEnd must always write the heuristic `sessions/<id>.md` page,
    /// even with `consolidate_on_session_end` enabled but no LLM provider:
    /// the opt-in LLM pass is additive and guarded by a present
    /// `consolidator`, so flag-on + no-provider degrades to today's
    /// deterministic behavior (issue #40 — no regression).
    #[tokio::test]
    async fn session_end_writes_heuristic_page_even_with_consolidate_flag_on() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.consolidate_on_session_end = true; // flag on; consolidator stays None

        let sid = "11111111-1111-1111-1111-111111111111";
        for event in ["session-start", "session-end"] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: event.into(),
                    agent: Some("claude-code".into()),
                    ..Default::default()
                },
                serde_json::json!({ "session_id": sid }),
            );
            process(&state, env, None).await.unwrap();
        }

        let pages = state
            .reader
            .recent_pages_for_project(state.workspace_id, state.project_id, 20)
            .await
            .unwrap();
        assert!(
            pages
                .iter()
                .any(|p| p.path.as_str().starts_with("sessions/")),
            "SessionEnd must write a heuristic sessions/<id>.md page regardless of the flag; got {:?}",
            pages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn stop_does_not_end_session() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let sid = "22222222-2222-2222-2222-222222222222";

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "stop".into(),
                agent: Some("codex".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": sid }),
        );
        process(&state, env, None).await.unwrap();

        let completed = state
            .reader
            .latest_completed_session_for_project(state.workspace_id, state.project_id)
            .await
            .unwrap();
        assert!(
            completed.is_none(),
            "Stop must not be treated as SessionEnd"
        );
    }

    #[tokio::test]
    async fn session_end_closes_only_matching_scoped_session() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let target = state
            .writer
            .get_or_create_project(state.workspace_id, "target", None)
            .await
            .unwrap();
        let other = state
            .writer
            .get_or_create_project(state.workspace_id, "other", None)
            .await
            .unwrap();
        let target_sid = SessionId::new();
        let other_project_sid = SessionId::new();
        let other_agent_sid = SessionId::new();
        for (id, project_id, agent) in [
            (target_sid, target, AgentKind::Codex),
            (other_project_sid, other, AgentKind::Codex),
            (other_agent_sid, target, AgentKind::ClaudeCode),
        ] {
            state
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: state.workspace_id,
                    project_id,
                    agent_kind: agent,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                })
                .await
                .unwrap();
        }

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "session-end".into(),
                agent: Some("codex".into()),
                cwd: Some("/tmp/target".into()),
                workspace: Some("default".into()),
                project: Some("target".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": target_sid.to_string(), "cwd": "/tmp/target" }),
        );
        process(&state, env, None).await.unwrap();

        assert_eq!(
            state
                .reader
                .latest_completed_session_for_project(state.workspace_id, target)
                .await
                .unwrap(),
            Some(target_sid)
        );
        assert_eq!(
            state
                .reader
                .open_sessions_for_scope_agent(state.workspace_id, other, AgentKind::Codex, None)
                .await
                .unwrap()
                .len(),
            1,
            "other project Codex session must remain open"
        );
        assert_eq!(
            state
                .reader
                .open_sessions_for_scope_agent(
                    state.workspace_id,
                    target,
                    AgentKind::ClaudeCode,
                    None
                )
                .await
                .unwrap()
                .len(),
            1,
            "other agent session in same project must remain open"
        );
    }

    #[tokio::test]
    async fn mismatched_session_end_does_not_create_summary_or_handoff() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let target = state
            .writer
            .get_or_create_project(state.workspace_id, "target", None)
            .await
            .unwrap();
        let other = state
            .writer
            .get_or_create_project(state.workspace_id, "other", None)
            .await
            .unwrap();
        let wrong_project_sid = SessionId::new();
        let wrong_agent_sid = SessionId::new();
        for (id, project_id, agent) in [
            (wrong_project_sid, other, AgentKind::Codex),
            (wrong_agent_sid, target, AgentKind::ClaudeCode),
        ] {
            state
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: state.workspace_id,
                    project_id,
                    agent_kind: agent,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                })
                .await
                .unwrap();
        }

        for sid in [wrong_project_sid, wrong_agent_sid] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: "session-end".into(),
                    agent: Some("codex".into()),
                    cwd: Some("/tmp/target".into()),
                    workspace: Some("default".into()),
                    project: Some("target".into()),
                    ..Default::default()
                },
                serde_json::json!({ "session_id": sid.to_string(), "cwd": "/tmp/target" }),
            );
            process(&state, env, None).await.unwrap();
        }

        let pages = state
            .reader
            .recent_pages_for_project(state.workspace_id, target, 20)
            .await
            .unwrap();
        assert!(
            pages
                .iter()
                .all(|p| !p.path.as_str().starts_with("sessions/")),
            "mismatched SessionEnd must not write target summary pages: {:?}",
            pages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>()
        );
        assert!(
            state
                .reader
                .latest_open_handoff(state.workspace_id, target, None)
                .await
                .unwrap()
                .is_none(),
            "mismatched SessionEnd must not create a target handoff"
        );
    }

    #[tokio::test]
    async fn already_ended_session_end_does_not_create_summary_or_handoff() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let target = state
            .writer
            .get_or_create_project(state.workspace_id, "target", None)
            .await
            .unwrap();
        let sid = SessionId::new();
        state
            .writer
            .begin_session(NewSession {
                id: sid,
                workspace_id: state.workspace_id,
                project_id: target,
                agent_kind: AgentKind::Codex,
                cwd: Some(std::path::PathBuf::from("/tmp/target")),
            })
            .await
            .unwrap();
        state.writer.end_session(sid, None).await.unwrap();

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "session-end".into(),
                agent: Some("codex".into()),
                cwd: Some("/tmp/target".into()),
                workspace: Some("default".into()),
                project: Some("target".into()),
                ..Default::default()
            },
            serde_json::json!({ "session_id": sid.to_string(), "cwd": "/tmp/target" }),
        );
        process(&state, env, None).await.unwrap();

        let pages = state
            .reader
            .recent_pages_for_project(state.workspace_id, target, 20)
            .await
            .unwrap();
        assert!(
            pages
                .iter()
                .all(|p| !p.path.as_str().starts_with("sessions/")),
            "already-ended synthetic SessionEnd must not write summary pages"
        );
        assert!(
            state
                .reader
                .latest_open_handoff(state.workspace_id, target, None)
                .await
                .unwrap()
                .is_none(),
            "already-ended synthetic SessionEnd must not create a handoff"
        );
    }

    // Issue #152: an agent that resumes an ended session under the same id
    // and keeps working must get its page re-compiled by the second
    // SessionEnd instead of that end being dropped as "already-ended".
    #[tokio::test]
    async fn resumed_session_second_end_reruns_end_path() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let sid = "33333333-3333-3333-3333-333333333333";
        let session_id: SessionId = sid.parse().unwrap();
        let fire = |event: &str, tool: Option<&str>| {
            let mut body = serde_json::json!({ "session_id": sid });
            if let Some(tool) = tool {
                body["tool_name"] = serde_json::Value::String(tool.into());
            }
            HookEnvelope::from_query_and_body(
                HookQuery {
                    event: event.into(),
                    agent: Some("claude-code".into()),
                    ..Default::default()
                },
                body,
            )
        };

        // First life: one tool call, then a real end.
        process(&state, fire("post-tool-use", Some("Bash")), None)
            .await
            .unwrap();
        process(&state, fire("session-end", None), None)
            .await
            .unwrap();
        let disposition = state
            .reader
            .session_end_disposition(
                session_id,
                state.workspace_id,
                state.project_id,
                AgentKind::ClaudeCode,
            )
            .await
            .unwrap();
        assert_eq!(
            disposition,
            engram_store::SessionEndDisposition::DropStale,
            "a freshly-ended session with no newer work must drop duplicate ends"
        );
        let page_after_first_end = state
            .reader
            .recent_pages_for_project(state.workspace_id, state.project_id, 20)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.path.as_str().starts_with("sessions/"))
            .expect("first SessionEnd writes the session page");

        // Second life: the agent resumed the same id and did more work.
        process(&state, fire("post-tool-use", Some("Edit")), None)
            .await
            .unwrap();
        let disposition = state
            .reader
            .session_end_disposition(
                session_id,
                state.workspace_id,
                state.project_id,
                AgentKind::ClaudeCode,
            )
            .await
            .unwrap();
        assert_eq!(
            disposition,
            engram_store::SessionEndDisposition::ReEndWithNewWork,
            "observations after ended_at must mark the session re-endable"
        );

        process(&state, fire("session-end", None), None)
            .await
            .unwrap();

        let page_after_second_end = state
            .reader
            .recent_pages_for_project(state.workspace_id, state.project_id, 20)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.path.as_str().starts_with("sessions/"))
            .expect("second SessionEnd keeps the session page");
        // The rewrite supersedes the page, so the latest version carries a
        // new page id.
        assert_ne!(
            page_after_first_end.id, page_after_second_end.id,
            "the re-end must rewrite the session page with the resumed work"
        );
        assert!(
            state
                .reader
                .latest_open_handoff(state.workspace_id, state.project_id, None)
                .await
                .unwrap()
                .is_some(),
            "the re-end must refresh the auto-handoff"
        );
        // ended_at advanced past the resumed work: the next duplicate end is
        // dropped again (pins the de1cef2 dedupe behaviour post-re-end).
        let disposition = state
            .reader
            .session_end_disposition(
                session_id,
                state.workspace_id,
                state.project_id,
                AgentKind::ClaudeCode,
            )
            .await
            .unwrap();
        assert_eq!(
            disposition,
            engram_store::SessionEndDisposition::DropStale,
            "after the re-end, ended_at must cover the resumed work again"
        );
    }

    #[tokio::test]
    async fn process_accepts_prompt_before_session_start() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("opencode".into()),
                ..Default::default()
            },
            serde_json::json!({
                "sessionID": "opencode-resumed-session",
                "cwd": "/home/user/resumed-project",
                "prompt": "continue",
            }),
        );

        process(&state, env, None).await.unwrap();

        let counts = state.reader.status_counts().await.unwrap();
        assert_eq!(counts.sessions, 1);
        assert_eq!(counts.observations, 1);
    }

    #[tokio::test]
    async fn process_preserves_opt_in_extension_event_metadata() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                agent: Some("other".into()),
                extension: Some("fstech".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "fstech-custom-event",
                "cwd": "/home/user/crm",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );
        let session_id = resolve_session_id(&env).unwrap();

        process(&state, env, None).await.unwrap();

        let observations = state
            .reader
            .observations_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(observations.len(), 1);
        let obs = &observations[0];
        assert_eq!(obs.kind, ObservationKind::Other);
        assert_eq!(obs.extension.as_deref(), Some("fstech"));
        assert_eq!(obs.source_event.as_deref(), Some("lead.contact"));
        assert_eq!(obs.title, "Lead contacted");
        assert_eq!(obs.body, "Lead Maria requested a proposal");
        let hits = state
            .reader
            .search_observations_for_project(obs.workspace_id, obs.project_id, "maria".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "extension body should be searchable");
    }

    #[tokio::test]
    async fn process_unknown_event_without_extension_leaves_storage_clean() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                agent: Some("other".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "plain-unknown-event",
                "cwd": "/home/user/crm",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );
        let session_id = resolve_session_id(&env).unwrap();

        process(&state, env, None).await.unwrap();

        let observations = state
            .reader
            .observations_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(observations.len(), 1);
        let obs = &observations[0];
        assert_eq!(obs.kind, ObservationKind::Other);
        assert_eq!(obs.extension, None);
        assert_eq!(obs.source_event, None);
        assert_eq!(obs.title, "other");
        assert!(obs.body.is_empty());
        let hits = state
            .reader
            .search_observations_for_project(obs.workspace_id, obs.project_id, "maria".into(), 5)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "unknown events without extension must not leak custom payload into observation FTS"
        );
    }

    /// `.engram.toml` walk-up declares `workspace = "movvia"`. The hook
    /// forwards it as a query param, so the same `cwd` ends up in a
    /// distinct workspace from the default-buckets resolver path.
    #[tokio::test]
    async fn workspace_override_yields_distinct_workspace() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws_default, _) = resolve_project_ids(
            &state,
            Some("/home/u/repo"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (ws_movvia, _) = resolve_project_ids(
            &state,
            Some("/home/u/repo"),
            Some("movvia"),
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(
            ws_default, ws_movvia,
            "marker-declared workspace must not collide with the default"
        );
    }

    #[tokio::test]
    async fn handoff_with_workspace_marker_and_cwd_uses_basename_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/repo";

        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            Some("acme"),
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        state
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some(std::path::PathBuf::from(cwd)),
                summary: "handoff summary".to_string(),
                open_questions: Vec::new(),
                next_steps: vec!["continue".to_string()],
                files_touched: Vec::new(),
            })
            .await
            .unwrap();

        let rendered = fetch_and_accept_handoff(
            &state,
            HandoffQuery {
                agent: Some("codex".into()),
                cwd: Some(cwd.into()),
                workspace: Some("acme".into()),
                project: None,
                project_strategy: None,
                briefing: None,
                briefing_budget: None,
            },
            None,
        )
        .await
        .unwrap();

        assert!(
            rendered.as_deref().is_some_and(|s| s.contains("continue")),
            "workspace-only marker handoff lookup must resolve workspace + basename(cwd)"
        );
    }

    fn brief_page(
        ws: engram_core::WorkspaceId,
        proj: engram_core::ProjectId,
        path: &str,
        body: &str,
        pinned: bool,
    ) -> engram_core::NewPage {
        engram_core::NewPage {
            workspace_id: ws,
            project_id: proj,
            path: engram_core::PagePath::new(path).unwrap(),
            title: path.trim_end_matches(".md").to_string(),
            body: body.into(),
            tier: engram_core::Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned,
            links: Vec::new(),
            author_id: None,
        }
    }

    /// `briefing=true` on the `/handoff` query returns the compiled project
    /// brief even with NO pending handoff — the `/clear` case of #176 — and
    /// a truthy value combined with a pending handoff returns both, handoff
    /// first. A non-truthy value leaves the endpoint's contract unchanged.
    #[tokio::test]
    async fn handoff_query_briefing_flag_appends_project_brief() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/briefed-repo";

        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        state
            .writer
            .upsert_page(brief_page(
                ws,
                proj,
                "_rules/style.md",
                "always use the single writer actor",
                false,
            ))
            .await
            .unwrap();

        let query = |briefing: Option<&str>| HandoffQuery {
            agent: Some("claude-code".into()),
            cwd: Some(cwd.into()),
            workspace: None,
            project: None,
            project_strategy: None,
            briefing: briefing.map(str::to_owned),
            briefing_budget: None,
        };

        // Non-truthy opt-in: no handoff pending, nothing to inject.
        let rendered = fetch_and_accept_handoff(&state, query(Some("false")), None)
            .await
            .unwrap();
        assert!(
            rendered.is_none(),
            "non-truthy briefing flag must not inject anything"
        );

        // Truthy opt-in, no pending handoff: brief alone (the /clear case).
        let rendered = fetch_and_accept_handoff(&state, query(Some("true")), None)
            .await
            .unwrap()
            .expect("brief must be injected without a pending handoff");
        assert!(
            rendered.contains("project brief") && rendered.contains("single writer actor"),
            "brief must carry the rules page body: {rendered}"
        );
        assert!(
            rendered.contains("do NOT re-explore"),
            "brief must end with the agent-facing reading instructions"
        );

        // Truthy opt-in with a pending handoff: handoff first, brief after.
        state
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some(std::path::PathBuf::from(cwd)),
                summary: "resume the auth refactor".to_string(),
                open_questions: Vec::new(),
                next_steps: Vec::new(),
                files_touched: Vec::new(),
            })
            .await
            .unwrap();
        let rendered = fetch_and_accept_handoff(&state, query(Some("true")), None)
            .await
            .unwrap()
            .expect("handoff + brief must both be injected");
        let handoff_pos = rendered.find("resume the auth refactor").unwrap();
        let brief_pos = rendered.find("project brief").unwrap();
        assert!(
            handoff_pos < brief_pos,
            "pending handoff must precede the brief"
        );
    }

    /// The brief renderer respects the char budget: an over-budget body is
    /// truncated with a visible note, fully crowded-out core pages are
    /// listed as omitted, and an empty project renders nothing at all.
    #[test]
    fn render_session_brief_enforces_budget() {
        let core = vec![
            engram_store::BriefPageBody {
                path: "_rules/a.md".into(),
                title: "a".into(),
                body: "x".repeat(2_000),
                pinned: true,
                updated_at: "2026-07-12T00:00:00Z".into(),
            },
            engram_store::BriefPageBody {
                path: "_rules/b.md".into(),
                title: "b".into(),
                body: "never truncated into view".into(),
                pinned: false,
                updated_at: "2026-07-12T00:00:00Z".into(),
            },
        ];
        let recent = vec![engram_store::BriefingPage {
            path: "concepts/q.md".into(),
            title: "queue".into(),
            kind: "fact".into(),
            updated_at: "2026-07-12T00:00:00Z".into(),
        }];

        let out = render_session_brief(&core, &recent, BRIEF_BUDGET_MIN).unwrap();
        assert!(
            out.contains("[truncated by `[briefing] max_chars`]"),
            "over-budget body must be visibly truncated: {out}"
        );
        assert!(
            out.contains("Core pages omitted by budget") && out.contains("`_rules/b.md`"),
            "crowded-out core pages must be listed by path: {out}"
        );
        assert!(
            !out.contains("never truncated into view"),
            "omitted page bodies must not leak: {out}"
        );
        assert!(
            out.contains("Recently updated pages") && out.contains("concepts/q.md"),
            "recent pointers survive the budget cut: {out}"
        );

        // Multi-byte safety: a body of 4-byte chars must cut on a boundary.
        let emoji_core = vec![engram_store::BriefPageBody {
            path: "_rules/e.md".into(),
            title: "e".into(),
            body: "🦀".repeat(1_000),
            pinned: false,
            updated_at: "2026-07-12T00:00:00Z".into(),
        }];
        let out = render_session_brief(&emoji_core, &[], BRIEF_BUDGET_MIN).unwrap();
        assert!(out.is_char_boundary(out.len()), "must remain valid UTF-8");

        assert!(
            render_session_brief(&[], &[], BRIEF_BUDGET_DEFAULT).is_none(),
            "empty project must inject nothing"
        );
    }

    #[tokio::test]
    async fn handoff_with_no_marker_uses_cwd_basename_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/plain-repo";

        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        state
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some(std::path::PathBuf::from(cwd)),
                summary: "handoff summary".to_string(),
                open_questions: Vec::new(),
                next_steps: vec!["resume plain repo".to_string()],
                files_touched: Vec::new(),
            })
            .await
            .unwrap();

        let rendered = fetch_and_accept_handoff(
            &state,
            HandoffQuery {
                agent: Some("codex".into()),
                cwd: Some(cwd.into()),
                workspace: None,
                project: None,
                project_strategy: None,
                briefing: None,
                briefing_budget: None,
            },
            None,
        )
        .await
        .unwrap();

        assert!(
            rendered
                .as_deref()
                .is_some_and(|s| s.contains("resume plain repo")),
            "no-marker handoff lookup must still resolve basename(cwd)"
        );
    }

    /// A marker file with `project = "pe-portais"` replaces the
    /// basename-derived project name for every descendant `cwd`.
    #[tokio::test]
    async fn project_override_replaces_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (_, proj_basename) = resolve_project_ids(
            &state,
            Some("/home/u/api"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (_, proj_override) = resolve_project_ids(
            &state,
            Some("/home/u/api"),
            None,
            Some("pe-portais"),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(
            proj_basename, proj_override,
            "project override must produce a different ProjectId than basename(cwd)"
        );
    }

    /// Two events resolved with overrides land in the same `(ws, proj)`
    /// pair as long as the override names match — even if the `cwd`
    /// differs. Confirms the override is the source of truth.
    #[tokio::test]
    async fn matching_overrides_collapse_to_same_pair() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws_a, proj_a) = resolve_project_ids(
            &state,
            Some("/x"),
            Some("acme"),
            Some("api"),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (ws_b, proj_b) = resolve_project_ids(
            &state,
            Some("/y"),
            Some("acme"),
            Some("api"),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(ws_a, ws_b);
        assert_eq!(proj_a, proj_b);
    }

    /// During a hook-script upgrade window, the same `cwd` may resolve
    /// with and without an override in the same process. The composite
    /// cache key keeps both rows isolated; otherwise the first one
    /// "wins" and the second silently inherits its `ProjectId`.
    #[tokio::test]
    async fn cache_does_not_poison_across_override_variants() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/poison-test";

        let (ws_default, _) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (ws_movvia, _) = resolve_project_ids(
            &state,
            Some(cwd),
            Some("movvia"),
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(
            ws_default, ws_movvia,
            "cache must distinguish override variants"
        );

        let cache = state.project_cache.lock().await;
        assert_eq!(
            cache.len(),
            2,
            "two distinct cache entries for same cwd with different overrides"
        );
    }

    /// With no `cwd` but with both overrides, the resolver still produces
    /// a real `(ws, proj)` pair — covers handoff fetches issued before
    /// any hook event has populated the cwd cache.
    #[tokio::test]
    async fn overrides_resolve_without_cwd() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws, proj) = resolve_project_ids(
            &state,
            None,
            Some("acme"),
            Some("api"),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(ws, state.workspace_id);
        assert_ne!(proj, state.project_id);
    }

    #[test]
    fn unknown_project_strategy_defaults_to_basename() {
        assert_eq!(
            ProjectStrategy::parse(Some("repo-root")),
            ProjectStrategy::RepoRoot
        );
        assert_eq!(
            ProjectStrategy::parse(Some("repo_root")),
            ProjectStrategy::RepoRoot
        );
        assert_eq!(
            ProjectStrategy::parse(Some("git-root")),
            ProjectStrategy::Basename
        );
    }

    #[tokio::test]
    async fn default_strategy_keeps_git_subdirs_as_basename_projects() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let main_dir = tmp.path().join("my-project");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cwd = app_dir.to_str().unwrap();

        let (_, proj_basename) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (_, proj_explicit_app) = resolve_project_ids(
            &state,
            Some(main_dir.to_str().unwrap()),
            None,
            Some("app"),
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (_, proj_repo_root) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            proj_basename, proj_explicit_app,
            "default strategy must keep project = basename(cwd) inside git repos"
        );
        assert_ne!(
            proj_basename, proj_repo_root,
            "repo-root strategy is opt-in and must not affect the basename default"
        );
    }

    #[tokio::test]
    async fn project_override_wins_over_repo_root_strategy() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let main_dir = tmp.path().join("repo");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cwd = app_dir.to_str().unwrap();

        let (_, proj_repo_root) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (_, proj_override_repo_root) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            Some("manual"),
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (_, proj_override_basename) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            Some("manual"),
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(proj_override_repo_root, proj_override_basename);
        assert_ne!(
            proj_override_repo_root, proj_repo_root,
            "explicit project override must beat repo-root derivation"
        );
    }

    #[tokio::test]
    async fn host_resolved_repo_root_override_records_repo_path_when_visible() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Canonicalize the temp root before deriving the repo paths. On macOS
        // `TempDir` lives under `/var/folders/...`, a symlink to
        // `/private/var/...`; git2's repo discovery records the resolved
        // `/private/var/...` path, and the sibling cwd below is prefix-matched
        // against it — so both sides must agree on the resolved form. (The `_`
        // in the macOS temp hash no longer breaks the match:
        // `find_project_by_cwd_prefix` now escapes `%`/`_` and matches them
        // literally, so this also exercises that fix on macOS.)
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let main_dir = root.join("repo");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        let sibling_dir = main_dir.join("sibling");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(&sibling_dir).unwrap();

        let (_, proj_from_host_override) = resolve_project_ids(
            &state,
            Some(app_dir.to_str().unwrap()),
            None,
            Some("repo"),
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        let (_, proj_from_sibling) = resolve_project_ids(
            &state,
            Some(sibling_dir.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            proj_from_sibling, proj_from_host_override,
            "host-resolved repo-root override should still record repo_path so sibling cwd prefix-matches the repo project",
        );
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn repo_root_override_stores_repo_path_in_cwd_namespace() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let real_root = tmp.path().join("real");
        let real_repo = real_root.join("repo");
        init_repo_with_commit(&real_repo);
        std::fs::create_dir_all(real_repo.join("app")).unwrap();
        std::fs::create_dir_all(real_repo.join("sibling")).unwrap();

        let alias_root = tmp.path().join("alias");
        if !create_test_symlink_dir(&real_root, &alias_root) {
            return;
        }
        let alias_app = alias_root.join("repo/app");
        let alias_sibling = alias_root.join("repo/sibling");

        let (_, proj_from_alias_override) = resolve_project_ids(
            &state,
            Some(alias_app.to_str().unwrap()),
            None,
            Some("repo"),
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        let (_, proj_from_alias_sibling) = resolve_project_ids(
            &state,
            Some(alias_sibling.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            proj_from_alias_sibling, proj_from_alias_override,
            "stored repo_path must use the incoming cwd spelling so raw prefix matching works across symlink aliases",
        );
    }

    #[tokio::test]
    async fn cache_does_not_poison_across_project_strategies() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let main_dir = tmp.path().join("repo");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cwd = app_dir.to_str().unwrap();

        let (_, proj_basename) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (_, proj_repo_root) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_ne!(proj_basename, proj_repo_root);
        let cache = state.project_cache.lock().await;
        assert_eq!(
            cache.len(),
            2,
            "same cwd must have isolated cache entries per project strategy"
        );
    }

    /// A git worktree must resolve to the same project as the main
    /// working directory only when the marker opts into repo-root identity.
    #[tokio::test]
    async fn worktree_resolves_to_same_project_as_main_repo() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Create a real git repo inside the temp dir.
        let main_dir = tmp.path().join("my-project");

        // Create a worktree in a sibling directory.
        let wt_dir = tmp.path().join("my-project-feature-branch");
        #[cfg(windows)]
        {
            init_repo_with_commit(&main_dir);
            let mut branch = std::process::Command::new("git");
            branch
                .arg("-C")
                .arg(&main_dir)
                .args(["branch", "feature-branch"]);
            assert_command_success(branch);

            let mut worktree = std::process::Command::new("git");
            worktree
                .arg("-C")
                .arg(&main_dir)
                .args(["worktree", "add", "-q"])
                .arg(&wt_dir)
                .arg("feature-branch");
            assert_command_success(worktree);
        }
        #[cfg(not(windows))]
        {
            let repo = init_repo_with_commit(&main_dir);
            let head = repo.head().unwrap().peel_to_commit().unwrap();
            // Create a branch for the worktree to check out.
            let branch = repo.branch("feature-branch", &head, false).unwrap();
            repo.worktree(
                "feature-branch",
                &wt_dir,
                Some(git2::WorktreeAddOptions::new().reference(Some(&branch.into_reference()))),
            )
            .unwrap();
        }

        let main_cwd = main_dir.to_str().unwrap();
        let wt_cwd = wt_dir.to_str().unwrap();

        let (ws_main, proj_main) = resolve_project_ids(
            &state,
            Some(main_cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        let (ws_wt, proj_wt) = resolve_project_ids(
            &state,
            Some(wt_cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(ws_main, ws_wt, "same workspace");
        assert_eq!(
            proj_main, proj_wt,
            "worktree must resolve to same project as main repo"
        );

        let (_, proj_wt_basename) = resolve_project_ids(
            &state,
            Some(wt_cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            proj_main, proj_wt_basename,
            "default strategy must not collapse worktrees into the main repo project"
        );
    }

    /// A directory that is NOT inside a git repo must still resolve
    /// via basename(cwd), preserving the existing behaviour.
    #[tokio::test]
    async fn non_git_dir_falls_back_to_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Create a plain directory (no .git).
        let plain_dir = tmp.path().join("plain-project");
        std::fs::create_dir_all(&plain_dir).unwrap();
        let cwd = plain_dir.to_str().unwrap();

        let (_, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // Must NOT be the server-default scratch project.
        assert_ne!(proj, state.project_id);

        // Resolve a second time with a different basename to prove
        // they produce distinct projects (basename-based).
        let other_dir = tmp.path().join("other-project");
        std::fs::create_dir_all(&other_dir).unwrap();
        let (_, proj2) = resolve_project_ids(
            &state,
            Some(other_dir.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(proj, proj2, "different basenames → different projects");
    }

    /// A bare repository must fall back to basename(cwd), not resolve
    /// to the grandparent directory via commondir().parent().
    #[tokio::test]
    async fn bare_repo_falls_back_to_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let bare_dir = tmp.path().join("my-bare-project.git");
        #[cfg(windows)]
        init_bare_repo(&bare_dir);
        #[cfg(not(windows))]
        git2::Repository::init_bare(&bare_dir).unwrap();
        let cwd = bare_dir.to_str().unwrap();

        let (_, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        // Must NOT be the server-default scratch project — basename should work.
        assert_ne!(proj, state.project_id);

        // The project name should come from basename, not from the grandparent.
        // To verify: resolve with a different bare repo name and confirm different project.
        let bare_dir2 = tmp.path().join("other-bare.git");
        #[cfg(windows)]
        init_bare_repo(&bare_dir2);
        #[cfg(not(windows))]
        git2::Repository::init_bare(&bare_dir2).unwrap();
        let (_, proj2) = resolve_project_ids(
            &state,
            Some(bare_dir2.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();
        assert_ne!(
            proj, proj2,
            "different bare repo basenames → different projects"
        );
    }

    /// Windows-style backslash paths sent to a Linux server must
    /// still resolve to `basename(cwd)`, not the full path string.
    #[tokio::test]
    async fn windows_backslash_path_resolves_to_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (_, proj_a) = resolve_project_ids(
            &state,
            Some(r"E:\source\engram"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        let (_, proj_b) = resolve_project_ids(
            &state,
            Some(r"C:\Users\dev\projects\engram"),
            None,
            None,
            ProjectStrategy::Basename,
            &engram_core::ActorKey::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            proj_a, proj_b,
            "different Windows paths with same basename must resolve to same project"
        );
        assert_ne!(
            proj_a, state.project_id,
            "Windows path must not fall back to the server-default project"
        );
    }
}
