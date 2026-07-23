//! Agent lifecycle hook plumbing for engram.
//!
//! Wire flow:
//!
//! 1. The agent CLI (Claude Code, Codex, OpenCode) emits a lifecycle event
//!    JSON over stdin to one of the vendored hook scripts under `hooks/`.
//! 2. The script `curl`s the JSON to `POST /hook?event=<kind>&agent=<kind>`
//!    on the running engram server with a sub-second timeout. The
//!    script exits 0 unconditionally so the agent never blocks on us
//!    (lesson from agentmemory #221 — hooks that `await` REST round-trips
//!    can deadlock the engine under fan-out).
//! 3. The server parses the body as JSON, runs it through the
//!    [`engram_core::Sanitizer`] redaction layer, and forwards a
//!    [`engram_core::Sanitized<NewObservation>`] to the store writer.
//!    On `SessionEnd` it also synthesises a wiki page summarising the
//!    session via [`synth`].
//!
//! Privacy strip is a *typed* boundary: there is no way to write an
//! observation without first passing through `Sanitized::new`.

pub mod log;
pub mod payload;
pub mod router;
pub mod synth;

// Re-export the sanitizer types from core so callers that grew up
// pointing at this crate's `sanitize` module keep working.
pub use engram_core::{SanitizeConfig, Sanitized, Sanitizer};
pub use payload::{HookEnvelope, HookEvent};
pub use router::{
    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, DEFAULT_PROJECT_CACHE_MAX_ENTRIES, HookState, ProjectCache,
    ProjectCacheStore, SubagentSessionSet, SubagentSessions, hook_router,
};
pub use synth::synthesize_session_page;
