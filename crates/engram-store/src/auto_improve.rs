//! SQLite persistence for the auto-improvement loop.
//!
//! The consolidate crate reviews a finished session and produces *proposals*
//! (create/update a wiki page). This module stores those proposals with a
//! full audit trail so an operator can approve, reject, or fail them later:
//!
//! - `auto_improve_runs` — one row per review pass, with provider/model,
//!   warnings, and the rejected-candidate list for telemetry.
//! - `auto_improve_proposals` — staged page edits plus a snapshot of the
//!   target page at stage time (`*_at_stage` columns). Approval re-checks
//!   that snapshot and returns [`ApproveAutoImproveProposalResult::Conflict`]
//!   instead of clobbering a page that changed since staging.
//! - `auto_improve_proposal_events` — append-only status history per
//!   proposal (`staged` / `approved` / `rejected` / `failed` / `conflict`).
//! - `auto_improve_rejections` — normalized rejection records with a
//!   whitespace/case-insensitive fingerprint so the telemetry report can
//!   spot the same proposal being re-staged and re-rejected.
//! - `auto_improve_scheduler_state` / `_claims` — the session-end scheduler
//!   watermark and per-session claims that make the background reviewer
//!   idempotent across restarts.
//!
//! All mutations run inside a transaction on the caller's connection; the
//! writer actor owns that connection, so the single-writer invariant holds.

use std::str::FromStr;

use engram_core::{
    ActorContext, AutoImproveProposalId, AutoImproveRunId, NewPage, PageId, PagePath, ProjectId,
    SessionId, UserId, WorkspaceId,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::ops;

/// Lifecycle status of a staged proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoImproveProposalStatus {
    /// Staged and awaiting an operator decision.
    Pending,
    /// Approved. Wiki proposals are already applied; project-instruction
    /// proposals are only authorized/apply-ready and keep `applied_page_id` null.
    Approved,
    /// Rejected by an operator with a reason.
    Rejected,
    /// Approval was attempted but the target page changed since staging.
    Conflict,
    /// Application failed after approval (e.g. the page write errored).
    Failed,
}

impl AutoImproveProposalStatus {
    /// Snake-case string stored in the `status` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for AutoImproveProposalStatus {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "conflict" => Ok(Self::Conflict),
            "failed" => Ok(Self::Failed),
            other => Err(StoreError::MalformedRecord(format!(
                "unknown auto-improve proposal status: {other}"
            ))),
        }
    }
}

/// What a proposal wants to do to its Wiki or project-instruction target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoImproveProposalOperation {
    /// Create a page that must not exist yet at stage time.
    Create,
    /// Add new project-instruction content.
    Add,
    /// Rewrite a page that must already exist at stage time.
    Update,
    /// Remove instruction content proven stale, generic, or duplicated.
    StaleDelete,
    /// Relocate a procedure to an Agent Skill.
    MoveToSkill,
    /// Relocate component guidance to path-scoped instructions.
    MoveToPathRule,
    /// Relocate evidence or history to the Wiki.
    MoveToWiki,
    /// Reinforce a mandatory requirement in a technical control.
    MoveToEnforcement,
    /// Record that reviewed content should remain unchanged.
    NoChange,
}

impl AutoImproveProposalOperation {
    /// Stable snake-case spelling stored or exposed for review.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Add => "add",
            Self::Update => "update",
            Self::StaleDelete => "stale_delete",
            Self::MoveToSkill => "move_to_skill",
            Self::MoveToPathRule => "move_to_path_rule",
            Self::MoveToWiki => "move_to_wiki",
            Self::MoveToEnforcement => "move_to_enforcement",
            Self::NoChange => "no_change",
        }
    }
}

impl FromStr for AutoImproveProposalOperation {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create" => Ok(Self::Create),
            "add" => Ok(Self::Add),
            "update" => Ok(Self::Update),
            "stale_delete" => Ok(Self::StaleDelete),
            "move_to_skill" => Ok(Self::MoveToSkill),
            "move_to_path_rule" => Ok(Self::MoveToPathRule),
            "move_to_wiki" => Ok(Self::MoveToWiki),
            "move_to_enforcement" => Ok(Self::MoveToEnforcement),
            "no_change" => Ok(Self::NoChange),
            other => Err(StoreError::MalformedRecord(format!(
                "unknown auto-improve proposal operation: {other}"
            ))),
        }
    }
}

/// Mutation domain a pending proposal targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingProposalTargetKind {
    /// Existing auto-improvement proposal that mutates one Wiki page.
    WikiPage,
    /// Proposal-only project instruction change; never routed to Wiki apply.
    ProjectInstruction,
}

impl PendingProposalTargetKind {
    /// Stable database / structured-output spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WikiPage => "wiki_page",
            Self::ProjectInstruction => "project_instruction",
        }
    }
}

impl FromStr for PendingProposalTargetKind {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "wiki_page" => Ok(Self::WikiPage),
            "project_instruction" => Ok(Self::ProjectInstruction),
            other => Err(StoreError::MalformedRecord(format!(
                "unknown pending proposal target kind: {other}"
            ))),
        }
    }
}

/// One review pass to persist: run metadata plus its staged proposals.
#[derive(Debug, Clone)]
pub struct StageAutoImproveRun {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session the review covered, when session-scoped. Must belong to the
    /// same workspace/project or staging fails.
    pub session_id: Option<SessionId>,
    /// LLM provider name used for the review, for telemetry.
    pub provider: Option<String>,
    /// LLM model id used for the review, for telemetry.
    pub model: Option<String>,
    /// Reviewer's one-paragraph run summary.
    pub summary: Option<String>,
    /// Reviewer warnings (JSON array) surfaced in reports.
    pub warnings_json: serde_json::Value,
    /// Candidates the reviewer itself rejected (JSON array); each entry with
    /// a non-empty `reason` also lands in `auto_improve_rejections`.
    pub rejected_candidates_json: serde_json::Value,
    /// Effective review config snapshot for reproducibility.
    pub config_json: serde_json::Value,
    /// Actor attribution recorded on the run and its `staged` events.
    pub proposal_actor: ActorContext,
    /// Page edits to stage as pending proposals.
    pub proposals: Vec<NewAutoImproveProposal>,
}

/// One page edit to stage, before it gets an id or a status.
#[derive(Debug, Clone)]
pub struct NewAutoImproveProposal {
    /// Create a new page or update an existing one.
    pub operation: AutoImproveProposalOperation,
    /// Wiki path of the page this proposal targets.
    pub target_path: PagePath,
    /// Proposal category (e.g. `learning`, maintenance kinds) for telemetry.
    pub kind: String,
    /// Human-readable proposal title.
    pub title: String,
    /// Reviewer confidence in `0.0..=1.0`.
    pub confidence: f64,
    /// Why the reviewer proposes this edit.
    pub rationale: String,
    /// Supporting evidence (JSON) shown to the deciding operator.
    pub evidence_json: serde_json::Value,
    /// Full proposed page body (also the materialized result for patches).
    pub body_markdown: String,
    /// SHA-256 of the staged `_pending/` artifact file, when one was written.
    pub artifact_sha256: Option<[u8; 32]>,
    /// `full_page` (default when `None`) or `patch`.
    pub edit_mode: Option<String>,
    /// Patch operations (JSON) — required when `edit_mode == "patch"`.
    pub patch_json: Option<serde_json::Value>,
    /// SHA-256 of the base body the patch was materialized against —
    /// required for patches; staging fails if the live target differs.
    pub expected_base_body_sha256: Option<[u8; 32]>,
}

/// Ids assigned by [`stage_run`].
#[derive(Debug, Clone, Serialize)]
pub struct StagedAutoImproveRun {
    /// Id of the recorded run row.
    pub run_id: AutoImproveRunId,
    /// Ids of the staged proposals, in input order.
    pub proposal_ids: Vec<AutoImproveProposalId>,
    /// Proposals dropped by a per-proposal guard (target raced, or a
    /// pending proposal already holds that path). The rest of the run
    /// still lands; these are reported, not errors.
    pub skipped: Vec<SkippedAutoImproveProposal>,
}

/// One proposal the staging pass declined to record, with the reason.
#[derive(Debug, Clone, Serialize)]
pub struct SkippedAutoImproveProposal {
    /// Wiki path the dropped proposal targeted.
    pub target_path: String,
    /// Why it was dropped (operator-facing).
    pub reason: String,
}

/// List-view projection of a proposal (no bodies or event history).
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveProposalSummary {
    /// Proposal id.
    pub id: AutoImproveProposalId,
    /// Run that staged the proposal.
    pub run_id: AutoImproveRunId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Current lifecycle status.
    pub status: AutoImproveProposalStatus,
    /// Wiki page or project instruction target domain.
    pub target_kind: PendingProposalTargetKind,
    /// Target-specific create, update, relocation, deletion, or no-change action.
    pub operation: AutoImproveProposalOperation,
    /// Wiki path of the targeted page.
    pub target_path: PagePath,
    /// Logical target shown to reviewers (same as `target_path` for legacy rows).
    pub logical_target: String,
    /// Intended context layer (`wiki`, `root_instructions`, `agent_skill`, ...).
    pub target_context_layer: String,
    /// Proposal category for telemetry.
    pub kind: String,
    /// Human-readable proposal title.
    pub title: String,
    /// Reviewer confidence in `0.0..=1.0`.
    pub confidence: f64,
    /// Stage time (Unix microseconds).
    pub staged_at: i64,
    /// Decision time (Unix microseconds), once decided.
    pub decided_at: Option<i64>,
    /// Actor that staged the proposal, retained separately from the decider.
    #[serde(rename = "proposing_actor")]
    pub proposed_by_actor_json: serde_json::Value,
}

