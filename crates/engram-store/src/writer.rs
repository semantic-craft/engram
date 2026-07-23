//! Single-writer SQLite actor.
//!
//! Every mutating SQL statement flows through one dedicated OS thread that
//! owns the writer [`rusqlite::Connection`]. Callers send [`WriteCmd`]
//! variants over an mpsc channel and receive results back through a
//! `oneshot`. This pattern eliminates the `database is locked` failure
//! mode that bit cognee (#2717) — there is exactly one writer at all
//! times, by construction.

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use engram_core::{
    AgentKind, HandoffId, NewHandoff, NewObservation, NewPage, NewSession, NewUser, ObservationId,
    PageId, PagePath, ProjectId, SessionId, UserId, WorkspaceId,
};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::auto_improve::{
    ApproveAutoImproveProposal, ApproveAutoImproveProposalResult, FailAutoImproveProposal,
    RejectAutoImproveProposal, StageAutoImproveRun, StagedAutoImproveRun,
};
use crate::error::{StoreError, StoreResult};
use crate::ops::{self, EmbeddingWrite, MoveSummary, PurgeSummary, ReorgSummary};
use crate::users::{self, TOKEN_HASH_LEN};

/// Commands accepted by the writer thread.
pub(crate) enum WriteCmd {
    GetOrCreateWorkspace {
        name: String,
        reply: oneshot::Sender<StoreResult<WorkspaceId>>,
    },
    GetOrCreateProject {
        workspace_id: WorkspaceId,
        name: String,
        repo_path: Option<String>,
        reply: oneshot::Sender<StoreResult<ProjectId>>,
    },
    EnsureProjectWorkspace {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EnsureWorkspaceWithId {
        id: WorkspaceId,
        name: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EnsureProjectWithId {
        id: ProjectId,
        workspace_id: WorkspaceId,
        name: String,
        repo_path: Option<String>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    UpsertPage {
        page: NewPage,
        reply: oneshot::Sender<StoreResult<PageId>>,
    },
    UpsertPageBatch {
        pages: Vec<NewPage>,
        reply: oneshot::Sender<StoreResult<Vec<PageId>>>,
    },
    DeletePage {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    BeginSession {
        session: NewSession,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EndSession {
        session_id: SessionId,
        summary_page_id: Option<PageId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    SweepHollowProjects {
        min_age_days: u32,
        reply: oneshot::Sender<StoreResult<Vec<String>>>,
    },
    InsertObservation {
        obs: NewObservation,
        reply: oneshot::Sender<StoreResult<ObservationId>>,
    },
    InsertHandoff {
        handoff: NewHandoff,
        reply: oneshot::Sender<StoreResult<HandoffId>>,
    },
    AcceptHandoff {
        handoff_id: HandoffId,
        accepting_agent: AgentKind,
        accepting_session: Option<SessionId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CancelHandoff {
        handoff_id: HandoffId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    /// Retro-fit sessions + observations to per-cwd projects and graveyard
    /// mash-up pages. Executed in one transaction for atomicity.
    Reorg {
        /// Workspace whose legacy mash-up pages and sessions are being reorged.
        workspace_id: WorkspaceId,
        /// Each entry is `(session_id, new_project_id)`.
        plan: Vec<(SessionId, ProjectId)>,
        reply: oneshot::Sender<StoreResult<ReorgSummary>>,
    },
    BumpAccess {
        page_ids: Vec<PageId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    SoftDeleteForDecay {
        page_ids: Vec<PageId>,
        reply: oneshot::Sender<StoreResult<usize>>,
    },
    HardDeleteDecayed {
        hard_delete_after_days: i64,
        reply: oneshot::Sender<StoreResult<usize>>,
    },
    HealCatchAllRepoPaths {
        home: Option<String>,
        reply: oneshot::Sender<StoreResult<u64>>,
    },
    StoreEmbedding {
        page_id: PageId,
        vectors: Vec<Vec<u8>>,
        provider: String,
        model: String,
        dim: u32,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    StoreEmbeddingBatch {
        embeddings: Vec<EmbeddingWrite>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    DeleteStalePageEmbeddings {
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
        provider: String,
        model: String,
        dim: u32,
        reply: oneshot::Sender<StoreResult<u64>>,
    },
    /// Delete a project and all its data (pages, sessions, observations,
    /// handoffs, embeddings) in one transaction. Returns the paths of
    /// every page file that must be removed from disk by the caller.
    PurgeProject {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        /// Human-readable `workspace/project` label forwarded into the summary.
        label: String,
        reply: oneshot::Sender<StoreResult<PurgeSummary>>,
    },
    /// Re-stamp a project's `workspace_id` across every domain table in one
    /// transaction, keeping the same `project_id` (a lossless cross-workspace
    /// "true move"). The caller renames the on-disk dir and ensures the
    /// destination workspace row exists first.
    MoveProjectWorkspace {
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
        reply: oneshot::Sender<StoreResult<MoveSummary>>,
    },
    /// Rename a project's `name` column without moving any files (the wiki
    /// is flat on disk). Fails with [`crate::error::StoreError::ProjectNameTaken`]
    /// when `new_name` is already used in the same workspace.
    RenameProject {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        new_name: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    /// Record a successfully-applied wiki-structure migration.
    InsertWikiMigration {
        name: String,
        /// Unix microseconds UTC.
        applied_at: i64,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CreateUser {
        new_user: NewUser,
        token_hash: [u8; TOKEN_HASH_LEN],
        reply: oneshot::Sender<StoreResult<UserId>>,
    },
    RotateUserToken {
        user_id: UserId,
        new_token_hash: [u8; TOKEN_HASH_LEN],
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    ExpireUserToken {
        user_id: UserId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    ReviveUserToken {
        user_id: UserId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    TouchUserLastSeen {
        user_id: UserId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    StageAutoImproveRun {
        input: StageAutoImproveRun,
        reply: oneshot::Sender<StoreResult<StagedAutoImproveRun>>,
    },
    RejectAutoImproveProposal {
        input: RejectAutoImproveProposal,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    FailAutoImproveProposal {
        input: FailAutoImproveProposal,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    ApproveAutoImproveProposal {
        input: ApproveAutoImproveProposal,
        reply: oneshot::Sender<StoreResult<ApproveAutoImproveProposalResult>>,
    },
    EnsureAutoImproveSchedulerState {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    ClaimAutoImproveSchedulerSession {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        ended_at: i64,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    Shutdown,
}

/// Cheap, cloneable handle that submits commands to the writer.
#[derive(Clone)]
pub struct WriterHandle {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    tx: mpsc::Sender<WriteCmd>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl WriterHandle {
    /// Take ownership of `conn` and spawn the writer thread.
    pub(crate) fn spawn(conn: Connection) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let handle = thread::Builder::new()
            .name("engram-writer".into())
            .spawn(move || worker_loop(conn, rx))
            .expect("spawn writer thread");

        Self {
            inner: Arc::new(WriterInner {
                tx,
                join: Mutex::new(Some(handle)),
            }),
        }
    }

    /// Resolve a workspace by name, creating it atomically if missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::get_or_create_workspace`].
    pub async fn get_or_create_workspace(
        &self,
        name: impl Into<String>,
    ) -> StoreResult<WorkspaceId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::GetOrCreateWorkspace {
            name: name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Resolve a project by `(workspace_id, name)`, creating it atomically
    /// if missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::get_or_create_project`].
    pub async fn get_or_create_project(
        &self,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
        repo_path: Option<String>,
    ) -> StoreResult<ProjectId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::GetOrCreateProject {
            workspace_id,
            name: name.into(),
            repo_path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Assert that a project still belongs to the supplied workspace.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] when the `(workspace_id, project_id)`
    /// pair is stale, [`StoreError::WriterClosed`] if the actor has shut down,
    /// or propagates the SQL error.
    pub async fn ensure_project_workspace(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureProjectWorkspace {
            workspace_id,
            project_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a workspace with an **explicit id** (idempotent). Used by
    /// `reindex` to recreate a scope under the id recovered from the wiki
    /// directory name. See [`ops::ensure_workspace_with_id`].
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn ensure_workspace_with_id(
        &self,
        id: WorkspaceId,
        name: impl Into<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureWorkspaceWithId {
            id,
            name: name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a project with an **explicit id** under `workspace_id`
    /// (idempotent). See [`ops::ensure_project_with_id`].
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn ensure_project_with_id(
        &self,
        id: ProjectId,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
        repo_path: Option<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureProjectWithId {
            id,
            workspace_id,
            name: name.into(),
            repo_path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Begin a session (idempotent on the supplied id).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn begin_session(&self, session: NewSession) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::BeginSession { session, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a session ended, optionally linking its summary page.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn end_session(
        &self,
        session_id: SessionId,
        summary_page_id: Option<PageId>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndSession {
            session_id,
            summary_page_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete hollow project rows (no data of any kind) older than
    /// `min_age_days`; returns the deleted names. See
    /// [`ops::sweep_hollow_projects`].
    ///
    /// # Errors
    /// Propagates store failures.
    pub async fn sweep_hollow_projects(&self, min_age_days: u32) -> StoreResult<Vec<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::SweepHollowProjects {
            min_age_days,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Append an observation row.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn insert_observation(&self, obs: NewObservation) -> StoreResult<ObservationId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertObservation { obs, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a new handoff in `open` state.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn insert_handoff(&self, handoff: NewHandoff) -> StoreResult<HandoffId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertHandoff { handoff, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a handoff accepted by the given agent / session.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn accept_handoff(
        &self,
        handoff_id: HandoffId,
        accepting_agent: AgentKind,
        accepting_session: Option<SessionId>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::AcceptHandoff {
            handoff_id,
            accepting_agent,
            accepting_session,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark an open handoff expired so it will no longer be consumed.
    ///
    /// Returns `true` when an open handoff was changed, `false` when the id was
    /// already accepted/expired or missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn cancel_handoff(&self, handoff_id: HandoffId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CancelHandoff {
            handoff_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Store (or replace) the embedding chunk set for one page (M9).
    /// One entry in `vectors` per document chunk, in document order.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn store_embedding(
        &self,
        page_id: PageId,
        vectors: Vec<Vec<u8>>,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StoreEmbedding {
            page_id,
            vectors,
            provider,
            model,
            dim,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Store or replace a batch of embeddings in one SQLite transaction.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn store_embeddings(&self, embeddings: Vec<EmbeddingWrite>) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StoreEmbeddingBatch {
            embeddings,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Remove embedding rows in a workspace/project scope whose triple does not match the configured provider/model/dim.
    ///
    /// Used when re-embedding after a model migration (e.g. Gemini → OpenRouter).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn delete_stale_page_embeddings(
        &self,
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::DeleteStalePageEmbeddings {
            workspace_id,
            project_id,
            provider,
            model,
            dim,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Bump access counters for a set of pages (M8 reinforcement term).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn bump_access(&self, page_ids: Vec<PageId>) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::BumpAccess {
            page_ids,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Soft-delete pages identified by the M8 forget sweep.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn soft_delete_for_decay(&self, page_ids: Vec<PageId>) -> StoreResult<usize> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::SoftDeleteForDecay {
            page_ids,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Hard-delete pages soft-deleted by the sweep more than
    /// `hard_delete_after_days` ago.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn hard_delete_decayed(&self, hard_delete_after_days: i64) -> StoreResult<usize> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::HardDeleteDecayed {
            hard_delete_after_days,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// NULL out catch-all project `repo_path` rows so existing installs
    /// self-heal on upgrade: the broad sentinels (`$HOME` and the filesystem
    /// root) plus any path that exists locally but is not a git work-tree
    /// root. Idempotent; returns the number of rows healed.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn heal_catch_all_repo_paths(&self, home: Option<String>) -> StoreResult<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::HealCatchAllRepoPaths { home, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete a project and all its data in one atomic transaction.
    ///
    /// ON DELETE CASCADE propagates the delete through pages, sessions,
    /// observations, handoffs, and page_embeddings automatically. The
    /// returned [`PurgeSummary`] includes pre-delete row counts and
    /// the distinct page paths that the caller must remove from disk.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from the purge transaction.
    pub async fn purge_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        label: impl Into<String>,
    ) -> StoreResult<PurgeSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::PurgeProject {
            workspace_id,
            project_id,
            label: label.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Re-stamp a project's `workspace_id` to `to_workspace` across every
    /// domain table in one transaction, keeping the same `project_id`. This is
    /// the lossless cross-workspace move: pages, sessions, observations and
    /// supersession history all follow. The destination workspace row must
    /// already exist; the caller renames the on-disk dir afterwards.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down,
    /// [`StoreError::NotFound`] if the project is absent from the source
    /// workspace, or propagates the SQL error (e.g. a
    /// `UNIQUE(workspace_id, name)` collision when the destination already
    /// holds a same-named project — the caller routes that merge case to
    /// copy+purge instead).
    pub async fn move_project_workspace(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
    ) -> StoreResult<MoveSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::MoveProjectWorkspace {
            project_id,
            from_workspace,
            to_workspace,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Rename a project within its workspace (column-only; no file moves).
    ///
    /// # Errors
    /// Returns [`crate::error::StoreError::WriterClosed`] if the actor has
    /// shut down, [`crate::error::StoreError::ProjectNameTaken`] if
    /// `new_name` is already in use in the same workspace, or
    /// [`crate::error::StoreError::InvalidProjectName`] for invalid names.
    pub async fn rename_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        new_name: impl Into<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RenameProject {
            workspace_id,
            project_id,
            new_name: new_name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Record a wiki-structure migration as successfully applied.
    ///
    /// Called by the wiki migration runner immediately after [`WikiMigration::up`]
    /// returns `Ok`. `applied_at` is unix microseconds UTC. If the name is
    /// already present the call is a no-op (idempotent insert-or-ignore).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error.
    pub async fn insert_wiki_migration(&self, name: String, applied_at: i64) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertWikiMigration {
            name,
            applied_at,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Retro-fit sessions and their observations to per-cwd projects and
    /// graveyard any mash-up pages. The `plan` slice contains
    /// `(session_id, new_project_id)` pairs. Everything runs in one
    /// SQLite transaction — either fully committed or fully rolled back.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from the reorg transaction.
    pub async fn reorg_sessions(
        &self,
        workspace_id: WorkspaceId,
        plan: Vec<(SessionId, ProjectId)>,
    ) -> StoreResult<ReorgSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::Reorg {
            workspace_id,
            plan,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Upsert a batch of pages atomically (one SQL transaction).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down,
    /// or propagates the SQL error from [`ops::upsert_pages_batch`].
    pub async fn upsert_pages_batch(&self, pages: Vec<NewPage>) -> StoreResult<Vec<PageId>> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertPageBatch { pages, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Upsert a page (creating it or superseding the existing latest
    /// version when the body has changed).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::upsert_page`].
    pub async fn upsert_page(&self, page: NewPage) -> StoreResult<PageId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertPage { page, reply: tx }).await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete every version of a page (by path) from the index. The wiki file
    /// removal is the caller's concern; this drops the derived rows so the
    /// page stops appearing in search/recent (the watcher does NOT reconcile
    /// file deletions — it only handles create/modify events).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or a
    /// SQL error from the delete.
    pub async fn delete_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::DeletePage {
            workspace_id,
            project_id,
            path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a new user. `new_user` MUST already have been validated by
    /// [`NewUser::validate`](engram_core::NewUser::validate); the
    /// caller (CLI or admin handler) generates the plaintext token,
    /// hashes it with the per-server pepper via
    /// [`crate::users::hash_token`], and passes only the digest in.
    ///
    /// # Errors
    /// - [`StoreError::WriterClosed`] if the writer has shut down.
    /// - [`StoreError::Duplicate`] when the username or email collides.
    /// - [`StoreError::Sqlite`] for any other SQL failure.
    pub async fn create_user(
        &self,
        new_user: NewUser,
        token_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<UserId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CreateUser {
            new_user,
            token_hash,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Replace a user's token hash with a freshly-generated digest.
    /// Implicitly clears `token_expired_at` — rotating an expired
    /// token only makes sense to make it usable again. Returns `false`
    /// when the user id doesn't exist; `true` on successful update.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`]
    /// per the usual writer-actor flow.
    pub async fn rotate_user_token(
        &self,
        user_id: UserId,
        new_token_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RotateUserToken {
            user_id,
            new_token_hash,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Stamp `token_expired_at = now()` so the user's current token stops
    /// authenticating. Idempotent — repeating the call leaves the original
    /// expiry timestamp untouched. Returns `false` when the user doesn't exist.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`].
    pub async fn expire_user_token(&self, user_id: UserId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ExpireUserToken { user_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Clear `token_expired_at`, re-activating the user's existing token.
    /// Idempotent. Returns `false` when the user doesn't exist.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`].
    pub async fn revive_user_token(&self, user_id: UserId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ReviveUserToken { user_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Update `last_seen_at = now()` for the user. Called fire-and-forget
    /// by the auth middleware on every authenticated request. Returns
    /// `false` when the user doesn't exist (caller authenticated against
    /// a token whose user vanished mid-request — already a logic error
    /// elsewhere).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`].
    pub async fn touch_user_last_seen(&self, user_id: UserId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::TouchUserLastSeen { user_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Stage one auto-improvement run and all proposals in one SQL transaction.
    pub async fn stage_auto_improve_run(
        &self,
        input: StageAutoImproveRun,
    ) -> StoreResult<StagedAutoImproveRun> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StageAutoImproveRun { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Reject a pending auto-improvement proposal and append an event.
    pub async fn reject_auto_improve_proposal(
        &self,
        input: RejectAutoImproveProposal,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RejectAutoImproveProposal { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a pending auto-improvement proposal failed and append an event.
    pub async fn fail_auto_improve_proposal(
        &self,
        input: FailAutoImproveProposal,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::FailAutoImproveProposal { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Approve a pending auto-improvement proposal in a DB-only transaction.
    pub async fn approve_auto_improve_proposal(
        &self,
        input: ApproveAutoImproveProposal,
    ) -> StoreResult<ApproveAutoImproveProposalResult> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ApproveAutoImproveProposal { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Initialise background auto-improvement scheduler state without resetting
    /// an existing watermark.
    pub async fn ensure_auto_improve_scheduler_state(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureAutoImproveSchedulerState {
            workspace_id,
            project_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically claim a completed session for scheduled auto-improvement
    /// before any LLM work runs. Returns `false` if another scheduler/manual run
    /// already claimed or reviewed the session.
    pub async fn claim_auto_improve_scheduler_session(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        ended_at: i64,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ClaimAutoImproveSchedulerSession {
            workspace_id,
            project_id,
            session_id,
            ended_at,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    async fn send(&self, cmd: WriteCmd) -> StoreResult<()> {
        self.inner
            .tx
            .send(cmd)
            .await
            .map_err(|_| StoreError::WriterClosed)
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WriteCmd::Shutdown);
        if let Some(handle) = self.join.lock().expect("writer join mutex poisoned").take() {
            let _ = handle.join();
        }
    }
}

/// Dispatch one operation's result back to its caller. Logs a warn
/// when the receiver was dropped (caller cancelled their `.await` or
/// hit a timeout) so the operator sees backpressure / cancellation
/// noise instead of silent loss. The result itself is consumed by
/// the failed `send` and discarded — the caller's await has already
/// returned a JoinError-shaped failure by this point.
fn send_or_warn<T>(reply: oneshot::Sender<T>, result: T, op: &'static str) {
    if reply.send(result).is_err() {
        tracing::warn!(
            op,
            "writer reply dropped — caller cancelled or oneshot receiver closed"
        );
    }
}

fn worker_loop(mut conn: Connection, mut rx: mpsc::Receiver<WriteCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::Shutdown => break,
            WriteCmd::GetOrCreateWorkspace { name, reply } => {
                let result = ops::get_or_create_workspace(&mut conn, &name);
                send_or_warn(reply, result, "get_or_create_workspace");
            }
            WriteCmd::GetOrCreateProject {
                workspace_id,
                name,
                repo_path,
                reply,
            } => {
                let result = ops::get_or_create_project(
                    &mut conn,
                    &workspace_id,
                    &name,
                    repo_path.as_deref(),
                );
                send_or_warn(reply, result, "get_or_create_project");
            }
            WriteCmd::EnsureProjectWorkspace {
                workspace_id,
                project_id,
                reply,
            } => {
                let result = ops::ensure_project_workspace(&conn, &workspace_id, &project_id);
                send_or_warn(reply, result, "ensure_project_workspace");
            }
            WriteCmd::EnsureWorkspaceWithId { id, name, reply } => {
                let result = ops::ensure_workspace_with_id(&mut conn, id, &name);
                send_or_warn(reply, result, "ensure_workspace_with_id");
            }
            WriteCmd::EnsureProjectWithId {
                id,
                workspace_id,
                name,
                repo_path,
                reply,
            } => {
                let result = ops::ensure_project_with_id(
                    &mut conn,
                    id,
                    workspace_id,
                    &name,
                    repo_path.as_deref(),
                );
                send_or_warn(reply, result, "ensure_project_with_id");
            }
            WriteCmd::UpsertPage { page, reply } => {
                let result = ops::upsert_page(&mut conn, &page);
                send_or_warn(reply, result, "upsert_page");
            }
            WriteCmd::DeletePage {
                workspace_id,
                project_id,
                path,
                reply,
            } => {
                let result = ops::delete_page(&conn, workspace_id, project_id, &path);
                send_or_warn(reply, result, "delete_page");
            }
            WriteCmd::UpsertPageBatch { pages, reply } => {
                let result = ops::upsert_pages_batch(&mut conn, &pages);
                send_or_warn(reply, result, "upsert_pages_batch");
            }
            WriteCmd::BeginSession { session, reply } => {
                let result = ops::begin_session(&mut conn, &session);
                send_or_warn(reply, result, "begin_session");
            }
            WriteCmd::EndSession {
                session_id,
                summary_page_id,
                reply,
            } => {
                let result = ops::end_session(&mut conn, &session_id, summary_page_id.as_ref());
                send_or_warn(reply, result, "end_session");
            }
            WriteCmd::SweepHollowProjects {
                min_age_days,
                reply,
            } => {
                let result = ops::sweep_hollow_projects(&mut conn, min_age_days);
                send_or_warn(reply, result, "sweep_hollow_projects");
            }
            WriteCmd::InsertObservation { obs, reply } => {
                let result = ops::insert_observation(&mut conn, &obs);
                send_or_warn(reply, result, "insert_observation");
            }
            WriteCmd::InsertHandoff { handoff, reply } => {
                let result = ops::insert_handoff(&mut conn, &handoff);
                send_or_warn(reply, result, "insert_handoff");
            }
            WriteCmd::AcceptHandoff {
                handoff_id,
                accepting_agent,
                accepting_session,
                reply,
            } => {
                let result = ops::accept_handoff(
                    &mut conn,
                    &handoff_id,
                    accepting_agent,
                    accepting_session.as_ref(),
                );
                send_or_warn(reply, result, "accept_handoff");
            }
            WriteCmd::CancelHandoff { handoff_id, reply } => {
                let result = ops::cancel_handoff(&mut conn, &handoff_id);
                send_or_warn(reply, result, "cancel_handoff");
            }
            WriteCmd::Reorg {
                workspace_id,
                plan,
                reply,
            } => {
                let result = ops::reorg_sessions(&mut conn, &workspace_id, &plan);
                send_or_warn(reply, result, "reorg_sessions");
            }
            WriteCmd::BumpAccess { page_ids, reply } => {
                let result = ops::bump_access_for_pages(&mut conn, &page_ids);
                send_or_warn(reply, result, "bump_access_for_pages");
            }
            WriteCmd::SoftDeleteForDecay { page_ids, reply } => {
                let result = ops::soft_delete_for_decay(&mut conn, &page_ids);
                send_or_warn(reply, result, "soft_delete_for_decay");
            }
            WriteCmd::HardDeleteDecayed {
                hard_delete_after_days,
                reply,
            } => {
                let result = ops::hard_delete_decayed_pages(&mut conn, hard_delete_after_days);
                send_or_warn(reply, result, "hard_delete_decayed_pages");
            }
            WriteCmd::HealCatchAllRepoPaths { home, reply } => {
                let result = ops::heal_catch_all_repo_paths(&mut conn, home.as_deref());
                send_or_warn(reply, result, "heal_catch_all_repo_paths");
            }
            WriteCmd::StoreEmbedding {
                page_id,
                vectors,
                provider,
                model,
                dim,
                reply,
            } => {
                let result =
                    ops::store_embedding(&mut conn, &page_id, &vectors, &provider, &model, dim);
                send_or_warn(reply, result, "store_embedding");
            }
            WriteCmd::StoreEmbeddingBatch { embeddings, reply } => {
                let result = ops::store_embeddings(&mut conn, &embeddings);
                send_or_warn(reply, result, "store_embeddings");
            }
            WriteCmd::DeleteStalePageEmbeddings {
                workspace_id,
                project_id,
                provider,
                model,
                dim,
                reply,
            } => {
                let result = ops::delete_stale_page_embeddings(
                    &mut conn,
                    &workspace_id,
                    project_id.as_ref(),
                    &provider,
                    &model,
                    dim,
                );
                send_or_warn(reply, result, "delete_stale_page_embeddings");
            }
            WriteCmd::PurgeProject {
                workspace_id,
                project_id,
                label,
                reply,
            } => {
                let result = ops::purge_project(&mut conn, &workspace_id, &project_id, &label);
                send_or_warn(reply, result, "purge_project");
            }
            WriteCmd::MoveProjectWorkspace {
                project_id,
                from_workspace,
                to_workspace,
                reply,
            } => {
                let result = ops::move_project_workspace(
                    &mut conn,
                    &project_id,
                    &from_workspace,
                    &to_workspace,
                );
                send_or_warn(reply, result, "move_project_workspace");
            }
            WriteCmd::RenameProject {
                workspace_id,
                project_id,
                new_name,
                reply,
            } => {
                let result = ops::rename_project(&mut conn, &workspace_id, &project_id, &new_name);
                send_or_warn(reply, result, "rename_project");
            }
            WriteCmd::InsertWikiMigration {
                name,
                applied_at,
                reply,
            } => {
                let result = ops::insert_wiki_migration(&mut conn, &name, applied_at);
                send_or_warn(reply, result, "insert_wiki_migration");
            }
            WriteCmd::CreateUser {
                new_user,
                token_hash,
                reply,
            } => {
                let result = users::insert_user(&conn, &new_user, &token_hash);
                send_or_warn(reply, result, "create_user");
            }
            WriteCmd::RotateUserToken {
                user_id,
                new_token_hash,
                reply,
            } => {
                let result = users::rotate_user_token(&conn, user_id, &new_token_hash);
                send_or_warn(reply, result, "rotate_user_token");
            }
            WriteCmd::ExpireUserToken { user_id, reply } => {
                let result = users::expire_user_token(&conn, user_id);
                send_or_warn(reply, result, "expire_user_token");
            }
            WriteCmd::ReviveUserToken { user_id, reply } => {
                let result = users::revive_user_token(&conn, user_id);
                send_or_warn(reply, result, "revive_user_token");
            }
            WriteCmd::TouchUserLastSeen { user_id, reply } => {
                let result = users::touch_user_last_seen(&conn, user_id);
                send_or_warn(reply, result, "touch_user_last_seen");
            }
            WriteCmd::StageAutoImproveRun { input, reply } => {
                let result = crate::auto_improve::stage_run(&mut conn, &input);
                send_or_warn(reply, result, "stage_auto_improve_run");
            }
            WriteCmd::RejectAutoImproveProposal { input, reply } => {
                let result = crate::auto_improve::reject_proposal(&mut conn, &input);
                send_or_warn(reply, result, "reject_auto_improve_proposal");
            }
            WriteCmd::FailAutoImproveProposal { input, reply } => {
                let result = crate::auto_improve::fail_proposal(&mut conn, &input);
                send_or_warn(reply, result, "fail_auto_improve_proposal");
            }
            WriteCmd::ApproveAutoImproveProposal { input, reply } => {
                let result = crate::auto_improve::approve_proposal(&mut conn, &input);
                send_or_warn(reply, result, "approve_auto_improve_proposal");
            }
            WriteCmd::EnsureAutoImproveSchedulerState {
                workspace_id,
                project_id,
                reply,
            } => {
                let result = crate::auto_improve::ensure_scheduler_state(
                    &mut conn,
                    workspace_id,
                    project_id,
                );
                send_or_warn(reply, result, "ensure_auto_improve_scheduler_state");
            }
            WriteCmd::ClaimAutoImproveSchedulerSession {
                workspace_id,
                project_id,
                session_id,
                ended_at,
                reply,
            } => {
                let result = crate::auto_improve::claim_scheduler_session(
                    &mut conn,
                    workspace_id,
                    project_id,
                    session_id,
                    ended_at,
                );
                send_or_warn(reply, result, "claim_auto_improve_scheduler_session");
            }
        }
    }
    tracing::debug!("writer thread exiting cleanly");
}
