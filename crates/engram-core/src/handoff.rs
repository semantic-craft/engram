//! Recoverable cross-agent task continuity domain.
//!
//! Identity is deliberately split across the protocol: a [`WorkItemId`] is a
//! stable unit of user work, [`SessionId`] identifies one agent Run/Session,
//! [`HandoffId`] identifies a revisioned transfer offer, [`ClaimId`] is the
//! opaque lease capability for one receiver, [`CheckpointId`] identifies one
//! append-only progress fact, and [`AttemptId`] makes retryable mutations
//! replay-safe. The Rust newtypes prevent accidental substitution.

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
///
/// `Acknowledged`, `Expired`, `Cancelled`, and `Superseded` are terminal and
/// immutable: a transfer that reached one of them stays readable as history and
/// is never revived. The four ways a transfer can end are distinct on purpose —
/// `Cancelled` is the source owner discarding its own offer, `Expired` is the
/// offer lapsing, `Superseded` is a successor replacing an unclaimed offer, and
/// a released Claim returns the transfer to `Open` rather than ending it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Open,
    Claimed,
    Acknowledged,
    Expired,
    Cancelled,
    Superseded,
}

impl HandoffState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Claimed => "claimed",
            Self::Acknowledged => "acknowledged",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
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
            "cancelled" => Ok(Self::Cancelled),
            "superseded" => Ok(Self::Superseded),
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

/// Publish input. `work_item_id = None` creates a new WorkItem; `Some`
/// publishes a successor transfer for that exact existing WorkItem.
///
/// A successor asserts the state it was constructed from:
/// `expected_work_item_revision` is required, and
/// `expected_checkpoint_revision` must equal the `work_item_revision` of the
/// WorkItem's latest Checkpoint (or be absent when it has none). Either being
/// stale is a conflict that mutates nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHandoff {
    pub work_item_id: Option<WorkItemId>,
    #[serde(default)]
    pub expected_work_item_revision: Option<u64>,
    #[serde(default)]
    pub expected_checkpoint_revision: Option<u64>,
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
    ///
    /// Defaulted on read: successful claim Attempts recorded before V104
    /// persist a serialized `Handoff` without this field, and an identical
    /// retry must still replay its exact recorded envelope.
    #[serde(default)]
    pub brief: String,
    /// Revisioned evidence locators published with this transfer. Defaulted
    /// on read for pre-V104 attempt envelopes, like `brief`.
    #[serde(default)]
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
    /// The transfer this one continues, when it is not the first.
    #[serde(default)]
    pub predecessor_handoff_id: Option<HandoffId>,
    /// Exact Checkpoint this successor was constructed from.
    #[serde(default)]
    pub source_checkpoint_id: Option<CheckpointId>,
    /// `work_item_revision` of [`Self::source_checkpoint_id`].
    #[serde(default)]
    pub source_checkpoint_revision: Option<u64>,
    /// Set on a still-unclaimed transfer that a successor replaced.
    #[serde(default)]
    pub superseded_by_handoff_id: Option<HandoffId>,
    #[serde(default)]
    pub superseded_at: Option<Timestamp>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

/// The durable progress fact a successor is published from.
///
/// Carries the acceptance-criterion status a receiver needs to see what is
/// still outstanding, plus the exact `work_item_revision` that
/// `NewHandoff::expected_checkpoint_revision` must assert. Large canonical
/// bodies stay in the wiki; this is the envelope, not the evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub work_item_id: WorkItemId,
    pub sequence: u64,
    pub work_item_revision: u64,
    pub work_item_state: WorkItemState,
    pub summary: String,
    pub acceptance_criteria: Vec<AcceptanceCriterionStatus>,
    pub actor_key: String,
    pub run_id: SessionId,
    pub handoff_id: Option<HandoffId>,
    pub created_at: Timestamp,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

/// One transfer in a WorkItem's ordered predecessor-to-successor chain.
///
/// Provenance keeps the dimensions apart: `source_actor` is the authenticated
/// publisher, `source_run_id` its Run, `from_agent` the execution agent,
/// `to_agent` the target selector, and the `receiving_*` fields the claimant
/// that took the transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffChainEntry {
    pub handoff_id: HandoffId,
    pub state: HandoffState,
    pub revision: u64,
    pub created_at: Timestamp,
    pub predecessor_handoff_id: Option<HandoffId>,
    pub superseded_by_handoff_id: Option<HandoffId>,
    pub source_checkpoint_id: Option<CheckpointId>,
    pub source_checkpoint_revision: Option<u64>,
    pub source_actor: String,
    pub source_run_id: SessionId,
    pub source_session_id: Option<SessionId>,
    pub from_agent: AgentKind,
    pub to_agent: Option<AgentKind>,
    pub receiving_actor: Option<String>,
    pub receiving_run_id: Option<SessionId>,
    pub receiving_claim_state: Option<String>,
    pub acknowledged_at: Option<Timestamp>,
}

/// Result of publishing a new transfer offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedHandoff {
    pub work_item_id: WorkItemId,
    pub handoff_id: HandoffId,
    pub work_item_revision: u64,
    pub handoff_revision: u64,
    #[serde(default)]
    pub predecessor_handoff_id: Option<HandoffId>,
    #[serde(default)]
    pub source_checkpoint_id: Option<CheckpointId>,
    #[serde(default)]
    pub source_checkpoint_revision: Option<u64>,
    /// Unclaimed transfers this successor atomically superseded.
    #[serde(default)]
    pub superseded_handoff_ids: Vec<HandoffId>,
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
    /// Caller-supplied context-assembly options, verbatim, as part of the
    /// Attempt identity.
    ///
    /// The claim returns an assembled ContextPackage, so budget, quotas and
    /// already-used refs decide what the caller receives. Without them in the
    /// digest, reusing one Attempt id with a different budget would replay the
    /// recorded claim and then assemble a *different* package — a changed
    /// request silently succeeding instead of being rejected. `Null` for
    /// callers that assemble nothing.
    #[serde(default)]
    pub context_options: serde_json::Value,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Successful claim Attempts recorded before V104 persist a serialized
    /// `Handoff` with no `brief` and no `context_refs`. An identical retry
    /// must still replay that exact recorded envelope rather than failing on
    /// a missing field.
    #[test]
    fn pre_v104_attempt_envelopes_still_deserialize() {
        let stored = serde_json::json!({
            "id": "01a062bf-14ab-7d73-9c43-ee3760232336",
            "work_item_id": "01a062bf-14ab-7d73-9c43-ee2fd747e292",
            "workspace_id": "01a062bf-148d-7430-8234-dda64f9d6a83",
            "project_id": "01a062bf-148d-7430-8234-ddb6dacfe75a",
            "from_session_id": null,
            "source_run_id": "019f0044-0000-7000-8000-000000000001",
            "from_agent": "other",
            "source_actor": "anonymous",
            "to_agent": null,
            "cwd": null,
            "summary": "recorded before the brief and context_refs columns",
            "open_questions": [],
            "next_steps": [],
            "files_touched": [],
            "state": "claimed",
            "revision": 2,
            "created_at": "2026-09-02T15:31:24.971519Z",
            "acknowledged_by": null,
            "acknowledged_at": null,
            "acknowledged_by_session": null
        });
        let handoff: Handoff = serde_json::from_value(stored).unwrap();
        assert!(handoff.brief.is_empty());
        assert!(handoff.context_refs.is_empty());
        assert!(handoff.artifacts.is_empty());
    }
}
