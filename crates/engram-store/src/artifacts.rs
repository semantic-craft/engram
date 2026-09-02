//! Persistence helpers for typed ArtifactRefs and WorkItem relationships.

use engram_core::{
    ArtifactId, ArtifactInput, ArtifactRef, DeliveryFacts, NormalizedArtifact, ParentResult,
    ParentResultInput, ProjectId, RelationshipInput, SessionId, VerificationEvidence, WorkItemId,
    WorkItemRelationship, WorkItemRelationshipId, WorkItemRelationshipKind, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};

pub(crate) const CHILD_MUTATION_FORBIDDEN: &str =
    "child WorkItem cannot complete, abandon, claim, or supersede its parent";
pub(crate) const RELATIONSHIP_UNAUTHORIZED: &str =
    "unauthorized actor cannot create work item relationship";
pub(crate) const SECOND_PARENT_FORBIDDEN: &str =
    "work item already has a child_of parent; a child cannot have two parents";

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_artifacts(
    tx: &rusqlite::Transaction<'_>,
    owner_kind: &str,
    owner_id: &[u8; 16],
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    source_run_id: SessionId,
    observed_at: i64,
    inputs: &[ArtifactInput],
) -> StoreResult<Vec<ArtifactRef>> {
    let mut refs = Vec::with_capacity(inputs.len());
    for input in inputs {
        refs.push(persist_one_artifact(
            tx,
            owner_kind,
            owner_id,
            workspace_id,
            project_id,
            source_run_id,
            observed_at,
            input,
        )?);
    }
    Ok(refs)
}

#[allow(clippy::too_many_arguments)]
fn persist_one_artifact(
    tx: &rusqlite::Transaction<'_>,
    owner_kind: &str,
    owner_id: &[u8; 16],
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    source_run_id: SessionId,
    observed_at: i64,
    input: &ArtifactInput,
) -> StoreResult<ArtifactRef> {
    let normalized = input.normalized().map_err(StoreError::from)?;
    let identity_key = normalized.identity_key_for_scope(project_id);
    let identity_hash = Sha256::digest(identity_key.as_bytes());
    let identity_hash: [u8; 32] = identity_hash.into();
    let artifact_id = artifact_id_from_hash(&identity_hash);

    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM artifacts WHERE identity_hash = ?1",
            params![identity_hash.as_slice()],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(existing_id) = existing {
        if existing_id.as_slice() != artifact_id.as_bytes() {
            return Err(StoreError::MalformedRecord(
                "artifact identity hash collided with a different id".into(),
            ));
        }
        reject_content_hash_conflict(tx, artifact_id, normalized.content_hash.as_deref())?;
    } else {
        tx.execute(
            "INSERT INTO artifacts \
                 (id, identity_hash, kind, locator, observed_revision, \
                  repository_identity, commit_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                artifact_id.as_bytes(),
                identity_hash.as_slice(),
                normalized.kind.as_str(),
                normalized.locator,
                normalized.observed_revision,
                normalized.repository_identity,
                normalized.commit_id,
            ],
        )?;
    }

    let facts = &normalized.delivery;
    tx.execute(
        "INSERT OR IGNORE INTO artifact_attachments \
         (artifact_id, owner_kind, owner_id, source_run_id, observed_at, provenance, \
          content_hash, git_ref, tree_hash, dirty, local_path_hint, workspace_id, project_id, \
          fact_changed, fact_verified, fact_committed, fact_pushed, \
          fact_reviewed, fact_merged, fact_released, fact_deployed, fact_submitted, \
          fact_approved) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                 ?18, ?19, ?20, ?21, ?22, ?23)",
        params![
            artifact_id.as_bytes(),
            owner_kind,
            owner_id.as_slice(),
            source_run_id.as_bytes(),
            observed_at,
            normalized.provenance,
            normalized.content_hash,
            normalized.git_ref,
            normalized.tree_hash,
            normalized.dirty.map(i64::from),
            normalized.local_path_hint,
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            i64::from(facts.changed),
            i64::from(facts.verified),
            i64::from(facts.committed),
            i64::from(facts.pushed),
            i64::from(facts.reviewed),
            i64::from(facts.merged),
            i64::from(facts.released),
            i64::from(facts.deployed),
            i64::from(facts.submitted),
            i64::from(facts.approved),
        ],
    )?;
    let attachment_id: i64 = tx.query_row(
        "SELECT id FROM artifact_attachments \
         WHERE owner_kind = ?1 AND owner_id = ?2 AND artifact_id = ?3",
        params![owner_kind, owner_id.as_slice(), artifact_id.as_bytes()],
        |row| row.get(0),
    )?;
    persist_verification(tx, attachment_id, source_run_id, observed_at, &normalized)?;

    load_attachment(tx, attachment_id)?.ok_or_else(|| {
        StoreError::MalformedRecord("artifact attachment missing after insert".into())
    })
}