/// Full proposal record: summary plus bodies, stage-time target snapshot,
/// decision attribution, and the append-only event history.
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveProposalDetail {
    /// The list-view fields.
    pub summary: AutoImproveProposalSummary,
    /// Why the reviewer proposed this edit.
    pub rationale: String,
    /// Supporting evidence (JSON) shown to the deciding operator.
    pub evidence_json: serde_json::Value,
    /// Full proposed page body.
    pub body_markdown: String,
    /// Target-domain-neutral alias for `body_markdown` in structured review UX.
    pub proposed_content: String,
    /// SHA-256 of `body_markdown`, computed at stage time.
    pub body_sha256: [u8; 32],
    /// `_pending/auto-improve/<id>.md` artifact path for this proposal.
    pub artifact_path: String,
    /// SHA-256 of the staged artifact file, when one was written.
    pub artifact_sha256: Option<[u8; 32]>,
    /// Latest page id of the target at stage time (`None` for creates).
    pub target_latest_page_id_at_stage: Option<PageId>,
    /// Target body hash at stage time — approval compares against this to
    /// detect a page that changed after staging.
    pub target_body_sha256_at_stage: Option<[u8; 32]>,
    /// Target `updated_at` at stage time (Unix microseconds).
    pub target_updated_at_at_stage: Option<i64>,
    /// Operator-supplied reason for a reject/fail/conflict decision.
    pub decision_reason: Option<String>,
    /// Deciding user, when the decision came from an identified account.
    pub decided_by_author_id: Option<UserId>,
    /// Full actor context (JSON) recorded at decision time.
    pub decided_by_actor_json: Option<serde_json::Value>,
    /// Page written by an approval.
    pub applied_page_id: Option<PageId>,
    /// Wiki git checkpoint recorded alongside an approval, when available.
    pub checkpoint: Option<String>,
    /// `full_page` or `patch`.
    pub edit_mode: String,
    /// Patch operations (JSON) for `patch` proposals.
    pub patch_json: Option<serde_json::Value>,
    /// Base body hash the patch was generated against.
    pub expected_base_body_sha256: Option<[u8; 32]>,
    /// Base body hash the staged `body_markdown` was materialized from.
    pub materialized_base_body_sha256: Option<[u8; 32]>,
    /// SHA-256 of repository target content used to construct an instruction proposal.
    pub base_sha256: Option<String>,
    /// SHA-256 of the canonical local repository root captured at staging.
    pub repository_identity_sha256: Option<String>,
    /// Whether the repository target existed when its base bytes were captured.
    pub base_target_existed: Option<bool>,
    /// `exact_anchor` or `owned_region` for project-instruction proposals.
    pub boundary_kind: Option<String>,
    /// Stable anchor/region identifier approved for later application.
    pub boundary_value: Option<String>,
    /// Stage-time unified diff. Project proposals return this exact stored diff.
    pub unified_diff: Option<String>,
    /// Estimated context-token change (`ceil(after/4) - ceil(before/4)`).
    pub estimated_token_delta: Option<i64>,
    /// Complete source selection and evidence provenance.
    #[serde(rename = "provenance")]
    pub provenance_json: serde_json::Value,
    /// Repository target content captured when an instruction proposal was staged.
    pub base_content: Option<String>,
    /// Hash binding the currently reviewable instruction proposal fields.
    pub approval_sha256: Option<String>,
    /// Latest persisted human-review revision (`0` is the staged wording).
    pub review_revision: Option<i64>,
    /// Append-only wording revisions for project-instruction proposals.
    pub revisions: Vec<ProjectInstructionProposalRevision>,
    /// Immutable local apply record, once this instruction proposal was applied.
    pub application: Option<ProjectInstructionApplication>,
    /// Status history, oldest first.
    pub events: Vec<AutoImproveProposalEvent>,
}

/// One immutable wording revision of a project-instruction proposal.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectInstructionProposalRevision {
    /// Monotonic proposal-local revision (`0` is the staged wording).
    pub revision: i64,
    /// Full wording reviewed at this revision.
    pub proposed_content: String,
    /// SHA-256 of `proposed_content`.
    pub proposed_content_sha256: String,
    /// Unified diff against the immutable staged base content.
    pub unified_diff: String,
    /// Estimated token delta against the immutable staged base content.
    pub estimated_token_delta: i64,
    /// Hash binding all fields that approval authorizes.
    pub approval_sha256: String,
    /// Actor that staged or edited this revision.
    pub actor: serde_json::Value,
    /// Acting user, when identified.
    pub author_id: Option<UserId>,
    /// Revision time (Unix microseconds).
    pub at: i64,
}

/// One proposal-only project-instruction change to persist.
#[derive(Debug, Clone)]
pub struct StageProjectInstructionProposal {
    /// Existing workspace scope.
    pub workspace_id: WorkspaceId,
    /// Existing project scope.
    pub project_id: ProjectId,
    /// Instruction-specific operation (never `create`).
    pub operation: AutoImproveProposalOperation,
    /// Repository-relative logical target.
    pub logical_target: PagePath,
    /// SHA-256 of the canonical repository root that supplied the target.
    pub repository_identity_sha256: [u8; 32],
    /// Whether the repository target existed when the base bytes were captured.
    pub base_target_existed: bool,
    /// Destination context layer.
    pub target_context_layer: String,
    /// Stage-time SHA-256 of target content.
    pub base_sha256: [u8; 32],
    /// Stage-time target content retained so later human edits can be re-diffed.
    pub base_content: String,
    /// `exact_anchor` or `owned_region`.
    pub boundary_kind: String,
    /// Exact anchor or owned-region identifier.
    pub boundary_value: String,
    /// Complete proposed target content.
    pub proposed_content: String,
    /// Exact stage-time unified diff.
    pub unified_diff: String,
    /// Estimated context-token delta.
    pub estimated_token_delta: i64,
    /// Human-facing proposal title.
    pub title: String,
    /// Evidence-backed classification rationale.
    pub rationale: String,
    /// Full provenance array.
    pub provenance_json: serde_json::Value,
    /// Actor that selected and staged the proposal.
    pub proposing_actor: ActorContext,
    /// Stable proposing user id when authenticated.
    pub proposing_author_id: Option<UserId>,
}

/// Identity returned after staging one project-instruction proposal.
#[derive(Debug, Clone, Serialize)]
pub struct StagedProjectInstructionProposal {
    /// Persisted proposal id.
    pub proposal_id: AutoImproveProposalId,
}

/// Edit the wording of one pending project-instruction proposal.
#[derive(Debug, Clone)]
pub struct EditProjectInstructionProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to edit.
    pub proposal_id: AutoImproveProposalId,
    /// Approval hash the reviewer actually inspected.
    pub expected_approval_sha256: [u8; 32],
    /// Replacement full target content.
    pub proposed_content: String,
    /// Reviewing actor, separate from the proposing actor.
    pub actor: ActorContext,
    /// Reviewing user, when identified.
    pub author_id: Option<UserId>,
}

/// Result of editing a project-instruction proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditProjectInstructionProposalResult {
    /// A new immutable revision was appended.
    Updated {
        /// Newly assigned review revision.
        revision: i64,
        /// New approval binding hash.
        approval_sha256: [u8; 32],
        /// Recalculated token delta.
        estimated_token_delta: i64,
    },
    /// The requested wording already matches the current revision.
    Unchanged {
        /// Current review revision.
        revision: i64,
        /// Current approval binding hash.
        approval_sha256: [u8; 32],
        /// Current token delta.
        estimated_token_delta: i64,
    },
    /// The request was stale or the proposal had already reached a terminal state.
    Conflict {
        /// Stable operator-facing reason.
        reason: String,
    },
}

/// Approve one project-instruction proposal without applying its target.
#[derive(Debug, Clone)]
pub struct ApproveProjectInstructionProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to authorize for later local apply.
    pub proposal_id: AutoImproveProposalId,
    /// Approval hash the reviewer actually inspected.
    pub expected_approval_sha256: [u8; 32],
    /// Reviewing actor, separate from the proposing actor.
    pub actor: ActorContext,
    /// Reviewing user, when identified.
    pub author_id: Option<UserId>,
}

/// Result of DB-only project-instruction approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveProjectInstructionProposalResult {
    /// Transitioned from pending to approved/apply-ready.
    Approved {
        /// Approval binding hash authorized by the reviewer.
        approval_sha256: [u8; 32],
    },
    /// The same approval was already recorded; no audit event was duplicated.
    AlreadyApproved {
        /// Existing approval binding hash.
        approval_sha256: [u8; 32],
    },
    /// The request was stale or the proposal had another terminal state.
    Conflict {
        /// Stable operator-facing reason.
        reason: String,
    },
}

/// Filesystem result reported by the local project-instruction apply path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectInstructionApplyOutcome {
    /// The canonical instruction file did not exist and was created.
    Created,
    /// The existing canonical instruction file changed and was backed up.
    Updated,
    /// The approved content already matched; no write or backup occurred.
    NoOp,
}

impl ProjectInstructionApplyOutcome {
    /// Stable spelling stored in SQLite and exposed in JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::NoOp => "no_op",
        }
    }
}

impl FromStr for ProjectInstructionApplyOutcome {
    type Err = StoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "created" => Ok(Self::Created),
            "updated" => Ok(Self::Updated),
            "no_op" => Ok(Self::NoOp),
            other => Err(StoreError::MalformedRecord(format!(
                "unknown project-instruction apply outcome: {other}"
            ))),
        }
    }
}

/// Immutable audit record for one locally-applied project instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectInstructionApplication {
    /// Proposal whose approved content was applied.
    pub proposal_id: AutoImproveProposalId,
    /// Approval binding hash authorized by the human reviewer (lowercase hex).
    pub approval_sha256: String,
    /// Canonical target hash immediately before local application (lowercase hex).
    pub before_sha256: String,
    /// Canonical target hash after local application (lowercase hex).
    pub after_sha256: String,
    /// Whether the local file was created, updated, or already identical.
    pub outcome: ProjectInstructionApplyOutcome,
    /// Recoverable local backup path for an update.
    pub backup_path: Option<String>,
    /// Actor snapshot retained from proposal staging.
    pub proposing_actor: ActorContext,
    /// Actor snapshot retained from human approval.
    pub approving_actor: ActorContext,
    /// Actor snapshot supplied by the local applying host.
    pub applying_actor: ActorContext,
    /// Stable applying user id when authenticated.
    pub applied_by_author_id: Option<UserId>,
    /// Application time (Unix microseconds).
    pub applied_at: i64,
}

/// Record the result of a completed local project-instruction application.
#[derive(Debug, Clone)]
pub struct RecordProjectInstructionApplication {
    /// Owning workspace (exact scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (exact scope check).
    pub project_id: ProjectId,
    /// Approved project-instruction proposal that was applied.
    pub proposal_id: AutoImproveProposalId,
    /// Approval hash loaded and applied by the local host.
    pub expected_approval_sha256: [u8; 32],
    /// Canonical target hash immediately before local application.
    pub before_sha256: [u8; 32],
    /// Canonical target hash after local application.
    pub after_sha256: [u8; 32],
    /// Filesystem result observed by the local host.
    pub outcome: ProjectInstructionApplyOutcome,
    /// Recoverable backup path; required only for updates.
    pub backup_path: Option<String>,
    /// Local actor that applied the approved content.
    pub actor: ActorContext,
    /// Stable applying user id when authenticated.
    pub author_id: Option<UserId>,
}

