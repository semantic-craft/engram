//! Recoverable cross-agent task continuity domain.
//!
//! Identity is deliberately split across the protocol: a [`WorkItemId`] is a
//! stable unit of user work, [`SessionId`] identifies one agent Run/Session,
//! [`HandoffId`] identifies a revisioned transfer offer, [`ClaimId`] is the
//! opaque lease capability for one receiver, [`CheckpointId`] identifies one
//! append-only progress fact, [`AttemptId`] makes retryable mutations
//! replay-safe, and [`BackgroundJobId`] remains reserved for asynchronous
//! server processing. The Rust newtypes prevent accidental substitution.

#![allow(missing_docs)]

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::AgentKind;
use crate::artifact::{ArtifactInput, ArtifactRef};
use crate::context::ContextRef;
use crate::ids::{
    AttemptId, CheckpointId, ClaimId, HandoffId, ProjectId, SessionId, WorkItemId, WorkspaceId,
};
use crate::relationship::{
    ParentResult, ParentResultInput, RelationshipInput, WorkItemRelationship,
};

/// State of the stable unit of user work. Receiving a handoff does not change
/// this state; only an explicit checkpoint can block, complete, or abandon it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemState {
    Active,
    Blocked,
    Completed,
    Abandoned,
}

impl WorkItemState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
        }
    }
}

impl std::str::FromStr for WorkItemState {
    type Err = crate::MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "blocked" => Ok(Self::Blocked),
            "completed" => Ok(Self::Completed),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown work item state: {other}"
            ))),
        }
    }
}

/// State of a transfer offer. `Acknowledged` means the claimant persisted its
/// first receiving checkpoint; it says nothing about WorkItem completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Open,
    Claimed,
    Acknowledged,
    Expired,
}

impl HandoffState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Claimed => "claimed",
            Self::Acknowledged => "acknowledged",
            Self::Expired => "expired",
        }
    }
}

impl std::str::FromStr for HandoffState {
    type Err = crate::MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "open" => Ok(Self::Open),
            "claimed" => Ok(Self::Claimed),
            "acknowledged" => Ok(Self::Acknowledged),
            "expired" => Ok(Self::Expired),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown handoff state: {other}"
            ))),
        }
    }
}

/// Explicit satisfaction status stored on every checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterionStatus {
    pub criterion: String,
    pub satisfied: bool,
}

/// Stable WorkItem materialized from operational SQLite state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: WorkItemId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub state: WorkItemState,
    pub revision: u64,
    pub owner_actor: String,
    pub owner_run_id: Option<SessionId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub relationships: Vec<WorkItemRelationship>,
    #[serde(default)]
    pub child_results: Vec<ParentResult>,
}

/// Publish input. `work_item_id = None` creates a new WorkItem; `Some` appends
/// a transfer offer to that exact existing WorkItem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHandoff {
    pub work_item_id: Option<WorkItemId>,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub from_session_id: Option<SessionId>,
    pub source_run_id: SessionId,
    pub from_agent: AgentKind,
    pub source_actor: String,
    pub to_agent: Option<AgentKind>,
    pub cwd: Option<PathBuf>,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub summary: String,
    /// Bounded continuation brief stored on the Handoff. Empty means the
    /// publisher omitted a distinct brief; persist `summary` instead. Never a
    /// copy of a referenced canonical page body.
    pub brief: String,
    /// Revisioned locators for canonical or derived evidence. Bodies stay in
    /// their source rows and are assembled at claim time.
    pub context_refs: Vec<ContextRef>,
    pub open_questions: Vec<String>,
    pub next_steps: Vec<String>,
    pub files_touched: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactInput>,
    #[serde(default)]
    pub relationships: Vec<RelationshipInput>,
}

/// Materialized transfer offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub id: HandoffId,
    pub work_item_id: WorkItemId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub from_session_id: Option<SessionId>,
    pub source_run_id: SessionId,
    pub from_agent: AgentKind,
    pub source_actor: String,
    pub to_agent: Option<AgentKind>,
    pub cwd: Option<String>,
    pub summary: String,
    /// Bounded continuation brief. Distinct from referenced canonical bodies.
    pub brief: String,
    /// Revisioned evidence locators published with this transfer.
    pub context_refs: Vec<ContextRef>,
    pub open_questions: Vec<String>,
    pub next_steps: Vec<String>,
    pub files_touched: Vec<String>,
    pub state: HandoffState,
    pub revision: u64,
    pub created_at: Timestamp,
    pub acknowledged_by: Option<String>,
    pub acknowledged_at: Option<Timestamp>,
    pub acknowledged_by_session: Option<SessionId>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

/// Result of publishing a new transfer offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedHandoff {
    pub work_item_id: WorkItemId,
    pub handoff_id: HandoffId,
    pub work_item_revision: u64,
    pub handoff_revision: u64,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub relationships: Vec<WorkItemRelationship>,
}

/// Claim outcome. The claim id is an opaque capability and must not be copied
/// into audit-log detail; it is persisted in attempt outcome state so an
/// identical lost-response retry can receive this exact envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffClaimResult {
    pub work_item_id: WorkItemId,
    pub handoff_id: HandoffId,
    pub claim_id: ClaimId,
    pub lease_expires_at: Timestamp,
    pub revision: u64,
    pub handoff: Handoff,
    #[serde(default)]
    pub relationships: Vec<WorkItemRelationship>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffClaim {
    pub handoff_id: HandoffId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub expected_revision: u64,
    pub run_id: SessionId,
    pub attempt_id: AttemptId,
    pub actor_key: String,
    pub lease_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffReleaseResult {
    pub work_item_id: WorkItemId,
    pub handoff_id: HandoffId,
    pub revision: u64,
    pub state: HandoffState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffRelease {
    pub handoff_id: HandoffId,
    pub claim_id: ClaimId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub expected_revision: u64,
    pub run_id: SessionId,
    pub attempt_id: AttemptId,
    pub actor_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffCancel {
    pub handoff_id: HandoffId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub expected_revision: u64,
    pub run_id: SessionId,
    pub actor_key: String,
}

/// Append-only checkpoint command after public input has been parsed and
/// sanitized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointWrite {
    pub work_item_id: WorkItemId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub run_id: SessionId,
    pub expected_work_item_revision: u64,
    pub handoff_id: Option<HandoffId>,
    pub claim_id: Option<ClaimId>,
    pub expected_handoff_revision: Option<u64>,
    pub summary: String,
    pub work_item_state: WorkItemState,
    pub acceptance_criteria: Vec<AcceptanceCriterionStatus>,
    pub actor_key: String,
    pub attempt_id: AttemptId,
    #[serde(default)]
    pub artifacts: Vec<ArtifactInput>,
    #[serde(default)]
    pub relationships: Vec<RelationshipInput>,
    #[serde(default)]
    pub parent_result: Option<ParentResultInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointWriteResult {
    pub checkpoint_id: CheckpointId,
    pub work_item_id: WorkItemId,
    pub sequence: u64,
    pub work_item_revision: u64,
    pub work_item_state: WorkItemState,
    pub handoff_id: Option<HandoffId>,
    pub handoff_revision: Option<u64>,
    pub handoff_state: Option<HandoffState>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub relationships: Vec<WorkItemRelationship>,
    #[serde(default)]
    pub parent_result: Option<ParentResult>,
}