fn persist_verification(
    tx: &rusqlite::Transaction<'_>,
    attachment_id: i64,
    source_run_id: SessionId,
    observed_at: i64,
    normalized: &NormalizedArtifact,
) -> StoreResult<()> {
    for evidence in &normalized.verification {
        let applies = evidence
            .applies_to_revision
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| normalized.observed_revision.clone())
            .unwrap_or_default();
        let id = Uuid::now_v7();
        tx.execute(
            "INSERT INTO artifact_verification \
             (id, attachment_id, check_name, observed_result, observed_at, source_run_id, \
              applies_to_revision) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.as_bytes().as_slice(),
                attachment_id,
                evidence.check,
                evidence.result,
                observed_at,
                source_run_id.as_bytes(),
                applies,
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn load_owner_artifacts(
    conn: &Connection,
    owner_kind: &str,
    owner_id: &[u8],
) -> StoreResult<Vec<ArtifactRef>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM artifact_attachments \
         WHERE owner_kind = ?1 AND owner_id = ?2 ORDER BY id",
    )?;
    let ids = stmt.query_map(params![owner_kind, owner_id], |row| row.get::<_, i64>(0))?;
    let mut artifacts = Vec::new();
    for id in ids {
        if let Some(artifact) = load_attachment(conn, id?)? {
            artifacts.push(artifact);
        }
    }
    Ok(artifacts)
}

fn load_attachment(conn: &Connection, attachment_id: i64) -> StoreResult<Option<ArtifactRef>> {
    let row = conn
        .query_row(
            "SELECT a.id, a.kind, a.locator, a.observed_revision, t.content_hash, \
                    a.repository_identity, t.git_ref, a.commit_id, t.tree_hash, t.dirty, \
                    t.local_path_hint, t.provenance, t.source_run_id, t.observed_at, \
                    t.fact_changed, t.fact_verified, t.fact_committed, t.fact_pushed, \
                    t.fact_reviewed, t.fact_merged, t.fact_released, t.fact_deployed, \
                    t.fact_submitted, t.fact_approved \
             FROM artifact_attachments t \
             JOIN artifacts a ON a.id = t.artifact_id \
             WHERE t.id = ?1",
            params![attachment_id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Vec<u8>>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, i64>(18)?,
                    row.get::<_, i64>(19)?,
                    row.get::<_, i64>(20)?,
                    row.get::<_, i64>(21)?,
                    row.get::<_, i64>(22)?,
                    row.get::<_, i64>(23)?,
                ))
            },
        )
        .optional()?;
    let Some((
        id,
        kind,
        locator,
        observed_revision,
        content_hash,
        repository_identity,
        git_ref,
        commit_id,
        tree_hash,
        dirty,
        local_path_hint,
        provenance,
        source_run,
        observed_at,
        changed,
        verified,
        committed,
        pushed,
        reviewed,
        merged,
        released,
        deployed,
        submitted,
        approved,
    )) = row
    else {
        return Ok(None);
    };
    let verification = load_verification(conn, attachment_id, observed_revision.as_deref())?;
    Ok(Some(ArtifactRef {
        id: ArtifactId::from_slice(&id)?,
        kind: kind.parse()?,
        locator,
        observed_revision,
        content_hash,
        repository_identity,
        git_ref,
        commit_id,
        tree_hash,
        dirty: dirty.map(|value| value != 0),
        local_path_hint,
        provenance,
        source_run_id: SessionId::from_slice(&source_run)?,
        observed_at: jiff::Timestamp::from_microsecond(observed_at)
            .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
        delivery: DeliveryFacts {
            changed: changed != 0,
            verified: verified != 0,
            committed: committed != 0,
            pushed: pushed != 0,
            reviewed: reviewed != 0,
            merged: merged != 0,
            released: released != 0,
            deployed: deployed != 0,
            submitted: submitted != 0,
            approved: approved != 0,
        },
        verification,
    }))
}