/// Result of atomically recording one local instruction application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordProjectInstructionApplicationResult {
    /// A new immutable application and its single audit event were recorded.
    Recorded {
        /// Newly persisted application.
        application: ProjectInstructionApplication,
    },
    /// This proposal already had an application; the existing record is returned.
    AlreadyRecorded {
        /// Previously persisted application.
        application: ProjectInstructionApplication,
    },
}

/// One append-only status-history entry for a proposal.
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveProposalEvent {
    /// Autoincrement row id (orders events).
    pub id: i64,
    /// Proposal this event belongs to.
    pub proposal_id: AutoImproveProposalId,
    /// Event name: `staged`, `approved`, `rejected`, `failed`, or `conflict`.
    pub event: String,
    /// Actor context (JSON) that caused the event.
    pub actor_json: serde_json::Value,
    /// Acting user, when identified.
    pub author_id: Option<UserId>,
    /// Event-specific payload (e.g. `reason`, `applied_page_id`).
    pub detail_json: serde_json::Value,
    /// Event time (Unix microseconds).
    pub at: i64,
}

/// Normalized rejection record used by the telemetry report to spot the
/// same proposal being re-staged and re-rejected.
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveRejectionSummary {
    /// Rejection row id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Targeted wiki path, when the rejected candidate named one.
    pub target_path: Option<String>,
    /// Proposal category, when known.
    pub kind: Option<String>,
    /// Create vs update, when known.
    pub operation: Option<String>,
    /// `full_page` / `patch`, when known.
    pub edit_mode: Option<String>,
    /// Why the candidate/proposal was rejected.
    pub reason: String,
    /// Whitespace/case-insensitive SHA-256 over the identifying fields —
    /// equal fingerprints mean "the same rejection happened again".
    pub normalized_fingerprint: String,
    /// One-line description (title, summary, or the reason itself).
    pub summary: String,
    /// Original candidate/evidence payload (JSON).
    pub evidence_json: serde_json::Value,
    /// Run the rejection came from, when known.
    pub source_run_id: Option<AutoImproveRunId>,
    /// Proposal the rejection came from (`None` for reviewer-side rejects).
    pub source_proposal_id: Option<AutoImproveProposalId>,
    /// Record time (Unix microseconds).
    pub created_at: i64,
}

/// Aggregate telemetry for auto-improvement runs in one scope.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryAggregate {
    /// Number of auto-improve runs in the window.
    pub run_count: usize,
    /// Number of runs that staged at least one learning proposal.
    pub runs_with_learning_proposals: usize,
    /// Learning proposal counts by status.
    pub proposals_by_status: Vec<AutoImproveTelemetryCount>,
    /// Learning proposal counts by operation.
    pub proposals_by_operation: Vec<AutoImproveTelemetryCount>,
    /// Learning proposal counts by edit mode.
    pub proposals_by_edit_mode: Vec<AutoImproveTelemetryCount>,
    /// Learning proposal counts by kind.
    pub proposals_by_kind: Vec<AutoImproveTelemetryCount>,
    /// Maintenance/report proposal counts by kind.
    pub maintenance_proposals_by_kind: Vec<AutoImproveTelemetryCount>,
    /// Most frequently targeted learning pages.
    pub top_targets: Vec<AutoImproveTelemetryCount>,
    /// Rejection counts by reason.
    pub rejections_by_reason: Vec<AutoImproveTelemetryCount>,
    /// Repeated rejection fingerprints.
    pub repeated_rejection_fingerprints: Vec<AutoImproveTelemetryCount>,
    /// Most common rejected targets.
    pub rejected_targets: Vec<AutoImproveTelemetryCount>,
}

/// Generic `(key, count)` telemetry row.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryCount {
    /// Aggregated key; field-specific meaning depends on the source vector.
    pub key: String,
    /// Number of rows matching the key.
    pub count: usize,
}

/// Reject a pending proposal with a reason.
#[derive(Debug, Clone)]
pub struct RejectAutoImproveProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to reject; must currently be pending.
    pub proposal_id: AutoImproveProposalId,
    /// Operator-supplied rejection reason.
    pub reason: String,
    /// Actor attribution for the decision event.
    pub actor: ActorContext,
    /// Deciding user, when identified.
    pub author_id: Option<UserId>,
}

/// Mark a pending proposal failed (its application errored).
#[derive(Debug, Clone)]
pub struct FailAutoImproveProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to fail; must currently be pending.
    pub proposal_id: AutoImproveProposalId,
    /// What went wrong.
    pub reason: String,
    /// Actor attribution for the decision event.
    pub actor: ActorContext,
    /// Deciding user, when identified.
    pub author_id: Option<UserId>,
}

/// Approve a pending proposal and apply its page in the same transaction.
#[derive(Debug, Clone)]
pub struct ApproveAutoImproveProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to approve; must currently be pending.
    pub proposal_id: AutoImproveProposalId,
    /// Page to write on approval. Its scope, author, and path must match the
    /// proposal or approval fails before touching anything.
    pub page: NewPage,
    /// Actor attribution for the decision event.
    pub actor: ActorContext,
    /// Deciding user, when identified.
    pub author_id: Option<UserId>,
    /// Wiki git checkpoint to record alongside the approval.
    pub checkpoint: Option<String>,
}

/// Outcome of [`approve_proposal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveAutoImproveProposalResult {
    /// The page was written and the proposal marked approved.
    Approved {
        /// Id of the written page version.
        page_id: PageId,
    },
    /// The target changed since staging; the proposal was marked `conflict`
    /// and nothing was written.
    Conflict,
}

/// `_pending/auto-improve/<id>.md` — where a staged proposal's body artifact
/// lives in the wiki tree.
#[must_use]
pub fn artifact_path_for(proposal_id: AutoImproveProposalId) -> String {
    format!("_pending/auto-improve/{proposal_id}.md")
}

/// Build the deterministic unified diff used for project-instruction review.
#[must_use]
pub fn project_instruction_unified_diff(path: &str, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }

    const CONTEXT_LINES: usize = 3;
    let before_lines: Vec<_> = before.split_inclusive('\n').collect();
    let after_lines: Vec<_> = after.split_inclusive('\n').collect();
    let mut common_prefix = 0;
    while common_prefix < before_lines.len()
        && common_prefix < after_lines.len()
        && before_lines[common_prefix] == after_lines[common_prefix]
    {
        common_prefix += 1;
    }
    let mut common_suffix = 0;
    while common_suffix < before_lines.len().saturating_sub(common_prefix)
        && common_suffix < after_lines.len().saturating_sub(common_prefix)
        && before_lines[before_lines.len() - common_suffix - 1]
            == after_lines[after_lines.len() - common_suffix - 1]
    {
        common_suffix += 1;
    }

    let old_change_end = before_lines.len() - common_suffix;
    let new_change_end = after_lines.len() - common_suffix;
    let context_start = common_prefix.saturating_sub(CONTEXT_LINES);
    let old_context_end = (old_change_end + CONTEXT_LINES).min(before_lines.len());
    let new_context_end = (new_change_end + CONTEXT_LINES).min(after_lines.len());
    let before_count = old_context_end - context_start;
    let after_count = new_context_end - context_start;
    let before_start = if before_count == 0 {
        0
    } else {
        context_start + 1
    };
    let after_start = if after_count == 0 {
        0
    } else {
        context_start + 1
    };
    let mut output = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{before_start},{before_count} +{after_start},{after_count} @@\n"
    );
    append_diff_lines(
        &mut output,
        ' ',
        &before_lines[context_start..common_prefix],
    );
    append_diff_lines(
        &mut output,
        '-',
        &before_lines[common_prefix..old_change_end],
    );
    append_diff_lines(
        &mut output,
        '+',
        &after_lines[common_prefix..new_change_end],
    );
    append_diff_lines(
        &mut output,
        ' ',
        &before_lines[old_change_end..old_context_end],
    );
    output
}

fn append_diff_lines(output: &mut String, prefix: char, lines: &[&str]) {
    for line in lines {
        output.push(prefix);
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
            output.push_str("\\ No newline at end of file\n");
        }
    }
}

/// Estimate the context-token delta used by instruction proposals.
#[must_use]
pub fn project_instruction_token_delta(before: &str, after: &str) -> i64 {
    fn estimated_tokens(body: &str) -> i64 {
        i64::try_from(body.len().div_ceil(4)).unwrap_or(i64::MAX)
    }
    estimated_tokens(after) - estimated_tokens(before)
}

/// Hash every field whose meaning is authorized by human approval.
#[must_use]
pub fn project_instruction_approval_sha256(
    operation: AutoImproveProposalOperation,
    logical_target: &str,
    target_context_layer: &str,
    base_sha256: &[u8; 32],
    boundary_kind: &str,
    boundary_value: &str,
    proposed_content: &str,
) -> [u8; 32] {
    fn update_field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value);
    }

    let mut hasher = Sha256::new();
    hasher.update(b"engram-project-instruction-approval-v1");
    for field in [
        operation.as_str().as_bytes(),
        logical_target.as_bytes(),
        target_context_layer.as_bytes(),
        base_sha256.as_slice(),
        boundary_kind.as_bytes(),
        boundary_value.as_bytes(),
        proposed_content.as_bytes(),
    ] {
        update_field(&mut hasher, field);
    }
    hasher.finalize().into()
}

/// Ensure a scheduler-state row exists for the scope, seeding the watermark
/// at the newest already-ended session so pre-existing history is not
/// retroactively reviewed.
///
/// # Errors
/// Returns an error when the underlying SQLite statements fail.
pub fn ensure_scheduler_state(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let watermark_ended_at = conn.query_row(
        "SELECT COALESCE(MAX(ended_at), 0) FROM sessions \
         WHERE workspace_id = ?1 AND project_id = ?2 AND ended_at IS NOT NULL",
        params![workspace_id.as_bytes(), project_id.as_bytes()],
        |row| row.get::<_, i64>(0),
    )?;
    conn.execute(
        "INSERT INTO auto_improve_scheduler_state \
         (workspace_id, project_id, watermark_ended_at, initialized_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?4) \
         ON CONFLICT(workspace_id, project_id) DO NOTHING",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            watermark_ended_at,
            now,
        ],
    )?;
    Ok(())
}

