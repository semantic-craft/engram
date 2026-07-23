//! SQLite storage layer for engram.
//!
//! The crate owns a single SQLite file under `<data_dir>/db/memory.sqlite`,
//! opens it in WAL mode with foreign keys on, runs all pending migrations
//! at startup, and exposes a [`WriterHandle`] that serialises every mutation
//! through a dedicated OS thread.
//!
//! Reader-side APIs land in milestone M1-B; the writer + migrations are
//! sufficient for M1-A's "drop a page in, see it persisted" demo.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

mod auto_improve;
pub mod decay;
mod error;
mod fts_query;
mod migrations;
mod ops;
mod reader;
mod scope;
pub mod users;
mod writer;

pub use fts_query::prepare_fts5_query;

pub use auto_improve::{
    ApproveAutoImproveProposal, ApproveAutoImproveProposalResult, AutoImproveProposalDetail,
    AutoImproveProposalEvent, AutoImproveProposalOperation, AutoImproveProposalStatus,
    AutoImproveProposalSummary, AutoImproveRejectionSummary, AutoImproveTelemetryAggregate,
    AutoImproveTelemetryCount, FailAutoImproveProposal, NewAutoImproveProposal,
    RejectAutoImproveProposal, StageAutoImproveRun, StagedAutoImproveRun, artifact_path_for,
};
pub use decay::{DecayParams, retention_score};
pub use error::{StoreError, StoreResult};
pub use ops::{EmbeddingWrite, MoveSummary, PurgeSummary, ReorgSummary};
pub use reader::{
    ActivityWindow, AutoImproveCandidateSession, BriefPageBody, BriefingPage, BriefingSnapshot,
    ContaminationFinding, ContaminationReport, ContaminationSummary, DecayCandidate,
    DerivedIndexStatus, EmbeddingTripleCount, HealthDetail, HealthPage, ObservationHit,
    OpenSession, PageAuthor, PageHit, PageHitWithMeta, PageLinks, PageMeta, PageSummary,
    ProjectSummary, ReaderPool, ReindexTargetStatus, RelatedPage, ScopeRow, SessionEndDisposition,
    StatusCounts, StoredEmbedding, StoredPageBody, WorkspaceScopeRow, WorkspaceSummary,
    f32_vec_to_bytes,
};
pub use scope::{
    ResolvedScope, ScopeName, ScopeResolutionError, ScopeResolver, WORKSPACE_PROJECT_PAIR_REQUIRED,
    create_explicit_scope, create_global_scope, lookup_existing_scope, lookup_global_scope,
    resolve_many_existing_scopes,
};
pub use users::{TOKEN_HASH_LEN, TOKEN_RAW_LEN, TokenPepper, generate_token, hash_token};
pub use writer::WriterHandle;

/// Filename used inside the data dir's `db/` subdirectory.
pub const DB_FILENAME: &str = "memory.sqlite";

/// Default soft cap for the read-only connection pool.
const READER_POOL_SOFT_CAP: usize = 4;

/// Open (and migrate) a [`Store`] rooted at the given data directory.
pub struct Store {
    /// Cloneable handle to submit mutations.
    pub writer: WriterHandle,
    /// Cloneable handle for read-only queries.
    pub reader: ReaderPool,
    db_path: PathBuf,
}

impl Store {
    /// Open the SQLite file at `<data_dir>/db/memory.sqlite`, applying any
    /// outstanding migrations, then spawn the writer thread and prepare
    /// the read-only connection pool.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the file cannot be opened, migrations
    /// cannot be applied, or the writer thread fails to start.
    pub fn open(data_dir: &Path) -> StoreResult<Self> {
        let db_dir = data_dir.join("db");
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join(DB_FILENAME);

        let mut conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?; // ms

        // SQLite cannot disable FK enforcement inside refinery's per-migration
        // transaction. Keep it off while migrations rebuild tables, then enable
        // it for all runtime reads/writes below.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrations::run(&mut conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let writer = WriterHandle::spawn(conn);
        let reader = ReaderPool::new(&db_path, READER_POOL_SOFT_CAP)?;
        Ok(Self {
            writer,
            reader,
            db_path,
        })
    }

