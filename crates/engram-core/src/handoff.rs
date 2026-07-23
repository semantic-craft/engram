//! Cross-agent handoff type.
//!
//! A handoff is a typed snapshot of "where we are" — created when one
//! agent CLI ends a session, accepted when the next one starts in the
//! same project. Stored explicitly (vs. inferring from the
//! observations log) because cross-agent continuity is the project's
//! headline feature and deserves a first-class schema.

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::ids::{AgentKind, HandoffId, ProjectId, SessionId, WorkspaceId};

/// State machine of a single handoff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    /// Created, not yet picked up by the next agent.
    Open,
    /// Another agent has called `memory_handoff_accept` on it.
    Accepted,
    /// Aged out (decay sweep).
    Expired,
}

impl HandoffState {
    /// Canonical wire string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Expired => "expired",
        }
    }
}

impl std::str::FromStr for HandoffState {
    type Err = crate::MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "accepted" => Ok(Self::Accepted),
            "expired" => Ok(Self::Expired),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown handoff state: {other}"
            ))),
        }
    }
}

/// Input for inserting a new handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHandoff {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session this handoff captures (None for manual handoffs).
    pub from_session_id: Option<SessionId>,
    /// Agent CLI that produced this handoff.
    pub from_agent: AgentKind,
    /// Optional explicit target hint (`claude-code`, `codex`, …).
    pub to_agent: Option<AgentKind>,
    /// Working directory at handoff time. Used to match the next
    /// session's `memory_handoff_accept` call.
    pub cwd: Option<PathBuf>,
    /// One-paragraph summary of where we left off.
    pub summary: String,
    /// Open questions for the next agent.
    pub open_questions: Vec<String>,
    /// Suggested next steps.
    pub next_steps: Vec<String>,
    /// Files touched in the session.
    pub files_touched: Vec<String>,
}

/// Materialised view of a handoff row.
#[derive(Debug, Clone, Serialize)]
pub struct Handoff {
    /// Stable identifier.
    pub id: HandoffId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session that produced this handoff, if any.
    pub from_session_id: Option<SessionId>,
    /// Agent CLI that produced this handoff.
    pub from_agent: AgentKind,
    /// Optional target hint.
    pub to_agent: Option<AgentKind>,
    /// Working directory at handoff time.
    pub cwd: Option<String>,
    /// Summary.
    pub summary: String,
    /// Open questions.
    pub open_questions: Vec<String>,
    /// Next steps.
    pub next_steps: Vec<String>,
    /// Files touched.
    pub files_touched: Vec<String>,
    /// State.
    pub state: HandoffState,
    /// Creation timestamp.
    pub created_at: Timestamp,
    /// Agent CLI that accepted, if any.
    pub accepted_by: Option<AgentKind>,
    /// Acceptance timestamp.
    pub accepted_at: Option<Timestamp>,
    /// Session that accepted, if any.
    pub accepted_by_session: Option<SessionId>,
}