/// Atomically claim one ended session for background review. Returns `true`
/// only for the first claimer: the insert requires the session to be past
/// the scope's watermark and not already covered by a run, so concurrent
/// schedulers and restarts cannot double-review a session.
///
/// # Errors
/// Returns an error when the underlying SQLite statements fail.
pub fn claim_scheduler_session(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    ended_at: i64,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO auto_improve_scheduler_claims \
         (workspace_id, project_id, session_id, claimed_at) \
         SELECT ?1, ?2, ?3, ?4 \
         WHERE EXISTS ( \
             SELECT 1 FROM auto_improve_scheduler_state st \
             JOIN sessions s \
               ON s.workspace_id = st.workspace_id \
              AND s.project_id = st.project_id \
             WHERE st.workspace_id = ?1 \
               AND st.project_id = ?2 \
               AND s.id = ?3 \
               AND s.ended_at = ?5 \
               AND s.ended_at > st.watermark_ended_at \
         ) \
           AND NOT EXISTS ( \
               SELECT 1 FROM auto_improve_runs r \
               WHERE r.workspace_id = ?1 \
                 AND r.project_id = ?2 \
                 AND r.session_id = ?3 \
           )",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            session_id.as_bytes(),
            now,
            ended_at,
        ],
    )?;
    if inserted == 1 {
        tx.execute(
            "UPDATE auto_improve_scheduler_state \
             SET updated_at = ?3 \
             WHERE workspace_id = ?1 AND project_id = ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes(), now],
        )?;
    }
    tx.commit()?;
    Ok(inserted == 1)
}

