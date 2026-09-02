//! Explicit WorkItem relationships.
//!
//! Related work uses typed identities (`depends_on`, `derived_from`,
//! `child_of`) instead of inheriting another WorkItem's claim, blockers, or
//! other transient state. All three kinds are directed acyclic graphs.
//! A child may return structured evidence for its parent; it cannot complete,
//! abandon, claim, or supersede the parent.

#![allow(missing_docs)]

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::artifact::{ArtifactInput, ArtifactRef};
use crate::error::MemoryError;
use crate::ids::{ProjectId, SessionId, WorkItemId, WorkItemRelationshipId, WorkspaceId};

/// Directed relationship from one WorkItem to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemRelationshipKind {
    DependsOn,
    DerivedFrom,
    ChildOf,
}

impl WorkItemRelationshipKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DependsOn => "depends_on",
            Self::DerivedFrom => "derived_from",
            Self::ChildOf => "child_of",
        }
    }

    #[must_use]
    pub const fn requires_dag(self) -> bool {
        true
    }
}

impl std::str::FromStr for WorkItemRelationshipKind {
    type Err = MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "depends_on" => Ok(Self::DependsOn),
            "derived_from" => Ok(Self::DerivedFrom),
            "child_of" => Ok(Self::ChildOf),
            other => Err(MemoryError::MalformedRecord(format!(
                "unknown work item relationship kind: {other}"
            ))),
        }
    }
}

/// Parsed relationship to attach while publishing a WorkItem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationshipInput {
    pub kind: WorkItemRelationshipKind,
    pub target_work_item_id: WorkItemId,
    pub target_workspace_id: WorkspaceId,
    pub target_project_id: ProjectId,
}

/// Materialized relationship with stable identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemRelationship {
    pub id: WorkItemRelationshipId,
    pub kind: WorkItemRelationshipKind,
    pub from_work_item_id: WorkItemId,
    pub to_work_item_id: WorkItemId,
    pub from_workspace_id: WorkspaceId,
    pub from_project_id: ProjectId,
    pub to_workspace_id: WorkspaceId,
    pub to_project_id: ProjectId,
    pub created_by_run_id: SessionId,
    pub created_at: Timestamp,
}

/// Structured result a child WorkItem may append for its parent without
/// changing the parent's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParentResultInput {
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactInput>,
}

/// Stored parent-facing child result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentResult {
    pub child_work_item_id: WorkItemId,
    pub parent_work_item_id: WorkItemId,
    pub summary: String,
    pub artifacts: Vec<ArtifactRef>,
    pub created_at: Timestamp,
}