fn load_verification(
    conn: &Connection,
    attachment_id: i64,
    artifact_revision: Option<&str>,
) -> StoreResult<Vec<VerificationEvidence>> {
    let mut stmt = conn.prepare(
        "SELECT check_name, observed_result, observed_at, source_run_id, applies_to_revision \
         FROM artifact_verification WHERE attachment_id = ?1 ORDER BY observed_at, check_name",
    )?;
    let rows = stmt.query_map(params![attachment_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut evidence = Vec::new();
    for row in rows {
        let (check, result, observed_at, source_run, applies_to_revision) = row?;
        let stale = artifact_revision.is_some_and(|revision| revision != applies_to_revision);
        evidence.push(VerificationEvidence {
            check,
            result,
            observed_at: jiff::Timestamp::from_microsecond(observed_at)
                .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
            source_run_id: SessionId::from_slice(&source_run)?,
            applies_to_revision,
            stale,
        });
    }
    Ok(evidence)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_relationships(
    tx: &rusqlite::Transaction<'_>,
    from_work_item_id: WorkItemId,
    from_workspace_id: WorkspaceId,
    from_project_id: ProjectId,
    actor_key: &str,
    run_id: SessionId,
    created_at: i64,
    inputs: &[RelationshipInput],
) -> StoreResult<Vec<WorkItemRelationship>> {
    let mut stored = Vec::with_capacity(inputs.len());
    for input in inputs {
        stored.push(persist_one_relationship(
            tx,
            from_work_item_id,
            from_workspace_id,
            from_project_id,
            actor_key,
            run_id,
            created_at,
            input,
        )?);
    }
    Ok(stored)
}

#[allow(clippy::too_many_arguments)]
fn persist_one_relationship(
    tx: &rusqlite::Transaction<'_>,
    from_work_item_id: WorkItemId,
    from_workspace_id: WorkspaceId,
    from_project_id: ProjectId,
    actor_key: &str,
    run_id: SessionId,
    created_at: i64,
    input: &RelationshipInput,
) -> StoreResult<WorkItemRelationship> {
    if input.target_work_item_id == from_work_item_id {
        return Err(StoreError::InvalidState(
            "work item relationship cannot be a self-link".into(),
        ));
    }
    // V103's UNIQUE(kind, from, to) stops the same parent being recorded
    // twice but not two *different* parents, which would make
    // `parent_of_child` pick one arbitrarily. A child has exactly one
    // parent, enforced here rather than by a migration (#54).
    if input.kind == WorkItemRelationshipKind::ChildOf {
        let existing_parent: Option<Vec<u8>> = tx
            .query_row(
                "SELECT to_work_item_id FROM work_item_relationships \
                 WHERE kind = 'child_of' AND from_work_item_id = ?1 LIMIT 1",
                params![from_work_item_id.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        if existing_parent.is_some() {
            return Err(StoreError::InvalidState(
                SECOND_PARENT_FORBIDDEN.to_string(),
            ));
        }
    }
    let from_owner: Option<String> = tx
        .query_row(
            "SELECT owner_actor FROM work_items WHERE id = ?1",
            params![from_work_item_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    match from_owner {
        Some(owner) if owner == actor_key => {}
        Some(_) => {
            return Err(StoreError::InvalidState(
                RELATIONSHIP_UNAUTHORIZED.to_string(),
            ));
        }
        None => {
            return Err(StoreError::NotFound(format!(
                "related work item {from_work_item_id}"
            )));
        }
    }
    let target: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT workspace_id, project_id FROM work_items WHERE id = ?1",
            params![input.target_work_item_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((target_ws, target_project)) = target else {
        return Err(StoreError::NotFound(format!(
            "related work item {}",
            input.target_work_item_id
        )));
    };
    if target_ws.as_slice() != input.target_workspace_id.as_bytes()
        || target_project.as_slice() != input.target_project_id.as_bytes()
    {
        return Err(StoreError::InvalidState(
            "related work item does not belong to the resolved target scope".into(),
        ));
    }
    if relationship_would_cycle(tx, input.kind, from_work_item_id, input.target_work_item_id)? {
        return Err(StoreError::InvalidState(format!(
            "{} relationships must remain acyclic",
            input.kind.as_str()
        )));
    }
    let id = WorkItemRelationshipId::new();
    tx.execute(
        "INSERT INTO work_item_relationships \
         (id, kind, from_work_item_id, to_work_item_id, from_workspace_id, from_project_id, \
          to_workspace_id, to_project_id, created_by_actor, created_by_run_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.as_bytes(),
            input.kind.as_str(),
            from_work_item_id.as_bytes(),
            input.target_work_item_id.as_bytes(),
            from_workspace_id.as_bytes(),
            from_project_id.as_bytes(),
            input.target_workspace_id.as_bytes(),
            input.target_project_id.as_bytes(),
            actor_key,
            run_id.as_bytes(),
            created_at,
        ],
    )?;
    Ok(WorkItemRelationship {
        id,
        kind: input.kind,
        from_work_item_id,
        to_work_item_id: input.target_work_item_id,
        from_workspace_id,
        from_project_id,
        to_workspace_id: input.target_workspace_id,
        to_project_id: input.target_project_id,
        created_by_run_id: run_id,
        created_at: jiff::Timestamp::from_microsecond(created_at)
            .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
    })
}

fn relationship_would_cycle(
    tx: &rusqlite::Transaction<'_>,
    kind: WorkItemRelationshipKind,
    from: WorkItemId,
    to: WorkItemId,
) -> StoreResult<bool> {
    if !kind.requires_dag() {
        return Ok(false);
    }
    let mut stack = vec![to];
    let mut seen = std::collections::HashSet::from([to]);
    while let Some(current) = stack.pop() {
        if current == from {
            return Ok(true);
        }
        let mut stmt = tx.prepare(
            "SELECT to_work_item_id FROM work_item_relationships \
             WHERE kind = ?1 AND from_work_item_id = ?2",
        )?;
        let rows = stmt.query_map(params![kind.as_str(), current.as_bytes()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        for row in rows {
            let next = WorkItemId::from_slice(&row?)?;
            if seen.insert(next) {
                stack.push(next);
            }
        }
    }
    Ok(false)
}

pub(crate) fn load_work_item_relationships(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StoreResult<Vec<WorkItemRelationship>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, from_work_item_id, to_work_item_id, from_workspace_id, from_project_id, \
                to_workspace_id, to_project_id, created_by_run_id, created_at \
         FROM work_item_relationships \
         WHERE from_work_item_id = ?1 OR to_work_item_id = ?1 \
         ORDER BY created_at, kind",
    )?;
    let rows = stmt.query_map(params![work_item_id.as_bytes()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Vec<u8>>(4)?,
            row.get::<_, Vec<u8>>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, Vec<u8>>(7)?,
            row.get::<_, Vec<u8>>(8)?,
            row.get::<_, i64>(9)?,
        ))
    })?;
    let mut relationships = Vec::new();
    for row in rows {
        let (id, kind, from_id, to_id, from_ws, from_proj, to_ws, to_proj, run_id, created_at) =
            row?;
        relationships.push(WorkItemRelationship {
            id: WorkItemRelationshipId::from_slice(&id)?,
            kind: kind.parse()?,
            from_work_item_id: WorkItemId::from_slice(&from_id)?,
            to_work_item_id: WorkItemId::from_slice(&to_id)?,
            from_workspace_id: WorkspaceId::from_slice(&from_ws)?,
            from_project_id: ProjectId::from_slice(&from_proj)?,
            to_workspace_id: WorkspaceId::from_slice(&to_ws)?,
            to_project_id: ProjectId::from_slice(&to_proj)?,
            created_by_run_id: SessionId::from_slice(&run_id)?,
            created_at: jiff::Timestamp::from_microsecond(created_at)
                .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
        });
    }
    Ok(relationships)
}

/// Whether `(actor_key, run_id)` acts as a *child* of `parent_work_item_id`,
/// which is what makes completing, abandoning, claiming, or superseding that
/// parent forbidden.
///
/// Owning the parent settles the question first. One Run legitimately creates
/// a parent WorkItem and a child of it, and that Run owns both rows; without
/// the ownership check the child relationship it just created would classify
/// it as a foreign child and lock it out of the parent it owns (#54).
pub(crate) fn actor_run_is_child_of(
    conn: &Connection,
    parent_work_item_id: WorkItemId,
    actor_key: &str,
    run_id: SessionId,
) -> StoreResult<bool> {
    let found: i64 = conn.query_row(
        "SELECT EXISTS(\
            SELECT 1 FROM work_item_relationships r \
            JOIN work_items child ON child.id = r.from_work_item_id \
            WHERE r.kind = 'child_of' \
              AND r.to_work_item_id = ?1 \
              AND child.owner_actor = ?2 \
              AND child.owner_run_id = ?3 \
              AND NOT EXISTS (\
                    SELECT 1 FROM work_items parent \
                    WHERE parent.id = ?1 \
                      AND parent.owner_actor = ?2 \
                      AND parent.owner_run_id = ?3)\
         )",
        params![parent_work_item_id.as_bytes(), actor_key, run_id.as_bytes()],
        |row| row.get(0),
    )?;
    Ok(found != 0)
}

/// The single parent of a child WorkItem, if it has one.
///
/// `persist_one_relationship` rejects a second `child_of` row for the same
/// `from_work_item_id`, so at most one row can match; the ordering only keeps
/// the read deterministic for stores written before that check existed.
pub(crate) fn parent_of_child(
    conn: &Connection,
    child_work_item_id: WorkItemId,
) -> StoreResult<Option<WorkItemId>> {
    let row: Option<Vec<u8>> = conn
        .query_row(
            "SELECT to_work_item_id FROM work_item_relationships \
             WHERE kind = 'child_of' AND from_work_item_id = ?1 \
             ORDER BY created_at LIMIT 1",
            params![child_work_item_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    row.as_deref()
        .map(WorkItemId::from_slice)
        .transpose()
        .map_err(StoreError::from)
}

pub(crate) fn persist_parent_result(
    tx: &rusqlite::Transaction<'_>,
    child_work_item_id: WorkItemId,
    checkpoint_id: engram_core::CheckpointId,
    source_run_id: SessionId,
    observed_at: i64,
    input: &ParentResultInput,
) -> StoreResult<ParentResult> {
    let parent_id = parent_of_child(tx, child_work_item_id)?.ok_or_else(|| {
        StoreError::InvalidState(
            "parent_result requires the WorkItem to be a child_of an existing parent".into(),
        )
    })?;
    let id = Uuid::now_v7();
    let (workspace_bytes, project_bytes): (Vec<u8>, Vec<u8>) = tx.query_row(
        "SELECT workspace_id, project_id FROM work_items WHERE id = ?1",
        params![child_work_item_id.as_bytes()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    tx.execute(
        "INSERT INTO parent_results \
         (id, child_work_item_id, parent_work_item_id, checkpoint_id, summary, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id.as_bytes().as_slice(),
            child_work_item_id.as_bytes(),
            parent_id.as_bytes(),
            checkpoint_id.as_bytes(),
            input.summary,
            observed_at,
        ],
    )?;
    let artifacts = persist_artifacts(
        tx,
        "parent_result",
        id.as_bytes(),
        WorkspaceId::from_slice(&workspace_bytes)?,
        ProjectId::from_slice(&project_bytes)?,
        source_run_id,
        observed_at,
        &input.artifacts,
    )?;
    Ok(ParentResult {
        child_work_item_id,
        parent_work_item_id: parent_id,
        summary: input.summary.clone(),
        artifacts,
        created_at: jiff::Timestamp::from_microsecond(observed_at)
            .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
    })
}

pub(crate) fn load_child_results(
    conn: &Connection,
    parent_work_item_id: WorkItemId,
) -> StoreResult<Vec<ParentResult>> {
    let mut stmt = conn.prepare(
        "SELECT id, child_work_item_id, summary, created_at \
         FROM parent_results WHERE parent_work_item_id = ?1 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![parent_work_item_id.as_bytes()], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut results = Vec::new();
    for row in rows {
        let (id, child_id, summary, created_at) = row?;
        let artifacts = load_owner_artifacts(conn, "parent_result", &id)?;
        results.push(ParentResult {
            child_work_item_id: WorkItemId::from_slice(&child_id)?,
            parent_work_item_id,
            summary,
            artifacts,
            created_at: jiff::Timestamp::from_microsecond(created_at)
                .map_err(|error| StoreError::MalformedRecord(error.to_string()))?,
        });
    }
    Ok(results)
}

pub(crate) fn audit_artifact_ids(artifacts: &[ArtifactRef]) -> Vec<String> {
    artifacts
        .iter()
        .map(|artifact| artifact.id.to_string())
        .collect()
}

fn reject_content_hash_conflict(
    tx: &rusqlite::Transaction<'_>,
    artifact_id: ArtifactId,
    incoming: Option<&str>,
) -> StoreResult<()> {
    let Some(incoming) = incoming else {
        return Ok(());
    };
    let mut stmt = tx.prepare(
        "SELECT content_hash FROM artifact_attachments \
         WHERE artifact_id = ?1 AND content_hash IS NOT NULL",
    )?;
    let hashes = stmt.query_map(params![artifact_id.as_bytes()], |row| {
        row.get::<_, String>(0)
    })?;
    for hash in hashes {
        if hash? != incoming {
            return Err(StoreError::InvalidState(
                "artifact content-hash mismatch for the same identity".into(),
            ));
        }
    }
    Ok(())
}

fn artifact_id_from_hash(hash: &[u8; 32]) -> ArtifactId {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    ArtifactId(Uuid::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    //! Parent/child resolution rules, tested against real rows because both
    //! bugs they cover were SQL-shaped rather than Rust-shaped.
    use super::*;
    use crate::ops::{get_or_create_project, get_or_create_workspace};
    use engram_core::WorkItemRelationshipKind;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn fresh_db() -> (TempDir, Connection, WorkspaceId, ProjectId) {
        let tmp = TempDir::new().unwrap();
        let mut conn = Connection::open(tmp.path().join("test.sqlite")).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        (tmp, conn, ws, proj)
    }

    fn work_item(
        conn: &Connection,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        objective: &str,
        owner_actor: &str,
        owner_run_id: SessionId,
    ) -> WorkItemId {
        let id = WorkItemId::new();
        let now = jiff::Timestamp::now().as_microsecond();
        conn.execute(
            "INSERT INTO work_items \
             (id, workspace_id, project_id, objective, acceptance_criteria, state, \
              revision, owner_actor, owner_run_id, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, '[]', 'active', 1, ?5, ?6, ?7, ?7)",
            params![
                id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                objective,
                owner_actor,
                owner_run_id.as_bytes(),
                now,
            ],
        )
        .unwrap();
        id
    }

    fn link(
        conn: &mut Connection,
        from: WorkItemId,
        scope: (WorkspaceId, ProjectId),
        owner: (&str, SessionId),
        kind: WorkItemRelationshipKind,
        target: WorkItemId,
    ) -> StoreResult<WorkItemRelationship> {
        let (workspace_id, project_id) = scope;
        let (actor_key, run_id) = owner;
        let tx = conn.transaction().unwrap();
        let result = persist_one_relationship(
            &tx,
            from,
            workspace_id,
            project_id,
            actor_key,
            run_id,
            jiff::Timestamp::now().as_microsecond(),
            &RelationshipInput {
                kind,
                target_work_item_id: target,
                target_workspace_id: workspace_id,
                target_project_id: project_id,
            },
        );
        if result.is_ok() {
            tx.commit().unwrap();
        }
        result
    }

    /// #54: one Run that creates a parent WorkItem and a child of it owns
    /// both. The child link must not turn that Run into a forbidden child of
    /// the parent it owns, which is what made `CHILD_MUTATION_FORBIDDEN` fire
    /// on the owner's own parent.
    #[test]
    fn parent_owner_is_not_its_own_forbidden_child() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let run = SessionId::new();
        let parent = work_item(&conn, ws, proj, "parent", "agent:alice", run);
        let child = work_item(&conn, ws, proj, "child", "agent:alice", run);
        link(
            &mut conn,
            child,
            (ws, proj),
            ("agent:alice", run),
            WorkItemRelationshipKind::ChildOf,
            parent,
        )
        .unwrap();

        assert!(
            !actor_run_is_child_of(&conn, parent, "agent:alice", run).unwrap(),
            "the Run owning the parent must stay free to mutate it"
        );

        // A different Run that only owns the child is still the child.
        let other_run = SessionId::new();
        let foreign_child = work_item(&conn, ws, proj, "foreign child", "agent:bob", other_run);
        link(
            &mut conn,
            foreign_child,
            (ws, proj),
            ("agent:bob", other_run),
            WorkItemRelationshipKind::ChildOf,
            parent,
        )
        .unwrap();
        assert!(
            actor_run_is_child_of(&conn, parent, "agent:bob", other_run).unwrap(),
            "a Run that owns only the child stays a forbidden parent-mutator"
        );
    }

    /// #54: V103's UNIQUE(kind, from, to) allows two *different* parents for
    /// one child, which would make `parent_of_child` arbitrary. The second
    /// `child_of` is refused at the store layer.
    #[test]
    fn second_child_of_parent_is_rejected() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let run = SessionId::new();
        let child = work_item(&conn, ws, proj, "child", "agent:alice", run);
        let first_parent = work_item(&conn, ws, proj, "parent one", "agent:alice", run);
        let second_parent = work_item(&conn, ws, proj, "parent two", "agent:alice", run);

        link(
            &mut conn,
            child,
            (ws, proj),
            ("agent:alice", run),
            WorkItemRelationshipKind::ChildOf,
            first_parent,
        )
        .unwrap();
        let error = link(
            &mut conn,
            child,
            (ws, proj),
            ("agent:alice", run),
            WorkItemRelationshipKind::ChildOf,
            second_parent,
        )
        .expect_err("a child cannot acquire a second parent");
        assert!(
            error.to_string().contains(SECOND_PARENT_FORBIDDEN),
            "{error}"
        );
        assert_eq!(parent_of_child(&conn, child).unwrap(), Some(first_parent));

        // Other relationship kinds stay unrestricted.
        link(
            &mut conn,
            child,
            (ws, proj),
            ("agent:alice", run),
            WorkItemRelationshipKind::DependsOn,
            second_parent,
        )
        .unwrap();
    }
}