/// Persist one review run and stage its proposals as `pending`, all in one
/// transaction. Validates per-proposal preconditions (create targets must
/// not exist, update/patch targets must exist and — for patches — still
/// match the expected base hash) and snapshots the target page so approval
/// can detect later drift.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when a precondition fails, or an
/// SQLite error when a statement fails; either way nothing is committed.
pub fn stage_run(
    conn: &mut Connection,
    input: &StageAutoImproveRun,
) -> StoreResult<StagedAutoImproveRun> {
    let now = Timestamp::now().as_microsecond();
    let run_id = AutoImproveRunId::new();
    let actor_json = serde_json::to_string(&input.proposal_actor)?;
    let warnings_json = serde_json::to_string(&input.warnings_json)?;
    let rejected_json = serde_json::to_string(&input.rejected_candidates_json)?;
    let config_json = serde_json::to_string(&input.config_json)?;
    let tx = conn.transaction()?;
    if let Some(session_id) = input.session_id {
        tx.query_row(
            "SELECT 1 FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            params![
                session_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |_| Ok(()),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidState("auto-improve session is not in proposal scope".into())
        })?;
    }
    tx.execute(
        "INSERT INTO auto_improve_runs \
         (id, workspace_id, project_id, session_id, provider, model, summary, warnings_json, \
          rejected_candidates_json, config_json, proposal_actor_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            run_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            input.session_id.map(|id| id.as_bytes().to_vec()),
            input.provider.as_deref(),
            input.model.as_deref(),
            input.summary.as_deref(),
            warnings_json,
            rejected_json,
            config_json,
            actor_json,
            now,
        ],
    )?;
    insert_rejected_candidates_in_tx(&tx, input, run_id, now)?;
    let mut proposal_ids = Vec::with_capacity(input.proposals.len());
    let mut skipped: Vec<SkippedAutoImproveProposal> = Vec::new();
    for proposal in &input.proposals {
        // One pending proposal per target path is a schema invariant
        // (`idx_auto_improve_one_pending_target`). Under
        // `require_approval = true` the queue holds proposals for a long
        // time, so a later review proposing the same page is routine —
        // skip the newcomer and keep what the operator may already have
        // read, instead of failing the whole run on the UNIQUE index.
        let pending_exists = tx
            .query_row(
                "SELECT 1 FROM auto_improve_proposals \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND target_path = ?3 \
                   AND status = 'pending'",
                params![
                    input.workspace_id.as_bytes(),
                    input.project_id.as_bytes(),
                    proposal.target_path.as_str(),
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if pending_exists {
            skipped.push(SkippedAutoImproveProposal {
                target_path: proposal.target_path.as_str().to_owned(),
                reason: "a pending proposal already targets this path".to_owned(),
            });
            continue;
        }
        let id = AutoImproveProposalId::new();
        let artifact_path = artifact_path_for(id);
        let evidence_json = serde_json::to_string(&proposal.evidence_json)?;
        let body_sha256 = sha256(proposal.body_markdown.as_bytes());
        let edit_mode = proposal.edit_mode.as_deref().unwrap_or("full_page");
        let patch_json = proposal
            .patch_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let target_snapshot = latest_target_snapshot(
            &tx,
            input.workspace_id,
            input.project_id,
            proposal.target_path.as_str(),
        )?;
        let (
            target_latest_page_id_at_stage,
            target_body_sha256_at_stage,
            target_updated_at_at_stage,
        ) = match (proposal.operation, target_snapshot) {
            (AutoImproveProposalOperation::Create, None) => (None, None, None),
            (AutoImproveProposalOperation::Create, Some(_)) => {
                skipped.push(SkippedAutoImproveProposal {
                    target_path: proposal.target_path.as_str().to_owned(),
                    reason: "create proposal target already exists".to_owned(),
                });
                continue;
            }
            (AutoImproveProposalOperation::Update, Some(snapshot)) => (
                Some(snapshot.page_id),
                Some(bytes32(snapshot.body_sha256)?),
                Some(snapshot.updated_at),
            ),
            (AutoImproveProposalOperation::Update, None) => {
                skipped.push(SkippedAutoImproveProposal {
                    target_path: proposal.target_path.as_str().to_owned(),
                    reason: "update proposal target does not exist".to_owned(),
                });
                continue;
            }
            _ => {
                return Err(StoreError::InvalidState(
                    "Wiki proposals support only create or update operations".into(),
                ));
            }
        };
        if edit_mode == "patch" {
            // Patch-shape guards. Like the target guards above these are
            // per-proposal: a malformed or raced patch drops that proposal
            // and leaves the rest of the run intact.
            let patch_problem = if proposal.operation != AutoImproveProposalOperation::Update {
                Some("patch proposal must use update operation")
            } else if patch_json.is_none() {
                Some("patch proposal missing patch_json")
            } else {
                match (
                    proposal.expected_base_body_sha256,
                    target_body_sha256_at_stage,
                ) {
                    (None, _) => Some("patch proposal missing expected base body hash"),
                    (Some(expected), Some(current)) if current == expected => None,
                    (Some(_), Some(_)) => {
                        Some("proposal target changed since patch materialization")
                    }
                    (Some(_), None) => Some("patch proposal target does not exist"),
                }
            };
            if let Some(reason) = patch_problem {
                skipped.push(SkippedAutoImproveProposal {
                    target_path: proposal.target_path.as_str().to_owned(),
                    reason: reason.to_owned(),
                });
                continue;
            }
        }
        tx.execute(
            "INSERT INTO auto_improve_proposals \
             (id, run_id, workspace_id, project_id, status, operation, target_path, kind, title, \
              confidence, rationale, evidence_json, body_markdown, body_sha256, artifact_path, \
              artifact_sha256, target_latest_page_id_at_stage, target_body_sha256_at_stage, \
              target_updated_at_at_stage, staged_at, edit_mode, patch_json, \
              expected_base_body_sha256, materialized_base_body_sha256) \
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
                     ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
            params![
                id.as_bytes(),
                run_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
                proposal.operation.as_str(),
                proposal.target_path.as_str(),
                proposal.kind.as_str(),
                proposal.title.as_str(),
                proposal.confidence,
                proposal.rationale.as_str(),
                evidence_json,
                proposal.body_markdown.as_str(),
                body_sha256.as_slice(),
                artifact_path,
                proposal.artifact_sha256.map(|h| h.to_vec()),
                target_latest_page_id_at_stage.map(|id| id.as_bytes().to_vec()),
                target_body_sha256_at_stage.map(|h| h.to_vec()),
                target_updated_at_at_stage,
                now,
                edit_mode,
                patch_json,
                proposal.expected_base_body_sha256.map(|h| h.to_vec()),
                proposal.expected_base_body_sha256.map(|h| h.to_vec()),
            ],
        )?;
        insert_event_in_tx(
            &tx,
            id,
            "staged",
            &input.proposal_actor,
            None,
            &serde_json::json!({}),
            now,
        )?;
        proposal_ids.push(id);
    }
    tx.commit()?;
    Ok(StagedAutoImproveRun {
        run_id,
        skipped,
        proposal_ids,
    })
}

/// Stage one project-instruction proposal without writing a repository file,
/// Wiki page, or pending sidecar. The proposal and its initial audit event land
/// together in one transaction owned by the writer actor.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] for an invalid scope, operation,
/// boundary, context layer, missing provenance, or an already-pending target.
pub fn stage_project_instruction_proposal(
    conn: &mut Connection,
    input: &StageProjectInstructionProposal,
) -> StoreResult<StagedProjectInstructionProposal> {
    if input.operation == AutoImproveProposalOperation::Create {
        return Err(StoreError::InvalidState(
            "project-instruction proposals use add, not create".into(),
        ));
    }
    let valid_layer = match input.operation {
        AutoImproveProposalOperation::Add
        | AutoImproveProposalOperation::Update
        | AutoImproveProposalOperation::StaleDelete => matches!(
            input.target_context_layer.as_str(),
            "root_instructions" | "path_rules"
        ),
        AutoImproveProposalOperation::MoveToSkill => input.target_context_layer == "agent_skill",
        AutoImproveProposalOperation::MoveToPathRule => input.target_context_layer == "path_rules",
        AutoImproveProposalOperation::MoveToWiki => input.target_context_layer == "wiki",
        AutoImproveProposalOperation::MoveToEnforcement => {
            input.target_context_layer == "enforcement"
        }
        AutoImproveProposalOperation::NoChange => input.target_context_layer == "no_change",
        AutoImproveProposalOperation::Create => false,
    };
    if !valid_layer {
        return Err(StoreError::InvalidState(
            "project-instruction operation and target context layer do not match".into(),
        ));
    }
    if !matches!(
        input.boundary_kind.as_str(),
        "exact_anchor" | "owned_region"
    ) || input.boundary_value.trim().is_empty()
    {
        return Err(StoreError::InvalidState(
            "project-instruction proposal requires an exact anchor or owned region".into(),
        ));
    }
    if input.provenance_json.as_array().is_none_or(Vec::is_empty) {
        return Err(StoreError::InvalidState(
            "project-instruction proposal requires provenance".into(),
        ));
    }
    if sha256(input.base_content.as_bytes()) != input.base_sha256 {
        return Err(StoreError::InvalidState(
            "project-instruction base content hash does not match".into(),
        ));
    }
    if input.operation == AutoImproveProposalOperation::NoChange
        && input.base_content != input.proposed_content
    {
        return Err(StoreError::InvalidState(
            "no-change project-instruction proposals must preserve the exact base content".into(),
        ));
    }
    let expected_diff = project_instruction_unified_diff(
        input.logical_target.as_str(),
        &input.base_content,
        &input.proposed_content,
    );
    let expected_token_delta =
        project_instruction_token_delta(&input.base_content, &input.proposed_content);
    if input.unified_diff != expected_diff || input.estimated_token_delta != expected_token_delta {
        return Err(StoreError::InvalidState(
            "project-instruction derived review fields do not match content".into(),
        ));
    }

    let now = Timestamp::now().as_microsecond();
    let run_id = AutoImproveRunId::new();
    let proposal_id = AutoImproveProposalId::new();
    let actor_json = serde_json::to_string(&input.proposing_actor)?;
    let provenance_json = serde_json::to_string(&input.provenance_json)?;
    let body_sha256 = sha256(input.proposed_content.as_bytes());
    let approval_sha256 = project_instruction_approval_sha256(
        input.operation,
        input.logical_target.as_str(),
        &input.target_context_layer,
        &input.base_sha256,
        &input.boundary_kind,
        &input.boundary_value,
        &input.proposed_content,
    );
    let tx = conn.transaction()?;

    tx.query_row(
        "SELECT 1 FROM projects WHERE id = ?1 AND workspace_id = ?2",
        params![input.project_id.as_bytes(), input.workspace_id.as_bytes()],
        |_| Ok(()),
    )
    .optional()?
    .ok_or_else(|| {
        StoreError::InvalidState("project-instruction proposal scope does not exist".into())
    })?;

    let pending_exists = tx
        .query_row(
            "SELECT 1 FROM auto_improve_proposals \
             WHERE workspace_id = ?1 AND project_id = ?2 \
               AND target_path = ?3 AND status = 'pending'",
            params![
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
                input.logical_target.as_str(),
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if pending_exists {
        return Err(StoreError::InvalidState(
            "a pending proposal already targets this path".into(),
        ));
    }

    tx.execute(
        "INSERT INTO auto_improve_runs \
         (id, workspace_id, project_id, session_id, provider, model, summary, warnings_json, \
          rejected_candidates_json, config_json, proposal_actor_json, created_at) \
         VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, '[]', '[]', ?5, ?6, ?7)",
        params![
            run_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            input.title.as_str(),
            serde_json::json!({ "source": "instructions_propose" }).to_string(),
            actor_json,
            now,
        ],
    )?;

    tx.execute(
        "INSERT INTO auto_improve_proposals \
         (id, run_id, workspace_id, project_id, status, operation, target_path, kind, title, \
          confidence, rationale, evidence_json, body_markdown, body_sha256, artifact_path, \
          artifact_sha256, target_latest_page_id_at_stage, target_body_sha256_at_stage, \
          target_updated_at_at_stage, staged_at, edit_mode, patch_json, \
          expected_base_body_sha256, materialized_base_body_sha256, target_kind, \
          proposal_operation, logical_target, target_context_layer, base_sha256, \
          boundary_kind, boundary_value, unified_diff, estimated_token_delta, provenance_json, \
          base_content, approval_sha256, repository_identity_sha256, base_target_existed) \
         VALUES (?1, ?2, ?3, ?4, 'pending', 'update', ?5, 'project_instruction', ?6, 1.0, \
                 ?7, ?8, ?9, ?10, ?11, NULL, NULL, NULL, NULL, ?12, 'full_page', NULL, NULL, \
                 NULL, 'project_instruction', ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, \
                 ?22, ?23, ?24, ?25)",
        params![
            proposal_id.as_bytes(),
            run_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            input.logical_target.as_str(),
            input.title.as_str(),
            input.rationale.as_str(),
            provenance_json,
            input.proposed_content.as_str(),
            body_sha256.as_slice(),
            format!("db:project-instruction:{proposal_id}"),
            now,
            input.operation.as_str(),
            input.logical_target.as_str(),
            input.target_context_layer.as_str(),
            input.base_sha256.as_slice(),
            input.boundary_kind.as_str(),
            input.boundary_value.as_str(),
            input.unified_diff.as_str(),
            input.estimated_token_delta,
            serde_json::to_string(&input.provenance_json)?,
            input.base_content.as_str(),
            approval_sha256.as_slice(),
            input.repository_identity_sha256.as_slice(),
            input.base_target_existed,
        ],
    )?;
    tx.execute(
        "INSERT INTO project_instruction_proposal_revisions \
         (proposal_id, revision, proposed_content, proposed_content_sha256, unified_diff, \
          estimated_token_delta, approval_sha256, actor_json, author_id, at) \
         VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            proposal_id.as_bytes(),
            input.proposed_content.as_str(),
            body_sha256.as_slice(),
            input.unified_diff.as_str(),
            input.estimated_token_delta,
            approval_sha256.as_slice(),
            serde_json::to_string(&input.proposing_actor)?,
            input.proposing_author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    insert_event_in_tx(
        &tx,
        proposal_id,
        "staged",
        &input.proposing_actor,
        input.proposing_author_id,
        &serde_json::json!({
            "target_kind": "project_instruction",
            "operation": input.operation.as_str(),
            "logical_target": input.logical_target.as_str(),
            "review_revision": 0,
            "approval_sha256": hex_bytes(&approval_sha256),
            "repository_identity_sha256": hex_bytes(&input.repository_identity_sha256),
            "base_target_existed": input.base_target_existed,
        }),
        now,
    )?;
    tx.commit()?;
    Ok(StagedProjectInstructionProposal { proposal_id })
}

/// Replace the wording of one pending project-instruction proposal and append
/// an immutable revision plus audit event. Repository and Wiki targets are not
/// consulted or mutated.
///
/// # Errors
/// Returns a store error when persisted proposal metadata is malformed or the
/// SQLite transaction fails. Stale and terminal requests return a typed
/// [`EditProjectInstructionProposalResult::Conflict`].
pub fn edit_project_instruction_proposal(
    conn: &mut Connection,
    input: &EditProjectInstructionProposal,
) -> StoreResult<EditProjectInstructionProposalResult> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let proposal = tx
        .query_row(
            "SELECT status, target_kind, proposal_operation, logical_target, \
                    target_context_layer, base_sha256, boundary_kind, boundary_value, \
                    base_content, approval_sha256, body_markdown, estimated_token_delta \
             FROM auto_improve_proposals \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            params![
                input.proposal_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                ))
            },
        )
        .optional()?;
    let Some((
        status,
        target_kind,
        operation,
        logical_target,
        target_context_layer,
        base_sha256,
        boundary_kind,
        boundary_value,
        base_content,
        approval_sha256,
        current_content,
        current_token_delta,
    )) = proposal
    else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal not found in scope".into(),
        });
    };
    if target_kind != PendingProposalTargetKind::ProjectInstruction.as_str() {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "Wiki proposals do not support instruction wording edits".into(),
        });
    }
    if status != AutoImproveProposalStatus::Pending.as_str() {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: format!("project-instruction proposal is terminal: {status}"),
        });
    }
    let Some(operation) = operation else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(logical_target) = logical_target else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(target_context_layer) = target_context_layer else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(base_sha256) = base_sha256 else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(boundary_kind) = boundary_kind else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(boundary_value) = boundary_value else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(base_content) = base_content else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let Some(approval_sha256) = approval_sha256 else {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let base_sha256 = bytes32(base_sha256)?;
    let approval_sha256 = bytes32(approval_sha256)?;
    if approval_sha256 != input.expected_approval_sha256 {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "stale project-instruction review hash".into(),
        });
    }
    let revision: i64 = tx.query_row(
        "SELECT COALESCE(MAX(revision), -1) \
         FROM project_instruction_proposal_revisions WHERE proposal_id = ?1",
        params![input.proposal_id.as_bytes()],
        |row| row.get(0),
    )?;
    if revision < 0 {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    }
    let operation = AutoImproveProposalOperation::from_str(&operation)?;
    if operation == AutoImproveProposalOperation::NoChange && input.proposed_content != base_content
    {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "no-change project-instruction proposals must preserve the exact base content"
                .into(),
        });
    }
    if current_content == input.proposed_content {
        return Ok(EditProjectInstructionProposalResult::Unchanged {
            revision,
            approval_sha256,
            estimated_token_delta: current_token_delta.unwrap_or_default(),
        });
    }

    let unified_diff =
        project_instruction_unified_diff(&logical_target, &base_content, &input.proposed_content);
    let estimated_token_delta =
        project_instruction_token_delta(&base_content, &input.proposed_content);
    let new_approval_sha256 = project_instruction_approval_sha256(
        operation,
        &logical_target,
        &target_context_layer,
        &base_sha256,
        &boundary_kind,
        &boundary_value,
        &input.proposed_content,
    );
    let proposed_content_sha256 = sha256(input.proposed_content.as_bytes());
    let next_revision = revision + 1;
    let changed = tx.execute(
        "UPDATE auto_improve_proposals \
         SET body_markdown = ?1, body_sha256 = ?2, unified_diff = ?3, \
             estimated_token_delta = ?4, approval_sha256 = ?5 \
         WHERE id = ?6 AND workspace_id = ?7 AND project_id = ?8 \
           AND status = 'pending' AND approval_sha256 = ?9",
        params![
            input.proposed_content.as_str(),
            proposed_content_sha256.as_slice(),
            unified_diff.as_str(),
            estimated_token_delta,
            new_approval_sha256.as_slice(),
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            approval_sha256.as_slice(),
        ],
    )?;
    if changed != 1 {
        return Ok(EditProjectInstructionProposalResult::Conflict {
            reason: "stale project-instruction review hash".into(),
        });
    }
    tx.execute(
        "INSERT INTO project_instruction_proposal_revisions \
         (proposal_id, revision, proposed_content, proposed_content_sha256, unified_diff, \
          estimated_token_delta, approval_sha256, actor_json, author_id, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            input.proposal_id.as_bytes(),
            next_revision,
            input.proposed_content.as_str(),
            proposed_content_sha256.as_slice(),
            unified_diff.as_str(),
            estimated_token_delta,
            new_approval_sha256.as_slice(),
            serde_json::to_string(&input.actor)?,
            input.author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "edited",
        &input.actor,
        input.author_id,
        &serde_json::json!({
            "review_revision": next_revision,
            "previous_approval_sha256": hex_bytes(&approval_sha256),
            "approval_sha256": hex_bytes(&new_approval_sha256),
            "estimated_token_delta": estimated_token_delta,
        }),
        now,
    )?;
    tx.commit()?;
    Ok(EditProjectInstructionProposalResult::Updated {
        revision: next_revision,
        approval_sha256: new_approval_sha256,
        estimated_token_delta,
    })
}