    /// Path of the SQLite file on disk.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{
        ActorContext, AgentKind, LinkTarget, NewObservation, NewPage, NewSession, ObservationId,
        ObservationKind, PageId, PagePath, ProjectId, SessionId, Tier, UserId, WorkspaceId,
    };
    use rusqlite::{Connection, params};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    fn sample_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
        NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "test".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
        }
    }

    fn proposal(
        path: &str,
        op: AutoImproveProposalOperation,
        body: &str,
    ) -> NewAutoImproveProposal {
        NewAutoImproveProposal {
            operation: op,
            target_path: PagePath::new(path).unwrap(),
            kind: "note".into(),
            title: "Proposed".into(),
            confidence: 0.9,
            rationale: "rationale".into(),
            evidence_json: serde_json::json!([{"source":"test"}]),
            body_markdown: body.into(),
            artifact_sha256: None,
            edit_mode: None,
            patch_json: None,
            expected_base_body_sha256: None,
        }
    }

    fn delete_scheduler_state(store: &Store, ws: WorkspaceId, proj: ProjectId) {
        let conn = Connection::open(store.db_path()).unwrap();
        conn.execute(
            "DELETE FROM auto_improve_scheduler_state WHERE workspace_id = ?1 AND project_id = ?2",
            params![ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
    }

    fn stage_input(
        ws: WorkspaceId,
        proj: ProjectId,
        proposals: Vec<NewAutoImproveProposal>,
    ) -> StageAutoImproveRun {
        StageAutoImproveRun {
            workspace_id: ws,
            project_id: proj,
            session_id: None,
            provider: Some("test".into()),
            model: Some("model".into()),
            summary: Some("summary".into()),
            warnings_json: serde_json::json!([]),
            rejected_candidates_json: serde_json::json!([]),
            config_json: serde_json::json!({"mode":"stage"}),
            proposal_actor: ActorContext {
                agent: Some("auto_improve".into()),
                ..ActorContext::default()
            },
            proposals,
        }
    }

    fn sha256(body: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        hasher.finalize().into()
    }

    fn latest_snapshot(
        db_path: &std::path::Path,
        ws: WorkspaceId,
        proj: ProjectId,
        path: &str,
    ) -> (PageId, [u8; 32], i64) {
        let conn = Connection::open(db_path).unwrap();
        let (id, hash, updated): (Vec<u8>, Vec<u8>, i64) = conn.query_row(
            "SELECT id, body_sha256, updated_at FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![ws.as_bytes(), proj.as_bytes(), path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        (
            PageId::from_slice(&id).unwrap(),
            hash.try_into().unwrap(),
            updated,
        )
    }

    fn telemetry_count(rows: &[AutoImproveTelemetryCount], key: &str) -> usize {
        rows.iter()
            .find(|row| row.key == key)
            .map(|row| row.count)
            .unwrap_or(0)
    }

    // Issue #157: the documented safety invariant "pinned pages are never
    // rewritten by auto-improvement" is enforced at the single apply point
    // every flow shares (manual approval AND require_approval=false
    // auto-apply), so no prompt phrasing or approval policy can bypass it.
    #[tokio::test]
    async fn approve_refuses_update_proposals_against_pinned_pages() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();

        // A pinned decision record and an unpinned sibling.
        let mut pinned = sample_page(ws, proj, "decisions/adr-0001.md", "immutable decision");
        pinned.pinned = true;
        store.writer.upsert_page(pinned).await.unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/mutable.md", "old body"))
            .await
            .unwrap();

        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "decisions/adr-0001.md",
                        AutoImproveProposalOperation::Update,
                        "rewritten decision",
                    ),
                    proposal(
                        "notes/mutable.md",
                        AutoImproveProposalOperation::Update,
                        "new body",
                    ),
                ],
            ))
            .await
            .unwrap();
        let actor = ActorContext::default();

        // Pinned target: refused as a conflict, nothing written.
        let approve = |proposal_id, path: &str, body: &str| ApproveAutoImproveProposal {
            workspace_id: ws,
            project_id: proj,
            proposal_id,
            page: sample_page(ws, proj, path, body),
            actor: actor.clone(),
            author_id: None,
            checkpoint: None,
        };
        assert_eq!(
            store
                .writer
                .approve_auto_improve_proposal(approve(
                    staged.proposal_ids[0],
                    "decisions/adr-0001.md",
                    "rewritten decision",
                ))
                .await
                .unwrap(),
            ApproveAutoImproveProposalResult::Conflict,
            "pinned target must be refused"
        );
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert!(
            detail
                .decision_reason
                .as_deref()
                .is_some_and(|r| r.contains("pinned")),
            "decision reason must say WHY: {:?}",
            detail.decision_reason
        );
        let (_, body_hash, _) = latest_snapshot(store.db_path(), ws, proj, "decisions/adr-0001.md");
        assert_eq!(
            body_hash,
            sha256("immutable decision"),
            "pinned body untouched"
        );

        // Unpinned sibling still approves normally.
        assert!(matches!(
            store
                .writer
                .approve_auto_improve_proposal(approve(
                    staged.proposal_ids[1],
                    "notes/mutable.md",
                    "new body",
                ))
                .await
                .unwrap(),
            ApproveAutoImproveProposalResult::Approved { .. }
        ));
    }

    #[tokio::test]
    async fn auto_improve_migration_and_stage_persist_reopen_list_detail_scope() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/a.md",
                    AutoImproveProposalOperation::Create,
                    "# A",
                )],
            ))
            .await
            .unwrap();
        assert_eq!(staged.proposal_ids.len(), 1);
        let pending = store
            .reader
            .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target_path.as_str(), "notes/a.md");
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.events.len(), 1);
        assert_eq!(detail.edit_mode, "full_page");
        assert!(detail.patch_json.is_none());
        assert!(detail.expected_base_body_sha256.is_none());
        assert_eq!(
            detail.artifact_path,
            format!("_pending/auto-improve/{}.md", staged.proposal_ids[0])
        );
        assert!(
            store
                .reader
                .auto_improve_proposal_detail(ws, other, staged.proposal_ids[0])
                .await
                .unwrap()
                .is_none()
        );
        drop(store);
        let reopened = Store::open(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_reject_pending_only_records_event() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/r.md",
                    AutoImproveProposalOperation::Create,
                    "# R",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let actor = ActorContext {
            user: Some("reviewer".into()),
            ..ActorContext::default()
        };
        store
            .writer
            .reject_auto_improve_proposal(RejectAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: id,
                reason: "nope".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Rejected);
        assert_eq!(detail.decision_reason.as_deref(), Some("nope"));
        assert_eq!(detail.events.last().unwrap().event, "rejected");
        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].target_path.as_deref(), Some("notes/r.md"));
        assert_eq!(rejections[0].reason, "nope");
        assert_eq!(rejections[0].source_proposal_id, Some(id));
        assert_eq!(rejections[0].normalized_fingerprint.len(), 64);
        assert!(
            store
                .writer
                .reject_auto_improve_proposal(RejectAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: id,
                    reason: "again".into(),
                    actor,
                    author_id: None
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auto_improve_old_pending_proposal_survives_rejection_buffer_migration() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);
        let mut conn = Connection::open(&db_path).unwrap();
        migrations::run_to(&mut conn, 23).unwrap();
        let ws = ops::get_or_create_workspace(&mut conn, "default").unwrap();
        let proj = ops::get_or_create_project(&mut conn, &ws, "app", None).unwrap();
        let id = auto_improve::stage_run(
            &mut conn,
            &stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/old.md",
                    AutoImproveProposalOperation::Create,
                    "old",
                )],
            ),
        )
        .unwrap()
        .proposal_ids[0];
        drop(conn);

        let store = Store::open(tmp.path()).unwrap();
        store
            .writer
            .reject_auto_improve_proposal(RejectAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: id,
                reason: "old pending still rejectable".into(),
                actor: ActorContext::default(),
                author_id: None,
            })
            .await
            .unwrap();

        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Rejected);
        assert_eq!(detail.edit_mode, "full_page");
        assert_eq!(
            store
                .reader
                .recent_auto_improve_rejections(ws, proj, 10, None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_derives_snapshots_and_validates_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/update.md", "old"))
            .await
            .unwrap();
        let (latest_id, latest_hash, latest_updated) =
            latest_snapshot(store.db_path(), ws, proj, "notes/update.md");
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "notes/create.md",
                        AutoImproveProposalOperation::Create,
                        "new",
                    ),
                    proposal(
                        "notes/update.md",
                        AutoImproveProposalOperation::Update,
                        "newer",
                    ),
                ],
            ))
            .await
            .unwrap();
        let create = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert!(create.target_latest_page_id_at_stage.is_none());
        assert!(create.target_body_sha256_at_stage.is_none());
        assert!(create.target_updated_at_at_stage.is_none());
        let update = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.target_latest_page_id_at_stage, Some(latest_id));
        assert_eq!(update.target_body_sha256_at_stage, Some(latest_hash));
        assert_eq!(update.target_updated_at_at_stage, Some(latest_updated));

        // A create proposal whose target page already exists is a
        // per-proposal race: it is skipped and reported, not a run-level
        // error. (Uses a page with no queued proposal so this exercises the
        // create guard rather than the pending-collision guard.)
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/exists.md", "already here"))
            .await
            .unwrap();
        let raced = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/exists.md",
                    AutoImproveProposalOperation::Create,
                    "bad",
                )],
            ))
            .await
            .unwrap();
        assert!(raced.proposal_ids.is_empty());
        assert!(
            raced.skipped[0].reason.contains("target already exists"),
            "{:?}",
            raced.skipped
        );

        let out_of_scope_session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: out_of_scope_session,
                workspace_id: ws,
                project_id: other,
                agent_kind: AgentKind::Codex,
                cwd: None,
            })
            .await
            .unwrap();
        let mut input = stage_input(
            ws,
            proj,
            vec![proposal(
                "notes/session.md",
                AutoImproveProposalOperation::Create,
                "session",
            )],
        );
        input.session_id = Some(out_of_scope_session);
        assert!(store.writer.stage_auto_improve_run(input).await.is_err());
    }

    /// Two proposals for the same target inside ONE batch: the first is
    /// staged, the second skipped. (This used to roll the whole run back —
    /// see `auto_improve_stage_skips_target_with_existing_pending_proposal`
    /// for why that cost real reviews in production.) The
    /// one-pending-proposal-per-target invariant still holds.
    #[tokio::test]
    async fn auto_improve_duplicate_target_within_batch_keeps_the_first() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal("notes/dupe.md", AutoImproveProposalOperation::Create, "one"),
                    proposal("notes/dupe.md", AutoImproveProposalOperation::Create, "two"),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(staged.proposal_ids.len(), 1);
        assert_eq!(staged.skipped.len(), 1);
        assert_eq!(staged.skipped[0].target_path, "notes/dupe.md");

        let pending = store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, staged.proposal_ids[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.body_markdown, "one", "the first proposal wins");
    }

    #[tokio::test]
    async fn auto_improve_stage_persists_validator_rejections_with_scope_isolation() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        let mut input = stage_input(ws, proj, Vec::new());
        input.rejected_candidates_json = serde_json::json!([{
            "reason": "duplicate_existing_path",
            "evidence": "notes/repeat.md",
            "target_path": "notes/repeat.md",
            "kind": "note",
            "operation": "create_or_update",
            "edit_mode": "full_page"
        }]);
        let staged = store.writer.stage_auto_improve_run(input).await.unwrap();

        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(
            rejections[0].target_path.as_deref(),
            Some("notes/repeat.md")
        );
        assert_eq!(rejections[0].kind.as_deref(), Some("note"));
        assert_eq!(rejections[0].source_run_id, Some(staged.run_id));
        assert_eq!(rejections[0].source_proposal_id, None);
        assert!(
            store
                .reader
                .recent_auto_improve_rejections(ws, other, 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_improve_fail_pending_only_records_event() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/fail.md",
                    AutoImproveProposalOperation::Create,
                    "fail",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let actor = ActorContext {
            agent: Some("admission".into()),
            ..ActorContext::default()
        };
        store
            .writer
            .fail_auto_improve_proposal(FailAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: id,
                reason: "admission denied".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Failed);
        assert_eq!(detail.events.last().unwrap().event, "failed");
        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].target_path.as_deref(), Some("notes/fail.md"));
        assert_eq!(rejections[0].reason, "admission denied");
        assert!(
            store
                .writer
                .fail_auto_improve_proposal(FailAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: id,
                    reason: "again".into(),
                    actor,
                    author_id: None,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auto_improve_telemetry_aggregate_counts_learning_activity_only() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/update.md", "old update"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "procedures/patch.md", "old patch"))
            .await
            .unwrap();

        let mut update = proposal(
            "notes/update.md",
            AutoImproveProposalOperation::Update,
            "new update",
        );
        update.kind = "decision".into();
        let mut patch = proposal(
            "procedures/patch.md",
            AutoImproveProposalOperation::Update,
            "new patch",
        );
        patch.kind = "procedure".into();
        patch.edit_mode = Some("patch".into());
        patch.patch_json = Some(serde_json::json!([{ "op": "append", "content": "new" }]));
        patch.expected_base_body_sha256 = Some(sha256("old patch"));

        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "notes/pending.md",
                        AutoImproveProposalOperation::Create,
                        "pending",
                    ),
                    proposal(
                        "notes/approved.md",
                        AutoImproveProposalOperation::Create,
                        "approved",
                    ),
                    proposal(
                        "notes/rejected.md",
                        AutoImproveProposalOperation::Create,
                        "rejected",
                    ),
                    proposal(
                        "notes/failed.md",
                        AutoImproveProposalOperation::Create,
                        "failed",
                    ),
                    proposal(
                        "notes/conflict.md",
                        AutoImproveProposalOperation::Create,
                        "proposal",
                    ),
                    update,
                    patch,
                ],
            ))
            .await
            .unwrap();
        let approved_id = staged.proposal_ids[1];
        let rejected_id = staged.proposal_ids[2];
        let failed_id = staged.proposal_ids[3];
        let conflict_id = staged.proposal_ids[4];
        let actor = ActorContext::default();

        store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: approved_id,
                page: sample_page(ws, proj, "notes/approved.md", "approved"),
                actor: actor.clone(),
                author_id: None,
                checkpoint: None,
            })
            .await
            .unwrap();
        store
            .writer
            .reject_auto_improve_proposal(RejectAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: rejected_id,
                reason: "human rejected".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        store
            .writer
            .fail_auto_improve_proposal(FailAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: failed_id,
                reason: "admission denied".into(),
                actor: actor.clone(),
                author_id: None,
            })
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/conflict.md", "external"))
            .await
            .unwrap();
        assert_eq!(
            store
                .writer
                .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: conflict_id,
                    page: sample_page(ws, proj, "notes/conflict.md", "proposal"),
                    actor: actor.clone(),
                    author_id: None,
                    checkpoint: None,
                })
                .await
                .unwrap(),
            ApproveAutoImproveProposalResult::Conflict
        );

        let mut curator_report = proposal(
            "reports/curator.md",
            AutoImproveProposalOperation::Create,
            "curator",
        );
        curator_report.kind = "curator_report".into();
        let mut telemetry_report = proposal(
            "reports/auto-improve.md",
            AutoImproveProposalOperation::Create,
            "telemetry",
        );
        telemetry_report.kind = "auto_improve_report".into();
        let maintenance_staged = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![curator_report, telemetry_report],
            ))
            .await
            .unwrap();
        for id in maintenance_staged.proposal_ids {
            store
                .writer
                .reject_auto_improve_proposal(RejectAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id: id,
                    reason: "maintenance rejected".into(),
                    actor: actor.clone(),
                    author_id: None,
                })
                .await
                .unwrap();
        }

        let mut eval_rejections = stage_input(ws, proj, Vec::new());
        eval_rejections.rejected_candidates_json = serde_json::json!([
            {
                "reason": "eval_gate_failed",
                "target_path": "eval/repeat.md",
                "kind": "note",
                "operation": "create",
                "edit_mode": "full_page",
                "summary": "same eval failure"
            },
            {
                "reason": "eval_gate_failed",
                "target_path": "eval/repeat.md",
                "kind": "note",
                "operation": "create",
                "edit_mode": "full_page",
                "summary": "same eval failure"
            },
            {
                "reason": "eval_gate_timeout",
                "target_path": "eval/timeout.md",
                "summary": "timeout"
            },
            {
                "reason": "eval_gate_error",
                "target_path": "eval/error.md",
                "summary": "error"
            }
        ]);
        store
            .writer
            .stage_auto_improve_run(eval_rejections)
            .await
            .unwrap();

        store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                other,
                vec![proposal(
                    "notes/other.md",
                    AutoImproveProposalOperation::Create,
                    "other",
                )],
            ))
            .await
            .unwrap();

        let aggregate = store
            .reader
            .auto_improve_telemetry_aggregate(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(aggregate.run_count, 3);
        assert_eq!(aggregate.runs_with_learning_proposals, 1);
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "pending"),
            3
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "approved"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "rejected"),
            1
        );
        assert_eq!(telemetry_count(&aggregate.proposals_by_status, "failed"), 1);
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_status, "conflict"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_operation, "create"),
            5
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_operation, "update"),
            2
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_edit_mode, "full_page"),
            6
        );
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_edit_mode, "patch"),
            1
        );
        assert_eq!(telemetry_count(&aggregate.proposals_by_kind, "note"), 5);
        assert_eq!(telemetry_count(&aggregate.proposals_by_kind, "decision"), 1);
        assert_eq!(
            telemetry_count(&aggregate.proposals_by_kind, "procedure"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.maintenance_proposals_by_kind, "curator_report"),
            1
        );
        assert_eq!(
            telemetry_count(
                &aggregate.maintenance_proposals_by_kind,
                "auto_improve_report"
            ),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "human rejected"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "admission denied"),
            1
        );
        assert_eq!(
            telemetry_count(
                &aggregate.rejections_by_reason,
                "target changed since proposal was staged"
            ),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "eval_gate_failed"),
            2
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "eval_gate_timeout"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "eval_gate_error"),
            1
        );
        assert_eq!(
            telemetry_count(&aggregate.rejections_by_reason, "maintenance rejected"),
            0,
            "maintenance/report proposal rejections must not pollute learning telemetry"
        );
        assert_eq!(
            telemetry_count(&aggregate.rejected_targets, "eval/repeat.md"),
            2
        );
        assert_eq!(
            telemetry_count(&aggregate.rejected_targets, "reports/curator.md"),
            0
        );
        assert!(
            aggregate
                .repeated_rejection_fingerprints
                .iter()
                .any(|row| row.count == 2)
        );

        let other_aggregate = store
            .reader
            .auto_improve_telemetry_aggregate(ws, other, 0, 10)
            .await
            .unwrap();
        assert_eq!(other_aggregate.run_count, 1);
        assert_eq!(
            telemetry_count(&other_aggregate.proposals_by_status, "pending"),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_approve_upserts_page_and_conflicts_are_sql_atomic() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let actor = ActorContext {
            user: Some("approver".into()),
            ..ActorContext::default()
        };

        let create_id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/new.md",
                    AutoImproveProposalOperation::Create,
                    "approved body",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let result = store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: create_id,
                page: sample_page(ws, proj, "notes/new.md", "approved body"),
                actor: actor.clone(),
                author_id: None,
                checkpoint: Some("ck".into()),
            })
            .await
            .unwrap();
        assert!(matches!(
            result,
            ApproveAutoImproveProposalResult::Approved { .. }
        ));
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, create_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Approved);
        assert!(detail.applied_page_id.is_some());
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/new.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "approved body"
        );

        let stale_create = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/existing.md",
                    AutoImproveProposalOperation::Create,
                    "proposal",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/existing.md", "external"))
            .await
            .unwrap();
        let conflict = store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: stale_create,
                page: sample_page(ws, proj, "notes/existing.md", "proposal"),
                actor: actor.clone(),
                author_id: None,
                checkpoint: None,
            })
            .await
            .unwrap();
        assert_eq!(conflict, ApproveAutoImproveProposalResult::Conflict);
        let rejections = store
            .reader
            .recent_auto_improve_rejections(ws, proj, 10, None)
            .await
            .unwrap();
        assert!(rejections.iter().any(|rejection| {
            rejection.target_path.as_deref() == Some("notes/existing.md")
                && rejection.reason == "target changed since proposal was staged"
                && rejection.source_proposal_id == Some(stale_create)
        }));
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/existing.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "external"
        );

        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/update.md", "old"))
            .await
            .unwrap();
        let update = proposal(
            "notes/update.md",
            AutoImproveProposalOperation::Update,
            "new",
        );
        let update_id = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![update]))
            .await
            .unwrap()
            .proposal_ids[0];
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "notes/update.md",
                "changed elsewhere",
            ))
            .await
            .unwrap();
        let conflict = store
            .writer
            .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                workspace_id: ws,
                project_id: proj,
                proposal_id: update_id,
                page: sample_page(ws, proj, "notes/update.md", "new"),
                actor,
                author_id: None,
                checkpoint: None,
            })
            .await
            .unwrap();
        assert_eq!(conflict, ApproveAutoImproveProposalResult::Conflict);
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/update.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "changed elsewhere"
        );
        assert_eq!(
            sha256("approved body"),
            latest_snapshot(store.db_path(), ws, proj, "notes/new.md").1
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_rejects_patch_base_hash_mismatch() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "procedures/release.md", "old"))
            .await
            .unwrap();
        let mut patch = proposal(
            "procedures/release.md",
            AutoImproveProposalOperation::Update,
            "new",
        );
        patch.edit_mode = Some("patch".into());
        patch.patch_json =
            Some(serde_json::json!([{ "op": "append", "anchor": "## Steps", "content": "new" }]));
        patch.expected_base_body_sha256 = Some(sha256("different old body"));

        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![patch]))
            .await
            .unwrap();
        assert!(staged.proposal_ids.is_empty());
        assert_eq!(staged.skipped.len(), 1);
        assert!(
            staged.skipped[0]
                .reason
                .contains("target changed since patch materialization"),
            "{:?}",
            staged.skipped
        );
        assert!(
            store
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_improve_stage_rejects_patch_missing_base_hash_and_create() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "procedures/release.md", "old"))
            .await
            .unwrap();

        let mut missing_hash = proposal(
            "procedures/release.md",
            AutoImproveProposalOperation::Update,
            "new",
        );
        missing_hash.edit_mode = Some("patch".into());
        missing_hash.patch_json = Some(serde_json::json!([{ "op": "append" }]));
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![missing_hash]))
            .await
            .unwrap();
        assert!(staged.proposal_ids.is_empty());
        assert!(
            staged.skipped[0]
                .reason
                .contains("missing expected base body hash"),
            "{:?}",
            staged.skipped
        );

        let mut create_patch = proposal(
            "procedures/new.md",
            AutoImproveProposalOperation::Create,
            "new",
        );
        create_patch.edit_mode = Some("patch".into());
        create_patch.patch_json = Some(serde_json::json!([{ "op": "append" }]));
        create_patch.expected_base_body_sha256 = Some(sha256("old"));
        let staged = store
            .writer
            .stage_auto_improve_run(stage_input(ws, proj, vec![create_patch]))
            .await
            .unwrap();
        assert!(staged.proposal_ids.is_empty());
        assert!(
            staged.skipped[0]
                .reason
                .contains("patch proposal must use update operation"),
            "{:?}",
            staged.skipped
        );
    }

    /// Regression: with `require_approval = true` the queue holds pending
    /// proposals indefinitely, so the next scheduled review of the same
    /// project routinely proposes the same page again. That collided with
    /// the one-pending-per-target unique index and aborted the WHOLE run —
    /// in production the scheduler logged `UNIQUE constraint failed` and
    /// dropped every other proposal in the batch, and the session was
    /// already claimed so it never retried. The colliding proposal must be
    /// skipped and reported while its batch mates still land.
    #[tokio::test]
    async fn auto_improve_stage_skips_target_with_existing_pending_proposal() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        for path in ["_rules/style.md", "gotchas/timeouts.md"] {
            store
                .writer
                .upsert_page(sample_page(ws, proj, path, "old"))
                .await
                .unwrap();
        }

        let first = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "_rules/style.md",
                    AutoImproveProposalOperation::Update,
                    "first take",
                )],
            ))
            .await
            .unwrap();
        assert_eq!(first.proposal_ids.len(), 1);
        assert!(first.skipped.is_empty());

        // Second review: one proposal collides with the queued target, one
        // is new. The batch must not be lost.
        let second = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![
                    proposal(
                        "_rules/style.md",
                        AutoImproveProposalOperation::Update,
                        "second take",
                    ),
                    proposal(
                        "gotchas/timeouts.md",
                        AutoImproveProposalOperation::Update,
                        "unrelated lesson",
                    ),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(
            second.proposal_ids.len(),
            1,
            "the non-colliding proposal must still be staged"
        );
        assert_eq!(second.skipped.len(), 1);
        assert_eq!(second.skipped[0].target_path, "_rules/style.md");
        assert!(
            second.skipped[0]
                .reason
                .contains("pending proposal already"),
            "{:?}",
            second.skipped
        );

        let pending = store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap();
        assert_eq!(
            pending.len(),
            2,
            "one per target, no duplicates: {pending:?}"
        );
        let styles: Vec<_> = pending
            .iter()
            .filter(|p| p.target_path.as_str() == "_rules/style.md")
            .collect();
        assert_eq!(styles.len(), 1, "the queued proposal is kept, not replaced");
    }

    #[tokio::test]
    async fn auto_improve_approval_rejects_mismatched_page_author() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let proposal_id = store
            .writer
            .stage_auto_improve_run(stage_input(
                ws,
                proj,
                vec![proposal(
                    "notes/author.md",
                    AutoImproveProposalOperation::Create,
                    "body",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let mut page = sample_page(ws, proj, "notes/author.md", "body");
        page.author_id = Some(UserId::new());
        assert!(
            store
                .writer
                .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                    workspace_id: ws,
                    project_id: proj,
                    proposal_id,
                    page,
                    actor: ActorContext::default(),
                    author_id: None,
                    checkpoint: None,
                })
                .await
                .is_err()
        );
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/author.md")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn auto_improve_project_move_restamps_proposal_scope() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let src_ws = store
            .writer
            .get_or_create_workspace("source")
            .await
            .unwrap();
        let dst_ws = store.writer.get_or_create_workspace("dest").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(src_ws, "app", None)
            .await
            .unwrap();
        store
            .writer
            .ensure_auto_improve_scheduler_state(src_ws, proj)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let claimed_session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: claimed_session,
                workspace_id: src_ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .end_session(claimed_session, None)
            .await
            .unwrap();
        let candidate = store
            .reader
            .auto_improve_candidate_sessions(src_ws, proj, 0, 1)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            store
                .writer
                .claim_auto_improve_scheduler_session(
                    src_ws,
                    proj,
                    candidate.session_id,
                    candidate.ended_at,
                )
                .await
                .unwrap()
        );
        let proposal_id = store
            .writer
            .stage_auto_improve_run(stage_input(
                src_ws,
                proj,
                vec![proposal(
                    "notes/move.md",
                    AutoImproveProposalOperation::Create,
                    "move",
                )],
            ))
            .await
            .unwrap()
            .proposal_ids[0];
        let summary = store
            .writer
            .move_project_workspace(proj, src_ws, dst_ws)
            .await
            .unwrap();
        assert_eq!(summary.auto_improve_runs_moved, 1);
        assert_eq!(summary.auto_improve_proposals_moved, 1);
        assert_eq!(summary.auto_improve_scheduler_state_moved, 1);
        assert_eq!(summary.auto_improve_scheduler_claims_moved, 1);
        assert!(
            store
                .reader
                .auto_improve_proposal_detail(src_ws, proj, proposal_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .reader
                .auto_improve_proposal_detail(dst_ws, proj, proposal_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(dst_ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "moved scheduler claims should keep claimed sessions suppressed"
        );
    }

    /// `session_brief_pages` returns pinned / `_rules/` / `_slots/` pages
    /// WITH bodies (pinned first, then path order), recent titles for the
    /// whole project, and never leaks a sibling project's pages.
    #[tokio::test]
    async fn session_brief_pages_selects_core_pages_and_isolates_projects() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();

        let mut adr = sample_page(ws, proj, "decisions/adr-001.md", "single writer actor");
        adr.pinned = true;
        store.writer.upsert_page(adr).await.unwrap();
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "_rules/style.md",
                "no unwrap in runtime",
            ))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "_slots/focus.md", "shipping v2 auth"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "concepts/queue.md", "ordinary page"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, other, "_rules/other.md", "sibling rule"))
            .await
            .unwrap();

        let (core, recent) = store
            .reader
            .session_brief_pages(ws, proj, 24, 10)
            .await
            .unwrap();

        let core_paths: Vec<&str> = core.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            core_paths,
            vec!["decisions/adr-001.md", "_rules/style.md", "_slots/focus.md"],
            "core = pinned first, then _rules/ + _slots/ by path; no ordinary pages"
        );
        assert!(core[0].pinned, "pinned flag must survive the round-trip");
        assert_eq!(
            core[1].body, "no unwrap in runtime",
            "core pages carry bodies"
        );

        let recent_paths: Vec<&str> = recent.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            recent.len(),
            4,
            "recent lists every latest page in the project"
        );
        assert!(
            recent_paths.contains(&"concepts/queue.md"),
            "ordinary pages appear as recent pointers"
        );
        assert!(
            !recent_paths.contains(&"_rules/other.md") && core.len() == 3,
            "sibling project pages must not leak into the brief"
        );
    }

    #[tokio::test]
    async fn cross_project_links_surface_in_graph_briefing_and_lint() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let app = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();
        let infra = store
            .writer
            .get_or_create_project(ws, "infra", None)
            .await
            .unwrap();

        // Target page in `infra`, then a page in `app` that depends on it
        // plus a dangling link to a non-existent project.
        store
            .writer
            .upsert_page(sample_page(ws, infra, "runbooks/02.md", "the runbook"))
            .await
            .unwrap();
        let mut dep = sample_page(ws, app, "concepts/dep.md", "needs infra + a typo");
        dep.links = vec![
            LinkTarget {
                workspace: None,
                project: Some("infra".into()),
                path: PagePath::new("runbooks/02.md").unwrap(),
            },
            LinkTarget {
                workspace: None,
                project: Some("nope".into()),
                path: PagePath::new("ghost.md").unwrap(),
            },
        ];
        store.writer.upsert_page(dep).await.unwrap();

        // Graph: exactly one resolved cross-project edge, app -> infra.
        let edges = store.reader.cross_project_edges(None).await.unwrap();
        assert_eq!(edges.len(), 1, "one resolved cross-project edge");
        assert_eq!(edges[0].from_project, "app");
        assert_eq!(edges[0].to_project, "infra");

        // Briefing degree: app depends on 1 project; infra has 1 dependent.
        let app_brief = store.reader.briefing_for_project(ws, app, 5).await.unwrap();
        assert_eq!(app_brief.cross_project_dependencies, 1);
        assert_eq!(app_brief.cross_project_dependents, 0);
        let infra_brief = store
            .reader
            .briefing_for_project(ws, infra, 5)
            .await
            .unwrap();
        assert_eq!(infra_brief.cross_project_dependents, 1);

        // Lint: the dangling link to project `nope` is reported as unknown.
        let dangling = store
            .reader
            .dangling_cross_project_links(ws, app)
            .await
            .unwrap();
        assert_eq!(dangling.len(), 1, "only the unresolved `nope` link");
        assert_eq!(dangling[0].project, "nope");
        assert!(!dangling[0].project_exists);
    }

    #[tokio::test]
    async fn open_and_upsert_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        let id_a = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        // Same body again: returns the same id, no supersession.
        let id_b = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        assert_eq!(id_a, id_b);
        // Different body: supersession produces a new id.
        let id_c = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello world"))
            .await
            .unwrap();
        assert_ne!(id_b, id_c);
    }

    #[tokio::test]
    async fn get_or_create_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let a = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let b = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        assert_eq!(a, b);
        let pa = store
            .writer
            .get_or_create_project(a, "scratch", None)
            .await
            .unwrap();
        let pb = store
            .writer
            .get_or_create_project(a, "scratch", None)
            .await
            .unwrap();
        assert_eq!(pa, pb);
    }

    #[tokio::test]
    async fn session_agent_kind_migrations_preserve_observations() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 8).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let sid = SessionId::new();
        let oid = ObservationId::new();
        conn.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'default', 1)",
            params![ws.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, created_at) \
             VALUES (?1, ?2, 'scratch', 1)",
            params![proj.as_bytes(), ws.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, started_at) \
             VALUES (?1, ?2, ?3, 'open-code', 1)",
            params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO observations (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'user_prompt', 'keep', 'this observation must survive', 1)",
            params![oid.as_bytes(), sid.as_bytes(), ws.as_bytes(), proj.as_bytes()],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(tmp.path()).unwrap();
        let count = store.reader.status_counts().await.unwrap().observations;
        assert_eq!(count, 1, "V09 must not cascade-delete observations");

        store
            .writer
            .begin_session(NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::AntigravityCli,
                cwd: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn serialises_parallel_writes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        // Spawn 16 concurrent writes; the writer must serialise them.
        let mut handles = Vec::new();
        for i in 0..16 {
            let w = store.writer.clone();
            handles.push(tokio::spawn(async move {
                w.upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn recent_pages_returns_latest_only_in_order() {
        let tmp = TempDir::new().unwrap();
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
        for i in 0..3u8 {
            store
                .writer
                .upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
                .unwrap();
        }
        // Bump the second page to force a later updated_at.
        store
            .writer
            .upsert_page(sample_page(ws, proj, "p1.md", "body-1-rev"))
            .await
            .unwrap();

        let hits = store.reader.recent_pages(10).await.unwrap();
        assert_eq!(hits.len(), 3, "is_latest filter should give us 3 pages");
        assert_eq!(
            hits[0].path.as_str(),
            "p1.md",
            "most-recently-updated first"
        );
    }

    #[tokio::test]
    async fn status_counts_zero_on_fresh_db() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 0);
        assert_eq!(counts.pages_all, 0);
        assert_eq!(counts.sessions, 0);
        assert_eq!(counts.observations, 0);
    }

    #[tokio::test]
    async fn reindex_target_status_tracks_clean_and_dirty_store() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        let clean = store.reader.reindex_target_status().await.unwrap();
        assert!(clean.is_clean(), "fresh migrated DB must be reindex-clean");

        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "alpha.md", "body"))
            .await
            .unwrap();

        let dirty = store.reader.reindex_target_status().await.unwrap();
        assert!(
            !dirty.is_clean(),
            "existing rows must block lifecycle reindex"
        );
        assert!(dirty.nonzero_summary().contains("pages=1"));
    }

    #[tokio::test]
    async fn search_finds_inserted_page_and_counts_reflect_supersession() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "alpha.md",
                "the quick brown fox jumps over the lazy dog",
            ))
            .await
            .unwrap();

        let hits = store.reader.search_pages("quick".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "alpha.md");
        assert!(hits[0].snippet.contains("<mark>quick</mark>"));

        // Supersede: only the latest version should appear in counts'
        // pages_latest, and search should still return exactly one hit.
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "alpha.md",
                "a different sentence with quick still inside",
            ))
            .await
            .unwrap();

        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 1);
        assert_eq!(counts.pages_all, 2);

        let hits = store.reader.search_pages("quick".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("different"),
            "snippet should come from the latest version, got: {}",
            hits[0].snippet
        );
    }

    /// CJK routed search (#14): unicode61 tokenizes a CJK run as ONE token,
    /// so before the trigram shadow + router every query below returned zero
    /// hits. ≥3-char terms go through trigram MATCH, 1–2 char terms through
    /// the LIKE fallback, and mixed-script queries fan out per term.
    #[tokio::test]
    async fn search_cjk_terms_hit_via_trigram_and_like_router() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "cjk.md",
                "记忆系统的跨项目操作经验与迁移决策。",
            ))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "latin.md",
                "zotero import pipeline notes",
            ))
            .await
            .unwrap();

        // ≥3-char CJK term → trigram MATCH leg (substring semantics, real
        // FTS5 snippet with highlight).
        let hits = store
            .reader
            .search_pages("操作经验".into(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "trigram leg must hit: {hits:?}");
        assert_eq!(hits[0].path.as_str(), "cjk.md");
        assert!(
            hits[0].snippet.contains("<mark>"),
            "trigram snippet must highlight: {}",
            hits[0].snippet
        );

        // 2-char CJK term — the most common Chinese query shape — cannot
        // match trigram; the LIKE fallback leg must serve it.
        let hits = store.reader.search_pages("记忆".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 1, "LIKE leg must hit: {hits:?}");
        assert_eq!(hits[0].path.as_str(), "cjk.md");
        assert!(
            hits[0].snippet.contains("<mark>记忆</mark>"),
            "LIKE snippet must highlight: {}",
            hits[0].snippet
        );

        // Mixed-script query fans out per term and fuses across legs.
        let hits = store
            .reader
            .search_pages("zotero 迁移".into(), 10)
            .await
            .unwrap();
        let paths: Vec<_> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(
            paths.contains(&"cjk.md") && paths.contains(&"latin.md"),
            "mixed query must reach both legs: {paths:?}"
        );

        // The scoped search routes identically…
        let hits = store
            .reader
            .search_pages_for_project(ws, proj, "记忆".into(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        // …and never leaks across project scope.
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        let hits = store
            .reader
            .search_pages_for_project(ws, other, "记忆".into(), 10)
            .await
            .unwrap();
        assert!(hits.is_empty(), "scope isolation: {hits:?}");
    }

    /// Supply-chain guard (#14): the bundled SQLite must ship the trigram
    /// tokenizer (3.34+). If a future rusqlite / libsqlite3-sys bump silently
    /// dropped it, V100 would fail on fresh installs — fail loudly here
    /// instead.
    #[tokio::test]
    async fn bundled_sqlite_supports_trigram_tokenizer() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='trigram');")
            .unwrap();
    }

    /// Regression: bare `word:` in agent queries is FTS5 column syntax, not
    /// a literal token (`no such column: pick` / `memory`).
    #[tokio::test]
    async fn search_colon_tokens_do_not_error() {
        let tmp = TempDir::new().unwrap();
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
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "handoff.md",
                "pick up handoff context from engram bootstrap",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("pick: handoff bootstrap".into(), 10)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "colon-sanitized query should match without SQLite column error"
        );
    }

    #[tokio::test]
    async fn search_is_accent_insensitive() {
        // V13: an accent-free query matches accented stored text (PT-friendly).
        let tmp = TempDir::new().unwrap();
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
        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "notes/decisao.md",
                "a descrição da sessão e a consolidação dos commits",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("descricao sessao".into(), 10)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "accent-free query must match accented stored text"
        );
    }

    #[tokio::test]
    async fn search_boolean_or_still_works() {
        let tmp = TempDir::new().unwrap();
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
        store
            .writer
            .upsert_page(sample_page(ws, proj, "quick.md", "quick answer"))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages("quick OR slow".into(), 10)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "OR must remain an FTS5 operator");
    }

    #[tokio::test]
    async fn search_quotes_hyphenated_tokens_for_fts5() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(
                ws,
                proj,
                "hyphen.md",
                "the engram token should be searchable",
            ))
            .await
            .unwrap();

        let hits = store
            .reader
            .search_pages_for_project(ws, proj, "engram".into(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "hyphen.md");
    }

    #[tokio::test]
    async fn hybrid_search_includes_linked_neighbors() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();

        store
            .writer
            .upsert_page(sample_page(ws, proj, "target.md", "neighbor-only content"))
            .await
            .unwrap();
        let mut source = sample_page(ws, proj, "source.md", "needle source content");
        source.links = vec![PagePath::new("target.md").unwrap().into()];
        store.writer.upsert_page(source).await.unwrap();

        let hits = store
            .reader
            .hybrid_search(
                ws,
                proj,
                "needle".into(),
                None,
                String::new(),
                String::new(),
                0,
                10,
            )
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"source.md"));
        assert!(
            paths.contains(&"target.md"),
            "linked neighbor should be included"
        );
    }

    #[tokio::test]
    async fn observation_fts_finds_raw_fallback_hits() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "the raw-only zebra detail lives here".into(),
                importance: 5,
            })
            .await
            .unwrap();

        let hits = store
            .reader
            .search_observations_for_project(ws, proj, "zebra".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, session_id);
        assert!(hits[0].snippet.contains("<mark>zebra</mark>"));
    }

    /// The raw-observation fallback is the safety net for "we discussed it
    /// but no page compiled yet" — it was Chinese-blind, because
    /// `observations_fts` is unicode61 (one token per CJK run) and the raw
    /// log deliberately carries no trigram shadow (its size makes one a bad
    /// trade — see `fts_query::CjkIndex`). CJK terms now take the LIKE leg,
    /// so Chinese queries reach raw observations too, and mixed-script
    /// queries fuse both legs.
    #[tokio::test]
    async fn observation_fallback_finds_cjk_via_like_leg() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        let other = store
            .writer
            .get_or_create_project(ws, "other", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        for (title, body) in [
            ("用户提问", "讨论了记忆库的迁移方案，还没写成页面。"),
            ("tool", "ran zotero import for the reading list"),
        ] {
            store
                .writer
                .insert_observation(NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: title.into(),
                    body: body.into(),
                    importance: 5,
                })
                .await
                .unwrap();
        }

        // Two-char CJK term: no FTS leg can match it, the LIKE leg must.
        let hits = store
            .reader
            .search_observations_for_project(ws, proj, "迁移".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "CJK fallback must hit: {hits:?}");
        assert!(
            hits[0].snippet.contains("<mark>迁移</mark>"),
            "LIKE snippet must highlight: {}",
            hits[0].snippet
        );

        // Longer CJK term: still the LIKE leg here (no trigram shadow on
        // the raw log), so it must hit just the same.
        let hits = store
            .reader
            .search_observations_for_project(ws, proj, "记忆库".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        // Mixed-script query reaches both legs and fuses.
        let hits = store
            .reader
            .search_observations_for_project(ws, proj, "zotero 迁移".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "both legs must contribute: {hits:?}");

        // Scope isolation holds on the LIKE leg.
        let hits = store
            .reader
            .search_observations_for_project(ws, other, "迁移".into(), 5)
            .await
            .unwrap();
        assert!(hits.is_empty(), "must not leak across projects: {hits:?}");
    }

    #[tokio::test]
    async fn latest_completed_session_for_project_ignores_open_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        let first = SessionId::new();
        let open = SessionId::new();
        for id in [first, open] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
        }
        store.writer.end_session(first, None).await.unwrap();

        assert_eq!(
            store
                .reader
                .latest_completed_session_for_project(ws, proj)
                .await
                .unwrap(),
            Some(first)
        );
    }

    #[tokio::test]
    async fn auto_improve_scheduler_candidates_respect_watermark_age_and_runs() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        delete_scheduler_state(&store, ws, proj);

        let historical = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: historical,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store.writer.end_session(historical, None).await.unwrap();

        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();
        // Restart/idempotency: the second call must not reset the watermark.
        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let fresh_after_watermark = SessionId::new();
        let open_after_watermark = SessionId::new();
        for id in [fresh_after_watermark, open_after_watermark] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
        }
        store
            .writer
            .end_session(fresh_after_watermark, None)
            .await
            .unwrap();

        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 86_400, 10)
                .await
                .unwrap()
                .is_empty(),
            "too-fresh completed sessions must not be candidates"
        );

        let candidates = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, fresh_after_watermark);

        assert!(
            store
                .writer
                .claim_auto_improve_scheduler_session(
                    ws,
                    proj,
                    candidates[0].session_id,
                    candidates[0].ended_at,
                )
                .await
                .unwrap(),
            "first scheduler claim should be recorded"
        );
        assert!(
            !store
                .writer
                .claim_auto_improve_scheduler_session(
                    ws,
                    proj,
                    candidates[0].session_id,
                    candidates[0].ended_at,
                )
                .await
                .unwrap(),
            "duplicate scheduler claims should be rejected"
        );
        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "claimed sessions must not be retried if review fails before staging"
        );

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let reviewed_after_watermark = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: reviewed_after_watermark,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store
            .writer
            .end_session(reviewed_after_watermark, None)
            .await
            .unwrap();

        store
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: Some(reviewed_after_watermark),
                provider: Some("none".into()),
                model: Some("none".into()),
                summary: Some("reviewed".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({ "trigger": "scheduler" }),
                proposal_actor: ActorContext {
                    agent: Some("auto_improve".into()),
                    ..ActorContext::default()
                },
                proposals: Vec::new(),
            })
            .await
            .unwrap();

        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "any recorded run row suppresses scheduler retry for v1"
        );
    }

    #[tokio::test]
    async fn auto_improve_scheduler_state_and_candidates_are_per_project() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let first_project = store
            .writer
            .get_or_create_project(ws, "first", None)
            .await
            .unwrap();
        let second_project = store
            .writer
            .get_or_create_project(ws, "second", None)
            .await
            .unwrap();
        for project_id in [first_project, second_project] {
            delete_scheduler_state(&store, ws, project_id);
        }

        for project_id in [first_project, second_project] {
            let historical = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: historical,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
            store.writer.end_session(historical, None).await.unwrap();
        }

        for scope in store.reader.list_all_scopes().await.unwrap() {
            store
                .writer
                .ensure_auto_improve_scheduler_state(scope.workspace_id, scope.project_id)
                .await
                .unwrap();
        }

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut expected = Vec::new();
        for project_id in [first_project, second_project] {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: session_id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
            store.writer.end_session(session_id, None).await.unwrap();
            expected.push((project_id, session_id));
        }

        for (project_id, session_id) in expected {
            let candidates = store
                .reader
                .auto_improve_candidate_sessions(ws, project_id, 0, 10)
                .await
                .unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].session_id, session_id);
        }
    }

    #[tokio::test]
    async fn get_or_create_project_initializes_scheduler_state_before_first_session() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "brand-new", None)
            .await
            .unwrap();

        let first_session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: first_session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store.writer.end_session(first_session, None).await.unwrap();

        let candidates = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, first_session);
    }

    #[tokio::test]
    async fn auto_improve_scheduler_claims_do_not_skip_same_ended_at_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let first = SessionId::new();
        let second = SessionId::new();
        for id in [first, second] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
            store.writer.end_session(id, None).await.unwrap();
        }

        let same_ended_at = jiff::Timestamp::now().as_microsecond();
        let conn = Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id IN (?2, ?3)",
            params![same_ended_at, first.as_bytes(), second.as_bytes()],
        )
        .unwrap();

        let candidates = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 2);
        assert!(
            store
                .writer
                .claim_auto_improve_scheduler_session(
                    ws,
                    proj,
                    candidates[0].session_id,
                    candidates[0].ended_at,
                )
                .await
                .unwrap()
        );

        let remaining = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 10)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].session_id, candidates[0].session_id);
        assert_eq!(remaining[0].ended_at, same_ended_at);
    }

    #[tokio::test]
    async fn auto_improve_scheduler_claim_is_unique_across_store_instances() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::OpenCode,
                cwd: None,
            })
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();
        let ended_at = store
            .reader
            .auto_improve_candidate_sessions(ws, proj, 0, 1)
            .await
            .unwrap()[0]
            .ended_at;

        let second_store = Store::open(tmp.path()).unwrap();
        let (first, second) = tokio::join!(
            store
                .writer
                .claim_auto_improve_scheduler_session(ws, proj, session_id, ended_at),
            second_store
                .writer
                .claim_auto_improve_scheduler_session(ws, proj, session_id, ended_at),
        );
        let claimed = [first.unwrap(), second.unwrap()];
        assert_eq!(claimed.into_iter().filter(|claimed| *claimed).count(), 1);

        let conn = Connection::open(store.db_path()).unwrap();
        let claim_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_improve_scheduler_claims WHERE session_id = ?1",
                params![session_id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(claim_rows, 1);
    }

    #[tokio::test]
    async fn v103_schema_upgrades_to_current_without_backlog_or_integrity_breaks() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join(DB_FILENAME);

        let session_id = SessionId::new();
        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            super::migrations::run_to(&mut conn, 19).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();

            let ws = super::ops::get_or_create_workspace(&mut conn, "default").unwrap();
            let proj = super::ops::get_or_create_project(&mut conn, &ws, "engram", None).unwrap();
            let page_id = super::ops::upsert_page(
                &mut conn,
                &sample_page(ws, proj, "notes/v103.md", "v1.0.3 upgrade fixture"),
            )
            .unwrap();
            super::ops::begin_session(
                &mut conn,
                &NewSession {
                    id: session_id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                },
            )
            .unwrap();
            super::ops::insert_observation(
                &mut conn,
                &NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "upgrade observation survives".into(),
                    importance: 5,
                },
            )
            .unwrap();
            super::ops::end_session(&mut conn, &session_id, Some(&page_id)).unwrap();
        }

        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();

        assert_eq!(
            store
                .reader
                .latest_completed_session_for_project(ws, proj)
                .await
                .unwrap(),
            Some(session_id)
        );
        assert_eq!(
            store
                .reader
                .search_observations_for_project(ws, proj, "upgrade".into(), 10)
                .await
                .unwrap()
                .len(),
            1
        );
        let page = store
            .reader
            .page_body_by_ids(ws, proj, "notes/v103.md")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.body, "v1.0.3 upgrade fixture");

        store
            .writer
            .ensure_auto_improve_scheduler_state(ws, proj)
            .await
            .unwrap();
        assert!(
            store
                .reader
                .auto_improve_candidate_sessions(ws, proj, 0, 10)
                .await
                .unwrap()
                .is_empty(),
            "v1.0.3-era completed sessions must become the first-run watermark, not backlog"
        );

        let conn = Connection::open(store.db_path()).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0);
        for table in [
            "auto_improve_runs",
            "auto_improve_proposals",
            "auto_improve_proposal_events",
            "auto_improve_scheduler_state",
            "auto_improve_scheduler_claims",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "{table} should exist after v1.0.3 upgrade");
        }
    }

    #[tokio::test]
    async fn list_projects_with_stats_returns_aggregates() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "my-project", None)
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "a.md", "alpha"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "b.md", "beta"))
            .await
            .unwrap();

        let summaries = store.reader.list_projects_with_stats().await.unwrap();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.workspace_name, "default");
        assert_eq!(s.project_name, "my-project");
        assert_eq!(s.page_count, 2);
        assert!(s.last_updated.is_some());
    }

    #[tokio::test]
    async fn list_pages_returns_latest_pages_for_project() {
        let tmp = TempDir::new().unwrap();
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
        store
            .writer
            .upsert_page(sample_page(ws, proj, "x.md", "body x"))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "y.md", "body y"))
            .await
            .unwrap();
        // Supersede x.md — should still appear once (latest version).
        store
            .writer
            .upsert_page(sample_page(ws, proj, "x.md", "body x updated"))
            .await
            .unwrap();

        let pages = store.reader.list_pages("default", "scratch").await.unwrap();
        assert_eq!(pages.len(), 2, "only is_latest=1 pages");
        let paths: Vec<&str> = pages.iter().map(|p| p.path.as_str()).collect();
        assert!(paths.contains(&"x.md"));
        assert!(paths.contains(&"y.md"));
    }

    #[tokio::test]
    async fn page_meta_returns_metadata_for_existing_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "meta-test", None)
            .await
            .unwrap();
        let page = NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("decisions/d1.md").unwrap(),
            title: "Decision One".into(),
            body: "content here".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"kind": "decision"}),
            pinned: true,
            links: Vec::new(),
            author_id: None,
        };
        store.writer.upsert_page(page).await.unwrap();

        let meta = store
            .reader
            .page_meta("default", "meta-test", "decisions/d1.md")
            .await
            .unwrap();
        let meta = meta.expect("page_meta should return Some for existing page");
        assert_eq!(meta.workspace_name, "default");
        assert_eq!(meta.project_name, "meta-test");
        assert_eq!(meta.path, "decisions/d1.md");
        assert_eq!(meta.title, "Decision One");
        assert_eq!(meta.kind, "decision");
        assert!(meta.pinned);

        // Non-existent page returns None.
        let none = store
            .reader
            .page_meta("default", "meta-test", "no-such.md")
            .await
            .unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn delete_stale_page_embeddings_removes_mismatched_rows() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "test", None)
            .await
            .unwrap();
        let p1 = store
            .writer
            .upsert_page(sample_page(ws, proj, "a.md", "body a"))
            .await
            .unwrap();
        let p2 = store
            .writer
            .upsert_page(sample_page(ws, proj, "b.md", "body b"))
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                p1,
                vec![vec![0u8; 4]],
                "google".into(),
                "models/gemini-embedding-001".into(),
                768,
            )
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                p2,
                vec![vec![1u8; 4]],
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        let n = store
            .writer
            .delete_stale_page_embeddings(
                ws,
                Some(proj),
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let mismatch = store
            .reader
            .embedding_meta_for_mismatch(
                "openai".into(),
                "openai/text-embedding-3-small".into(),
                1536,
            )
            .await
            .unwrap();
        assert!(mismatch.is_empty());
    }

    /// Chunk vectors for the max-pooling fixtures. `many` holds five
    /// mediocre chunks (cosine 0.5 against the query below), `best`
    /// one strong chunk (0.9).
    ///
    /// This shape is what makes the max-pooling assertions bite. RRF
    /// fuses on page id, so per-chunk duplicates collapse either way —
    /// but WITHOUT max-pooling each chunk contributes its own
    /// `1/(k+rank)` term, and five mediocre chunks (Σ ≈ 0.078) out-score
    /// one strong chunk (≈ 0.016). Pooling to one score per page is the
    /// only thing that puts `best` on top.
    const MAXPOOL_QUERY: [f32; 4] = [1.0, 0.0, 0.0, 0.0];

    fn maxpool_many_chunks() -> Vec<Vec<u8>> {
        (0..5)
            .map(|_| f32_vec_to_bytes(&[0.5, 0.866, 0.0, 0.0]))
            .collect()
    }

    fn maxpool_best_chunk() -> Vec<Vec<u8>> {
        vec![f32_vec_to_bytes(&[0.9, 0.436, 0.0, 0.0])]
    }

    /// The hybrid vector leg max-pools chunk scores: a page ranks by its
    /// best-matching chunk, not by how many chunks it happens to have,
    /// and appears exactly once.
    #[tokio::test]
    async fn hybrid_search_max_pools_chunk_scores() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        let many = store
            .writer
            .upsert_page(sample_page(ws, proj, "many.md", "many chunks"))
            .await
            .unwrap();
        let best = store
            .writer
            .upsert_page(sample_page(ws, proj, "best.md", "one strong chunk"))
            .await
            .unwrap();
        for (id, vectors) in [(many, maxpool_many_chunks()), (best, maxpool_best_chunk())] {
            store
                .writer
                .store_embedding(id, vectors, "test".into(), "model".into(), 4)
                .await
                .unwrap();
        }

        // FTS misses on purpose; only the vector leg ranks.
        let hits = store
            .reader
            .hybrid_search(
                ws,
                proj,
                "zzzunmatchable".into(),
                Some(MAXPOOL_QUERY.to_vec()),
                "test".into(),
                "model".into(),
                4,
                10,
            )
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths.first(),
            Some(&"best.md"),
            "chunk count must not inflate a page's rank: {paths:?}"
        );
        assert_eq!(
            paths.iter().filter(|p| **p == "many.md").count(),
            1,
            "a multi-chunk page must appear once, not once per chunk: {paths:?}"
        );
    }

    /// The global vector leg max-pools the same way, across workspaces,
    /// and keeps each hit's workspace/project annotation.
    #[tokio::test]
    async fn hybrid_search_global_max_pools_chunk_scores() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws_a = store.writer.get_or_create_workspace("alpha").await.unwrap();
        let ws_b = store.writer.get_or_create_workspace("beta").await.unwrap();
        let proj_a = store
            .writer
            .get_or_create_project(ws_a, "one", None)
            .await
            .unwrap();
        let proj_b = store
            .writer
            .get_or_create_project(ws_b, "two", None)
            .await
            .unwrap();

        let many = store
            .writer
            .upsert_page(sample_page(ws_a, proj_a, "many.md", "many chunks"))
            .await
            .unwrap();
        let best = store
            .writer
            .upsert_page(sample_page(ws_b, proj_b, "best.md", "one strong chunk"))
            .await
            .unwrap();
        for (id, vectors) in [(many, maxpool_many_chunks()), (best, maxpool_best_chunk())] {
            store
                .writer
                .store_embedding(id, vectors, "test".into(), "model".into(), 4)
                .await
                .unwrap();
        }

        let hits = store
            .reader
            .hybrid_search_global(
                "zzzunmatchable".into(),
                Some(MAXPOOL_QUERY.to_vec()),
                "test".into(),
                "model".into(),
                4,
                10,
            )
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths.first(),
            Some(&"best.md"),
            "chunk count must not inflate a page's global rank: {paths:?}"
        );
        assert_eq!(
            paths.iter().filter(|p| **p == "many.md").count(),
            1,
            "multi-chunk page must appear once globally: {paths:?}"
        );
        // Cross-workspace annotation survives the max-pool grouping.
        let many_hit = hits.iter().find(|h| h.path.as_str() == "many.md").unwrap();
        assert_eq!(many_hit.workspace_name, "alpha");
        assert_eq!(many_hit.project_name, "one");
    }

    /// `embedded_pages` answers "how much of the wiki is embedded" in
    /// pages, so chunk rows must not inflate it, and rows left behind
    /// on superseded page versions must not count either.
    #[tokio::test]
    async fn derived_status_counts_embedded_pages_not_chunk_rows() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();

        let chunked = store
            .writer
            .upsert_page(sample_page(ws, proj, "long.md", "long body"))
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                chunked,
                vec![f32_vec_to_bytes(&[1.0]), f32_vec_to_bytes(&[0.5])],
                "test".into(),
                "model".into(),
                1,
            )
            .await
            .unwrap();

        // Superseded version keeps its row; a fresh id replaces it as latest.
        let old = store
            .writer
            .upsert_page(sample_page(ws, proj, "edited.md", "first body"))
            .await
            .unwrap();
        store
            .writer
            .store_embedding(
                old,
                vec![f32_vec_to_bytes(&[1.0])],
                "test".into(),
                "model".into(),
                1,
            )
            .await
            .unwrap();
        store
            .writer
            .upsert_page(sample_page(ws, proj, "edited.md", "second body"))
            .await
            .unwrap();

        // A latest page with no embedding at all.
        store
            .writer
            .upsert_page(sample_page(ws, proj, "bare.md", "bare body"))
            .await
            .unwrap();

        let derived = store.reader.derived_index_status().await.unwrap();
        assert_eq!(
            derived.embedded_pages, 1,
            "only long.md is a latest page with vectors (2 chunk rows)"
        );
        assert_eq!(
            derived.embedding_rows, 3,
            "rows still count chunks plus the superseded page's row"
        );
        assert_eq!(
            derived.latest_pages_missing_embeddings, 2,
            "edited.md (new version) and bare.md"
        );
    }

    /// Backfill skip predicate: a long page carrying only a legacy
    /// head-truncated single vector is NOT "fully embedded", while a
    /// short single-vector page and an already-chunked long page are.
    #[tokio::test]
    async fn fully_embedded_page_ids_flags_legacy_truncated_long_pages() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "engram", None)
            .await
            .unwrap();
        let budget = 100u64;
        let long_body = "x".repeat(200);

        let short = store
            .writer
            .upsert_page(sample_page(ws, proj, "short.md", "short body"))
            .await
            .unwrap();
        let legacy = store
            .writer
            .upsert_page(sample_page(ws, proj, "legacy.md", &long_body))
            .await
            .unwrap();
        let chunked = store
            .writer
            .upsert_page(sample_page(ws, proj, "chunked.md", &long_body))
            .await
            .unwrap();

        let one = vec![f32_vec_to_bytes(&[1.0])];
        let two = vec![f32_vec_to_bytes(&[1.0]), f32_vec_to_bytes(&[0.5])];
        for (id, vectors) in [(short, one.clone()), (legacy, one), (chunked, two)] {
            store
                .writer
                .store_embedding(id, vectors, "test".into(), "model".into(), 1)
                .await
                .unwrap();
        }

        let done = store
            .reader
            .fully_embedded_page_ids(ws, proj, "test".into(), "model".into(), 1, budget)
            .await
            .unwrap();
        assert!(done.contains(&short), "short single-vector page is done");
        assert!(done.contains(&chunked), "already-chunked page is done");
        assert!(
            !done.contains(&legacy),
            "long page with one legacy vector must be re-embedded"
        );
    }
}