/// Mark a project-instruction proposal approved/apply-ready without touching
/// repository files, Wiki files, Wiki index rows, or Git state.
///
/// # Errors
/// Returns a store error when persisted metadata is malformed or the SQLite
/// transaction fails. Stale and terminal requests return a typed conflict.
pub fn approve_project_instruction_proposal(
    conn: &mut Connection,
    input: &ApproveProjectInstructionProposal,
) -> StoreResult<ApproveProjectInstructionProposalResult> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let proposal = tx
        .query_row(
            "SELECT status, target_kind, approval_sha256 \
             FROM auto_improve_proposals \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            params![
                input.proposal_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((status, target_kind, approval_sha256)) = proposal else {
        return Ok(ApproveProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal not found in scope".into(),
        });
    };
    if target_kind != PendingProposalTargetKind::ProjectInstruction.as_str() {
        return Ok(ApproveProjectInstructionProposalResult::Conflict {
            reason: "Wiki proposals must use the Wiki approval path".into(),
        });
    }
    let Some(approval_sha256) = approval_sha256 else {
        return Ok(ApproveProjectInstructionProposalResult::Conflict {
            reason: "project-instruction proposal predates editable review metadata".into(),
        });
    };
    let approval_sha256 = bytes32(approval_sha256)?;
    if approval_sha256 != input.expected_approval_sha256 {
        return Ok(ApproveProjectInstructionProposalResult::Conflict {
            reason: "stale project-instruction review hash".into(),
        });
    }
    if status == AutoImproveProposalStatus::Approved.as_str() {
        return Ok(ApproveProjectInstructionProposalResult::AlreadyApproved { approval_sha256 });
    }
    if status != AutoImproveProposalStatus::Pending.as_str() {
        return Ok(ApproveProjectInstructionProposalResult::Conflict {
            reason: format!("project-instruction proposal is terminal: {status}"),
        });
    }

    let actor_json = serde_json::to_string(&input.actor)?;
    let changed = tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = 'approved', decided_at = ?1, decision_reason = NULL, \
             decided_by_author_id = ?2, decided_by_actor_json = ?3, \
             applied_page_id = NULL, checkpoint = NULL \
         WHERE id = ?4 AND workspace_id = ?5 AND project_id = ?6 \
           AND status = 'pending' AND target_kind = 'project_instruction' \
           AND approval_sha256 = ?7",
        params![
            now,
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            approval_sha256.as_slice(),
        ],
    )?;
    if changed != 1 {
        return Ok(ApproveProjectInstructionProposalResult::Conflict {
            reason: "stale project-instruction review hash".into(),
        });
    }
    let review_revision: i64 = tx.query_row(
        "SELECT COALESCE(MAX(revision), 0) \
         FROM project_instruction_proposal_revisions WHERE proposal_id = ?1",
        params![input.proposal_id.as_bytes()],
        |row| row.get(0),
    )?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "approved",
        &input.actor,
        input.author_id,
        &serde_json::json!({
            "apply_ready": true,
            "review_revision": review_revision,
            "approval_sha256": hex_bytes(&approval_sha256),
        }),
        now,
    )?;
    tx.commit()?;
    Ok(ApproveProjectInstructionProposalResult::Approved { approval_sha256 })
}

/// Record the result of applying one approved project-instruction proposal on
/// the local host. This transaction does not mutate repository files or the
/// proposal lifecycle status: it atomically inserts the immutable application
/// snapshot and exactly one `applied` audit event.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the proposal is missing from the
/// exact scope, is not an approved project-instruction proposal, its approval
/// or content hashes do not match, or the filesystem outcome is inconsistent.
pub fn record_project_instruction_application(
    conn: &mut Connection,
    input: &RecordProjectInstructionApplication,
) -> StoreResult<RecordProjectInstructionApplicationResult> {
    let tx = conn.transaction()?;
    let proposal = tx
        .query_row(
            "SELECT p.status, p.target_kind, p.proposal_operation, p.base_target_existed, \
                    p.approval_sha256, p.base_sha256, p.body_markdown, p.base_content, \
                    r.proposal_actor_json, p.decided_by_actor_json \
             FROM auto_improve_proposals p \
             JOIN auto_improve_runs r ON r.id = p.run_id \
             WHERE p.id = ?1 AND p.workspace_id = ?2 AND p.project_id = ?3",
            params![
                input.proposal_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<bool>>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )
        .optional()?;
    let Some((
        status,
        target_kind,
        operation,
        base_target_existed,
        approval_sha256,
        base_sha256,
        proposed_content,
        base_content,
        proposing_actor_json,
        approving_actor_json,
    )) = proposal
    else {
        return Err(StoreError::InvalidState(
            "project-instruction proposal not found in scope".into(),
        ));
    };
    if target_kind != PendingProposalTargetKind::ProjectInstruction.as_str() {
        return Err(StoreError::InvalidState(
            "Wiki proposals cannot use the local instruction apply path".into(),
        ));
    }
    if status != AutoImproveProposalStatus::Approved.as_str() {
        return Err(StoreError::InvalidState(format!(
            "project-instruction proposal is not apply-ready: {status}"
        )));
    }
    let operation = operation
        .ok_or_else(|| {
            StoreError::InvalidState("project-instruction proposal has no typed operation".into())
        })?
        .parse::<AutoImproveProposalOperation>()?;
    if !matches!(
        operation,
        AutoImproveProposalOperation::Add
            | AutoImproveProposalOperation::Update
            | AutoImproveProposalOperation::StaleDelete
            | AutoImproveProposalOperation::NoChange
    ) {
        return Err(StoreError::InvalidState(format!(
            "{} is not supported by the single-target local apply path",
            operation.as_str()
        )));
    }
    let base_target_existed = base_target_existed.ok_or_else(|| {
        StoreError::InvalidState(
            "project-instruction proposal predates target-existence metadata".into(),
        )
    })?;
    if operation == AutoImproveProposalOperation::NoChange
        && input.outcome != ProjectInstructionApplyOutcome::NoOp
    {
        return Err(StoreError::InvalidState(
            "no-change project-instruction proposals can only record a no-op".into(),
        ));
    }
    let approval_sha256 = approval_sha256.map(bytes32).transpose()?.ok_or_else(|| {
        StoreError::InvalidState("project-instruction proposal predates approval metadata".into())
    })?;
    if approval_sha256 != input.expected_approval_sha256 {
        return Err(StoreError::InvalidState(
            "stale project-instruction review hash".into(),
        ));
    }

    if let Some(application) = tx
        .query_row(
            "SELECT proposal_id, approval_sha256, before_sha256, after_sha256, outcome, \
                    backup_path, proposing_actor_json, approving_actor_json, \
                    applying_actor_json, applied_by_author_id, applied_at \
             FROM project_instruction_applications WHERE proposal_id = ?1",
            params![input.proposal_id.as_bytes()],
            project_instruction_application_from_row,
        )
        .optional()?
    {
        let matches_existing = application.approval_sha256 == hex_bytes(&approval_sha256)
            && application.before_sha256 == hex_bytes(&input.before_sha256)
            && application.after_sha256 == hex_bytes(&input.after_sha256)
            && application.outcome == input.outcome
            && application.backup_path == input.backup_path;
        if !matches_existing {
            return Err(StoreError::InvalidState(
                "project-instruction proposal already has a different application record".into(),
            ));
        }
        return Ok(RecordProjectInstructionApplicationResult::AlreadyRecorded { application });
    }

    let base_sha256 = base_sha256.map(bytes32).transpose()?.ok_or_else(|| {
        StoreError::InvalidState("project-instruction proposal predates base hash metadata".into())
    })?;
    if input.before_sha256 != base_sha256 {
        return Err(StoreError::InvalidState(
            "project-instruction target no longer matches the approved base".into(),
        ));
    }
    if input.after_sha256 != sha256(proposed_content.as_bytes()) {
        return Err(StoreError::InvalidState(
            "project-instruction applied content does not match the approved proposal".into(),
        ));
    }
    match input.outcome {
        ProjectInstructionApplyOutcome::Created => {
            if base_target_existed
                || base_content.as_deref() != Some("")
                || input.before_sha256 != sha256(b"")
                || input.before_sha256 == input.after_sha256
                || input.backup_path.is_some()
            {
                return Err(StoreError::InvalidState(
                    "created application requires an empty base, changed content, and no backup"
                        .into(),
                ));
            }
        }
        ProjectInstructionApplyOutcome::Updated => {
            if !base_target_existed
                || input.before_sha256 == input.after_sha256
                || input.backup_path.as_deref().is_none_or(str::is_empty)
            {
                return Err(StoreError::InvalidState(
                    "updated application requires changed content and a backup path".into(),
                ));
            }
        }
        ProjectInstructionApplyOutcome::NoOp => {
            if input.before_sha256 != input.after_sha256 || input.backup_path.is_some() {
                return Err(StoreError::InvalidState(
                    "no-op application requires identical hashes and no backup".into(),
                ));
            }
        }
    }

    let proposing_actor: ActorContext = serde_json::from_str(&proposing_actor_json)?;
    let approving_actor_json = approving_actor_json.ok_or_else(|| {
        StoreError::InvalidState("approved project-instruction proposal has no approver".into())
    })?;
    let approving_actor: ActorContext = serde_json::from_str(&approving_actor_json)?;
    let applying_actor_json = serde_json::to_string(&input.actor)?;
    let now = Timestamp::now().as_microsecond();
    tx.execute(
        "INSERT INTO project_instruction_applications \
         (proposal_id, approval_sha256, before_sha256, after_sha256, outcome, backup_path, \
          proposing_actor_json, approving_actor_json, applying_actor_json, \
          applied_by_author_id, applied_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            input.proposal_id.as_bytes(),
            approval_sha256.as_slice(),
            input.before_sha256.as_slice(),
            input.after_sha256.as_slice(),
            input.outcome.as_str(),
            input.backup_path.as_deref(),
            proposing_actor_json,
            approving_actor_json,
            applying_actor_json,
            input.author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "applied",
        &input.actor,
        input.author_id,
        &serde_json::json!({
            "approval_sha256": hex_bytes(&approval_sha256),
            "before_sha256": hex_bytes(&input.before_sha256),
            "after_sha256": hex_bytes(&input.after_sha256),
            "outcome": input.outcome.as_str(),
            "backup_path": input.backup_path,
        }),
        now,
    )?;
    tx.commit()?;

    let application = ProjectInstructionApplication {
        proposal_id: input.proposal_id,
        approval_sha256: hex_bytes(&approval_sha256),
        before_sha256: hex_bytes(&input.before_sha256),
        after_sha256: hex_bytes(&input.after_sha256),
        outcome: input.outcome,
        backup_path: input.backup_path.clone(),
        proposing_actor,
        approving_actor,
        applying_actor: input.actor.clone(),
        applied_by_author_id: input.author_id,
        applied_at: now,
    };
    Ok(RecordProjectInstructionApplicationResult::Recorded { application })
}

/// Mark a pending proposal `failed`, record the rejection fingerprint, and
/// append a `failed` event.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the proposal is not pending in
/// the given scope, or an SQLite error when a statement fails.
pub fn fail_proposal(conn: &mut Connection, input: &FailAutoImproveProposal) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let actor_json = serde_json::to_string(&input.actor)?;
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = 'failed', decided_at = ?1, decision_reason = ?2, \
             decided_by_author_id = ?3, decided_by_actor_json = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 AND status = 'pending'",
        params![
            now,
            input.reason.as_str(),
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidState(
            "auto-improve proposal is not pending or not in scope".into(),
        ));
    }
    insert_rejection_for_proposal_in_tx(&tx, input.proposal_id, &input.reason, now)?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "failed",
        &input.actor,
        input.author_id,
        &serde_json::json!({ "reason": input.reason.as_str() }),
        now,
    )?;
    tx.commit()?;
    Ok(())
}

/// Mark a pending proposal `rejected`, record the rejection fingerprint, and
/// append a `rejected` event.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the proposal is not pending in
/// the given scope, or an SQLite error when a statement fails.
pub fn reject_proposal(
    conn: &mut Connection,
    input: &RejectAutoImproveProposal,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let actor_json = serde_json::to_string(&input.actor)?;
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = 'rejected', decided_at = ?1, decision_reason = ?2, \
             decided_by_author_id = ?3, decided_by_actor_json = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 AND status = 'pending'",
        params![
            now,
            input.reason.as_str(),
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidState(
            "auto-improve proposal is not pending or not in scope".into(),
        ));
    }
    insert_rejection_for_proposal_in_tx(&tx, input.proposal_id, &input.reason, now)?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "rejected",
        &input.actor,
        input.author_id,
        &serde_json::json!({ "reason": input.reason.as_str() }),
        now,
    )?;
    tx.commit()?;
    Ok(())
}

/// Approve a pending proposal: re-check the stage-time target snapshot and,
/// when it still matches, write the page and mark the proposal `approved`
/// in the same transaction. A drifted target marks the proposal `conflict`
/// instead and writes nothing.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the approval page's scope,
/// author, or path disagrees with the proposal, or when the proposal is not
/// pending in the given scope; or an SQLite error when a statement fails.
pub fn approve_proposal(
    conn: &mut Connection,
    input: &ApproveAutoImproveProposal,
) -> StoreResult<ApproveAutoImproveProposalResult> {
    if input.page.workspace_id != input.workspace_id || input.page.project_id != input.project_id {
        return Err(StoreError::InvalidState(
            "approval page scope does not match proposal scope".into(),
        ));
    }
    if input.page.author_id != input.author_id {
        return Err(StoreError::InvalidState(
            "approval page author does not match approver author".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let proposal = tx
        .query_row(
            "SELECT operation, target_path, target_latest_page_id_at_stage, \
                    target_body_sha256_at_stage, target_updated_at_at_stage, target_kind \
             FROM auto_improve_proposals \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND status = 'pending'",
            params![
                input.proposal_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        operation,
        target_path,
        staged_page_id,
        staged_body_hash,
        staged_updated_at,
        target_kind,
    )) = proposal
    else {
        return Err(StoreError::InvalidState(
            "auto-improve proposal is not pending or not in scope".into(),
        ));
    };
    if target_kind != PendingProposalTargetKind::WikiPage.as_str() {
        return Err(StoreError::InvalidState(
            "project-instruction proposals cannot use the Wiki approval path".into(),
        ));
    }
    if input.page.path.as_str() != target_path {
        return Err(StoreError::InvalidState(
            "approval page path does not match proposal target".into(),
        ));
    }

    let current = latest_target_snapshot(&tx, input.workspace_id, input.project_id, &target_path)?;
    // Hard enforcement of the documented safety invariant: pinned pages are
    // never rewritten by the auto-improvement path (issue #157). The check
    // lives HERE — the single point every apply flows through, manual
    // approval and require_approval=false auto-apply alike — so no index
    // window, prompt phrasing, or approval policy can bypass it. Unpinning
    // the page first is the explicit way to allow the rewrite.
    if current.as_ref().is_some_and(|snapshot| {
        pinned_refusal_applies(&target_path, snapshot.pinned, &snapshot.frontmatter_json)
    }) {
        const REASON: &str =
            "target page is pinned; pinned pages are never rewritten by auto-improvement";
        insert_rejection_for_proposal_in_tx(&tx, input.proposal_id, REASON, now)?;
        mark_decision_in_tx(&tx, input, "conflict", None, Some(REASON), now)?;
        insert_event_in_tx(
            &tx,
            input.proposal_id,
            "conflict",
            &input.actor,
            input.author_id,
            &serde_json::json!({ "reason": REASON }),
            now,
        )?;
        tx.commit()?;
        return Ok(ApproveAutoImproveProposalResult::Conflict);
    }
    let conflict = match AutoImproveProposalOperation::from_str(&operation)? {
        AutoImproveProposalOperation::Create => current.is_some(),
        AutoImproveProposalOperation::Update => match current {
            Some(snapshot) => {
                Some(snapshot.page_id.as_bytes().to_vec()) != staged_page_id
                    || Some(snapshot.body_sha256) != staged_body_hash
                    || Some(snapshot.updated_at) != staged_updated_at
            }
            None => true,
        },
        _ => {
            return Err(StoreError::InvalidState(
                "project-instruction proposals cannot use the Wiki approval path".into(),
            ));
        }
    };
    if conflict {
        insert_rejection_for_proposal_in_tx(
            &tx,
            input.proposal_id,
            "target changed since proposal was staged",
            now,
        )?;
        mark_decision_in_tx(
            &tx,
            input,
            "conflict",
            None,
            Some("target changed since proposal was staged"),
            now,
        )?;
        insert_event_in_tx(
            &tx,
            input.proposal_id,
            "conflict",
            &input.actor,
            input.author_id,
            &serde_json::json!({ "reason": "target changed since proposal was staged" }),
            now,
        )?;
        tx.commit()?;
        return Ok(ApproveAutoImproveProposalResult::Conflict);
    }

    let page_id = ops::upsert_page_in_tx(&tx, &input.page, now)?;
    mark_decision_in_tx(&tx, input, "approved", Some(page_id), None, now)?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "approved",
        &input.actor,
        input.author_id,
        &serde_json::json!({ "applied_page_id": page_id.to_string() }),
        now,
    )?;
    tx.commit()?;
    Ok(ApproveAutoImproveProposalResult::Approved { page_id })
}

fn mark_decision_in_tx(
    tx: &rusqlite::Transaction<'_>,
    input: &ApproveAutoImproveProposal,
    status: &str,
    applied_page_id: Option<PageId>,
    reason: Option<&str>,
    now: i64,
) -> StoreResult<()> {
    let actor_json = serde_json::to_string(&input.actor)?;
    tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = ?1, decided_at = ?2, decision_reason = ?3, decided_by_author_id = ?4, \
             decided_by_actor_json = ?5, applied_page_id = ?6, checkpoint = ?7 \
         WHERE id = ?8 AND workspace_id = ?9 AND project_id = ?10 AND status = 'pending'",
        params![
            status,
            now,
            reason,
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            applied_page_id.map(|id| id.as_bytes().to_vec()),
            input.checkpoint.as_deref(),
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
        ],
    )?;
    Ok(())
}

/// The latest version of a proposal's target page at decision time.
struct TargetSnapshot {
    page_id: PageId,
    body_sha256: Vec<u8>,
    updated_at: i64,
    pinned: bool,
    frontmatter_json: String,
}

fn latest_target_snapshot(
    tx: &rusqlite::Transaction<'_>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    target_path: &str,
) -> StoreResult<Option<TargetSnapshot>> {
    let row = tx
        .query_row(
            "SELECT id, body_sha256, updated_at, pinned, frontmatter_json FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![workspace_id.as_bytes(), project_id.as_bytes(), target_path],
            |row| {
                Ok(TargetSnapshot {
                    page_id: PageId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?,
                    body_sha256: row.get::<_, Vec<u8>>(1)?,
                    updated_at: row.get::<_, i64>(2)?,
                    pinned: row.get::<_, bool>(3)?,
                    frontmatter_json: row.get::<_, String>(4)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Whether the pinned-target refusal applies (issue #157). Pinned pages
/// are never rewritten by auto-improvement — with one sanctioned
/// exception: NON-invariant memory slots under `_slots/` (e.g.
/// `current-focus`, a state slot) are always pinned by the slot regime
/// and are exactly the pages auto-improvement is SUPPOSED to refresh.
/// Slots whose frontmatter declares `slot_kind: "invariant"` stay
/// protected, matching the documented safety invariant ("never rewrite
/// pinned pages or invariant slots").
fn pinned_refusal_applies(target_path: &str, pinned: bool, frontmatter_json: &str) -> bool {
    if !pinned {
        return false;
    }
    if !target_path.starts_with("_slots/") {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(frontmatter_json)
        .ok()
        .and_then(|fm| {
            fm.get("slot_kind")
                .and_then(serde_json::Value::as_str)
                .map(|kind| kind.eq_ignore_ascii_case("invariant"))
        })
        .unwrap_or(false)
}

fn insert_event_in_tx(
    tx: &rusqlite::Transaction<'_>,
    proposal_id: AutoImproveProposalId,
    event: &str,
    actor: &ActorContext,
    author_id: Option<UserId>,
    detail: &serde_json::Value,
    at: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO auto_improve_proposal_events \
         (proposal_id, event, actor_json, author_id, detail_json, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            proposal_id.as_bytes(),
            event,
            serde_json::to_string(actor)?,
            author_id.map(|id| id.as_bytes().to_vec()),
            serde_json::to_string(detail)?,
            at,
        ],
    )?;
    Ok(())
}

fn insert_rejected_candidates_in_tx(
    tx: &rusqlite::Transaction<'_>,
    input: &StageAutoImproveRun,
    run_id: AutoImproveRunId,
    now: i64,
) -> StoreResult<()> {
    let Some(candidates) = input.rejected_candidates_json.as_array() else {
        return Ok(());
    };
    for candidate in candidates {
        let reason = candidate
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if reason.is_empty() {
            continue;
        }
        let target_path = string_field(candidate, "target_path")
            .or_else(|| string_field(candidate, "path"))
            .or_else(|| {
                string_field(candidate, "evidence").filter(|value| PagePath::new(value).is_ok())
            });
        let summary = string_field(candidate, "summary")
            .or_else(|| string_field(candidate, "evidence"))
            .unwrap_or_else(|| reason.to_string());
        let record = NewAutoImproveRejectionRecord {
            workspace_id: input.workspace_id,
            project_id: input.project_id,
            target_path,
            kind: string_field(candidate, "kind"),
            operation: string_field(candidate, "operation"),
            edit_mode: string_field(candidate, "edit_mode"),
            reason: reason.to_string(),
            summary,
            evidence_json: candidate.clone(),
            source_run_id: Some(run_id),
            source_proposal_id: None,
        };
        insert_rejection_record_in_tx(tx, &record, now)?;
    }
    Ok(())
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

struct NewAutoImproveRejectionRecord {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    target_path: Option<String>,
    kind: Option<String>,
    operation: Option<String>,
    edit_mode: Option<String>,
    reason: String,
    summary: String,
    evidence_json: serde_json::Value,
    source_run_id: Option<AutoImproveRunId>,
    source_proposal_id: Option<AutoImproveProposalId>,
}

fn insert_rejection_for_proposal_in_tx(
    tx: &rusqlite::Transaction<'_>,
    proposal_id: AutoImproveProposalId,
    reason: &str,
    now: i64,
) -> StoreResult<()> {
    let record = tx.query_row(
        "SELECT run_id, workspace_id, project_id, COALESCE(proposal_operation, operation), \
                COALESCE(logical_target, target_path), kind, title, rationale, evidence_json, \
                edit_mode \
         FROM auto_improve_proposals WHERE id = ?1",
        params![proposal_id.as_bytes()],
        |row| {
            let run_id =
                AutoImproveRunId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?;
            let workspace_id =
                WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(1)?).map_err(to_sql_err)?;
            let project_id =
                ProjectId::from_slice(&row.get::<_, Vec<u8>>(2)?).map_err(to_sql_err)?;
            let evidence_raw: String = row.get(8)?;
            let title: String = row.get(6)?;
            let rationale: String = row.get(7)?;
            Ok(NewAutoImproveRejectionRecord {
                workspace_id,
                project_id,
                target_path: Some(row.get(4)?),
                kind: Some(row.get(5)?),
                operation: Some(row.get(3)?),
                edit_mode: Some(row.get(9)?),
                reason: reason.to_string(),
                summary: if title.trim().is_empty() {
                    rationale
                } else {
                    title
                },
                evidence_json: serde_json::from_str(&evidence_raw).map_err(to_sql_err)?,
                source_run_id: Some(run_id),
                source_proposal_id: Some(proposal_id),
            })
        },
    )?;
    insert_rejection_record_in_tx(tx, &record, now)
}

fn insert_rejection_record_in_tx(
    tx: &rusqlite::Transaction<'_>,
    record: &NewAutoImproveRejectionRecord,
    now: i64,
) -> StoreResult<()> {
    let id = Uuid::new_v4();
    let evidence_json = serde_json::to_string(&record.evidence_json)?;
    let fingerprint = rejection_fingerprint(record);
    tx.execute(
        "INSERT INTO auto_improve_rejections \
         (id, workspace_id, project_id, target_path, kind, operation, edit_mode, reason, \
          normalized_fingerprint, summary, evidence_json, source_run_id, source_proposal_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id.as_bytes().as_slice(),
            record.workspace_id.as_bytes(),
            record.project_id.as_bytes(),
            record.target_path.as_deref(),
            record.kind.as_deref(),
            record.operation.as_deref(),
            record.edit_mode.as_deref(),
            record.reason.as_str(),
            fingerprint,
            record.summary.as_str(),
            evidence_json,
            record.source_run_id.map(|id| id.as_bytes().to_vec()),
            record.source_proposal_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    Ok(())
}

fn rejection_fingerprint(record: &NewAutoImproveRejectionRecord) -> String {
    let input = [
        normalize_fp(record.target_path.as_deref().unwrap_or("")),
        normalize_fp(record.kind.as_deref().unwrap_or("")),
        normalize_fp(record.operation.as_deref().unwrap_or("")),
        normalize_fp(record.edit_mode.as_deref().unwrap_or("")),
        normalize_fp(&record.reason),
        normalize_fp(&record.summary),
    ]
    .join("\n");
    hex_sha256(input.as_bytes())
}

fn normalize_fp(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hex_bytes(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(crate) fn project_instruction_application_from_row(
    row: &Row<'_>,
) -> rusqlite::Result<ProjectInstructionApplication> {
    let proposal_id =
        AutoImproveProposalId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?;
    let approval_sha256 = bytes32(row.get(1)?).map_err(to_sql_err)?;
    let before_sha256 = bytes32(row.get(2)?).map_err(to_sql_err)?;
    let after_sha256 = bytes32(row.get(3)?).map_err(to_sql_err)?;
    let outcome =
        ProjectInstructionApplyOutcome::from_str(&row.get::<_, String>(4)?).map_err(to_sql_err)?;
    let proposing_actor = serde_json::from_str(&row.get::<_, String>(6)?).map_err(to_sql_err)?;
    let approving_actor = serde_json::from_str(&row.get::<_, String>(7)?).map_err(to_sql_err)?;
    let applying_actor = serde_json::from_str(&row.get::<_, String>(8)?).map_err(to_sql_err)?;
    let applied_by_author_id = row
        .get::<_, Option<Vec<u8>>>(9)?
        .map(|bytes| UserId::from_slice(&bytes))
        .transpose()
        .map_err(to_sql_err)?;
    Ok(ProjectInstructionApplication {
        proposal_id,
        approval_sha256: hex_bytes(&approval_sha256),
        before_sha256: hex_bytes(&before_sha256),
        after_sha256: hex_bytes(&after_sha256),
        outcome,
        backup_path: row.get(5)?,
        proposing_actor,
        approving_actor,
        applying_actor,
        applied_by_author_id,
        applied_at: row.get(10)?,
    })
}

pub(crate) fn summary_from_row(row: &Row<'_>) -> rusqlite::Result<AutoImproveProposalSummary> {
    let status: String = row.get(4)?;
    let operation: String = row.get(5)?;
    let id = AutoImproveProposalId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?;
    let run_id = AutoImproveRunId::from_slice(&row.get::<_, Vec<u8>>(1)?).map_err(to_sql_err)?;
    let workspace_id = WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(2)?).map_err(to_sql_err)?;
    let project_id = ProjectId::from_slice(&row.get::<_, Vec<u8>>(3)?).map_err(to_sql_err)?;
    let target_path = PagePath::new(row.get::<_, String>(6)?).map_err(to_sql_err)?;
    Ok(AutoImproveProposalSummary {
        id,
        run_id,
        workspace_id,
        project_id,
        status: AutoImproveProposalStatus::from_str(&status).map_err(to_sql_err)?,
        target_kind: PendingProposalTargetKind::WikiPage,
        operation: AutoImproveProposalOperation::from_str(&operation).map_err(to_sql_err)?,
        logical_target: target_path.as_str().to_owned(),
        target_context_layer: "wiki".to_owned(),
        target_path,
        kind: row.get(7)?,
        title: row.get(8)?,
        confidence: row.get(9)?,
        staged_at: row.get(10)?,
        decided_at: row.get(11)?,
        proposed_by_actor_json: serde_json::json!({}),
    })
}

pub(crate) fn summary_from_row_with_metadata(
    row: &Row<'_>,
    offset: usize,
) -> rusqlite::Result<AutoImproveProposalSummary> {
    let mut summary = summary_from_row(row)?;
    let target_kind: String = row.get(offset)?;
    let proposal_operation: Option<String> = row.get(offset + 1)?;
    let logical_target: Option<String> = row.get(offset + 2)?;
    let target_context_layer: Option<String> = row.get(offset + 3)?;
    let proposed_actor_raw: String = row.get(offset + 4)?;
    summary.target_kind = PendingProposalTargetKind::from_str(&target_kind).map_err(to_sql_err)?;
    if let Some(operation) = proposal_operation {
        summary.operation =
            AutoImproveProposalOperation::from_str(&operation).map_err(to_sql_err)?;
    }
    summary.logical_target = logical_target.unwrap_or_else(|| summary.target_path.to_string());
    summary.target_context_layer = target_context_layer.unwrap_or_else(|| "wiki".to_owned());
    summary.proposed_by_actor_json =
        serde_json::from_str(&proposed_actor_raw).map_err(to_sql_err)?;
    Ok(summary)
}

pub(crate) fn bytes32(bytes: Vec<u8>) -> StoreResult<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| StoreError::MalformedRecord("invalid sha256 length".into()))
}

pub(crate) fn opt_bytes32(bytes: Option<Vec<u8>>) -> StoreResult<Option<[u8; 32]>> {
    bytes.map(bytes32).transpose()
}

pub(crate) fn to_sql_err<E: std::error::Error + Send + Sync + 'static>(err: E) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #157: pinned pages are refused; the sanctioned exception is
    // NON-invariant `_slots/` pages (the slot regime pins everything, and
    // state slots like current-focus are exactly what auto-improvement is
    // supposed to refresh). Invariant slots stay protected.
    #[test]
    fn pinned_refusal_spares_state_slots_but_not_invariant_slots() {
        // Regular pinned page: refused.
        assert!(pinned_refusal_applies("decisions/adr-0001.md", true, "{}"));
        // Unpinned: never refused.
        assert!(!pinned_refusal_applies(
            "decisions/adr-0001.md",
            false,
            "{}"
        ));
        // State slot (default kind): allowed.
        assert!(!pinned_refusal_applies(
            "_slots/current-focus.md",
            true,
            "{}"
        ));
        assert!(!pinned_refusal_applies(
            "_slots/current-focus.md",
            true,
            r#"{"slot_kind": "state"}"#
        ));
        // Invariant slot: refused.
        assert!(pinned_refusal_applies(
            "_slots/never-force-push.md",
            true,
            r#"{"slot_kind": "invariant"}"#
        ));
        // Malformed frontmatter on a slot: treated as state (allowed) —
        // the slot regime owns those pages either way.
        assert!(!pinned_refusal_applies("_slots/x.md", true, "not-json"));
    }
}
