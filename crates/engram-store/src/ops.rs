//! Mutating SQL operations executed on the writer thread.
//!
//! Each operation is one transaction. Calling them from anywhere other than
//! the writer thread would violate the single-writer invariant (see
//! [`crate::writer`]).

use std::collections::BTreeSet;

use engram_core::{
    AgentKind, HandoffId, LinkTarget, NewHandoff, NewObservation, NewPage, NewSession,
    ObservationId, ObservationKind, PageId, PagePath, ProjectId, SessionId, WorkspaceId,
};

/// Summary returned by [`reorg_sessions`] and exposed via
/// [`crate::writer::WriterHandle::reorg_sessions`].
#[derive(Debug, Default, Clone)]
pub struct ReorgSummary {
    /// Sessions whose `project_id` was changed.
    pub sessions_moved: usize,
    /// Observations updated to match their session's new project.
    pub observations_updated: usize,
    /// `is_latest=1` pages marked `is_latest=0` (mash-up graveyard).
    pub pages_graveyarded: usize,
}

/// Summary returned by [`purge_project`] and exposed via
/// [`crate::writer::WriterHandle::purge_project`].
#[derive(Debug, Default, Clone)]
pub struct PurgeSummary {
    /// Human-readable `workspace/project` label. Set by the caller (writer
    /// only knows IDs); filled in by [`purge_project`] from its parameters.
    pub label: String,
    /// Distinct page paths that were present before the delete (all versions,
    /// not just `is_latest=1`). The admin handler uses this list to remove
    /// the corresponding files from the wiki directory.
    pub page_paths: Vec<String>,
    /// Number of `pages` rows deleted (all versions, not just latest).
    pub pages_deleted: u64,
    /// Number of `sessions` rows deleted.
    pub sessions_deleted: u64,
    /// Number of `observations` rows deleted.
    pub observations_deleted: u64,
    /// Number of `handoffs` rows deleted.
    pub handoffs_deleted: u64,
    /// Number of `page_embeddings` rows deleted (cascades through pages).
    pub embeddings_deleted: u64,
}
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::{StoreError, StoreResult};

/// One embedding upsert requested by a backfill or embed command.
#[derive(Debug)]
pub struct EmbeddingWrite {
    /// Page receiving the embedding.
    pub page_id: PageId,
    /// Packed little-endian `f32` vector bytes, one entry per document
    /// chunk in document order (the index is the stored `chunk_index`).
    pub vectors: Vec<Vec<u8>>,
    /// Embedding provider name.
    pub provider: String,
    /// Embedding model name.
    pub model: String,
    /// Vector dimension.
    pub dim: u32,
}

/// Upsert a page by path, superseding any existing latest version when the
/// content (sha256 of body) has changed.
///
/// Returns the id of the page row that should now be considered current.
pub fn upsert_page(conn: &mut Connection, page: &NewPage) -> StoreResult<PageId> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let result_id = upsert_page_in_tx(&tx, page, now)?;
    tx.commit()?;
    Ok(result_id)
}

/// Resolve a workspace by name, creating it if missing. Atomic.
pub fn get_or_create_workspace(
    conn: &mut Connection,
    name: &str,
) -> StoreResult<engram_core::WorkspaceId> {
    let tx = conn.transaction()?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM workspaces WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(bytes) = existing {
        engram_core::WorkspaceId::from_slice(&bytes)?
    } else {
        let id = engram_core::WorkspaceId::new();
        tx.execute(
            "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
        )?;
        id
    };
    tx.commit()?;
    Ok(id)
}

/// Resolve a project by `(workspace_id, name)`, creating it if missing.
/// Atomic.
pub fn get_or_create_project(
    conn: &mut Connection,
    workspace_id: &engram_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<engram_core::ProjectId> {
    let repo_path = repo_path.map(normalize_repo_path_key);
    let tx = conn.transaction()?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
            params![workspace_id.as_bytes(), name],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(bytes) = existing {
        engram_core::ProjectId::from_slice(&bytes)?
    } else {
        let id = engram_core::ProjectId::new();
        tx.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id.as_bytes(),
                workspace_id.as_bytes(),
                name,
                repo_path.as_deref(),
                Timestamp::now().as_microsecond()
            ],
        )?;
        id
    };
    tx.commit()?;
    if scheduler_state_table_exists(conn)? {
        crate::auto_improve::ensure_scheduler_state(conn, *workspace_id, id)?;
    }
    Ok(id)
}

/// Delete "hollow" project rows: zero pages (any version), zero sessions,
/// zero observations, zero handoffs, zero auto-improve runs/proposals/
/// rejections, and older than `min_age_days`. (The per-project
/// scheduler-state row is bookkeeping created for every project and does
/// not count as data.) These
/// are pure bookkeeping noise left behind by probes, renames, and failed
/// first events — nothing exists to lose, which is what makes this safe to
/// run on a schedule (the operator-driven `purge-project` covers everything
/// that actually holds data). Reserved projects (`scratch`, the cwd-less
/// fallback; `_global`, the preferences scope) are exempt even when empty.
/// Returns the deleted names for logging.
///
/// # Errors
/// Propagates SQLite failures.
pub fn sweep_hollow_projects(conn: &mut Connection, min_age_days: u32) -> StoreResult<Vec<String>> {
    let cutoff =
        Timestamp::now().as_microsecond() - i64::from(min_age_days) * 24 * 60 * 60 * 1_000_000;
    let tx = conn.transaction()?;
    let names: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT name FROM projects
             WHERE name NOT IN ('scratch', ?1)
               AND created_at < ?2
               AND NOT EXISTS (SELECT 1 FROM pages        WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM sessions     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM observations WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM handoffs     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_runs      WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_proposals WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_rejections WHERE project_id = projects.id)",
        )?;
        let rows = stmt.query_map(params![engram_core::GLOBAL_SCOPE_PROJECT, cutoff], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<_, _>>()?
    };
    if !names.is_empty() {
        tx.execute(
            "DELETE FROM projects
             WHERE name NOT IN ('scratch', ?1)
               AND created_at < ?2
               AND NOT EXISTS (SELECT 1 FROM pages        WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM sessions     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM observations WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM handoffs     WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_runs      WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_proposals WHERE project_id = projects.id)
               AND NOT EXISTS (SELECT 1 FROM auto_improve_rejections WHERE project_id = projects.id)",
            params![engram_core::GLOBAL_SCOPE_PROJECT, cutoff],
        )?;
    }
    tx.commit()?;
    Ok(names)
}

/// NULL out `repo_path` values that act as longest-prefix-match catch-alls
/// (issue #103), so every project nested beneath them stops resolving to the
/// wrong row after upgrade. A one-shot startup heal; idempotent (a healed row
/// is NULL and drops out of the candidate set, so a second pass heals 0).
/// Returns the number of rows healed.
///
/// A row is healed when:
/// - `repo_path` is one of the two broad sentinels -- filesystem root (`/`)
///   or the operator's home directory (`home`, when provided). These are
///   healed even if they happen to be git work-tree roots (e.g. a dotfiles
///   repo checked out at `$HOME`): as prefix keys they swallow everything
///   beneath them.
/// - otherwise, `repo_path` EXISTS on this host but is NOT a git work-tree
///   root (e.g. a bare `~/projects` cwd the original corruption captured).
///
/// A `repo_path` that does NOT exist on this host is left untouched: under a
/// remote/multi-user daemon it may be a client path for another user, or a
/// temporarily unmounted drive, and destroying it would wipe a valid prefix
/// key. This safety rule is mandatory.
pub fn heal_catch_all_repo_paths(conn: &mut Connection, home: Option<&str>) -> StoreResult<u64> {
    let home = home.map(normalize_repo_path_key);
    let candidates: Vec<(Vec<u8>, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, repo_path FROM projects WHERE repo_path IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let to_null: Vec<Vec<u8>> = candidates
        .into_iter()
        .filter(|(_, repo_path)| should_heal_repo_path(repo_path, home.as_deref()))
        .map(|(id, _)| id)
        .collect();
    let tx = conn.transaction()?;
    for id in &to_null {
        tx.execute(
            "UPDATE projects SET repo_path = NULL WHERE id = ?1",
            params![id],
        )?;
    }
    tx.commit()?;
    Ok(u64::try_from(to_null.len()).unwrap_or(0))
}

/// Decide whether a non-NULL `repo_path` is a prefix-match catch-all that
/// should be NULLed. See [`heal_catch_all_repo_paths`] for the full rule.
fn should_heal_repo_path(repo_path: &str, home: Option<&str>) -> bool {
    let repo_path_key = normalize_repo_path_key(repo_path);
    if repo_path_key == "/" || home == Some(repo_path_key.as_str()) {
        return true; // broad sentinels, healed even if they look like git roots
    }
    let p = std::path::Path::new(repo_path);
    // Non-existent paths (and stat errors) are left alone: multi-user/unmounted
    // safety. An existing path is a catch-all only when its `.git` is
    // definitively absent (a normal repo has a `.git` dir, a worktree/submodule
    // a `.git` file); a `.git` stat error preserves the row, same as the
    // path-existence check above.
    matches!(p.try_exists(), Ok(true)) && matches!(p.join(".git").try_exists(), Ok(false))
}

fn normalize_repo_path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

fn scheduler_state_table_exists(conn: &Connection) -> StoreResult<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'auto_improve_scheduler_state'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Insert a workspace with an **explicit id**, idempotent. Unlike
/// [`get_or_create_workspace`] (which mints a fresh id), this preserves the id
/// the caller already holds — used by `reindex`, which recovers the id from the
/// wiki directory name so the rebuilt index keys pages by the same
/// `(workspace_id, project_id)` the on-disk tree is laid out under. Re-running
/// is a no-op (`ON CONFLICT(id)`). `created_at` is the rebuild time.
pub fn ensure_workspace_with_id(
    conn: &mut Connection,
    id: engram_core::WorkspaceId,
    name: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(id) DO NOTHING",
        params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
    )?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        Some(existing) if existing == name => Ok(()),
        Some(existing) => Err(StoreError::Duplicate(format!(
            "workspace id {id} already exists as name '{existing}', not manifest name '{name}'"
        ))),
        None => Err(StoreError::NotFound(format!(
            "workspace id {id} was not inserted"
        ))),
    }?;
    Ok(())
}

/// Insert a project with an **explicit id** under `workspace_id`, idempotent.
/// The reindex counterpart of [`ensure_workspace_with_id`].
pub fn ensure_project_with_id(
    conn: &mut Connection,
    id: engram_core::ProjectId,
    workspace_id: engram_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<()> {
    let repo_path = repo_path.map(normalize_repo_path_key);
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO NOTHING",
        params![
            id.as_bytes(),
            workspace_id.as_bytes(),
            name,
            repo_path.as_deref(),
            Timestamp::now().as_microsecond()
        ],
    )?;
    type ProjectRow = (Vec<u8>, String, Option<String>);
    let existing: Option<ProjectRow> = conn
        .query_row(
            "SELECT workspace_id, name, repo_path FROM projects WHERE id = ?1",
            params![id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match existing {
        Some((existing_ws, existing_name, existing_repo_path))
            if existing_ws.as_slice() == workspace_id.as_bytes()
                && existing_name == name
                && existing_repo_path.as_deref() == repo_path.as_deref() =>
        {
            Ok(())
        }
        Some((existing_ws, existing_name, existing_repo_path)) => {
            Err(StoreError::Duplicate(format!(
                "project id {id} already exists with workspace_id bytes length {}, name='{existing_name}', repo_path={existing_repo_path:?}; manifest has workspace={workspace_id}, name='{name}', repo_path={repo_path:?}",
                existing_ws.len(),
            )))
        }
        None => Err(StoreError::NotFound(format!(
            "project id {id} was not inserted"
        ))),
    }?;
    Ok(())
}

/// Assert that `project_id` currently belongs to `workspace_id`.
///
/// Wiki writes call this before touching the filesystem so a stale hook/cache
/// carrying the old workspace for a moved project fails before it can create an
/// orphan file. The pairing INSERT triggers are still the final SQL backstop.
pub fn ensure_project_workspace(
    conn: &Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
) -> StoreResult<()> {
    let found = conn
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1 AND workspace_id = ?2",
            params![project_id.as_bytes(), workspace_id.as_bytes()],
            |_| Ok(()),
        )
        .optional()?;
    if found.is_some() {
        Ok(())
    } else {
        Err(StoreError::NotFound(format!(
            "project {project_id} does not belong to workspace {workspace_id}"
        )))
    }
}

/// Upsert a batch of pages inside one transaction. Either *all* pages
/// land (each becoming the new `is_latest=true` version) or none do.
///
/// This is the M7b atomic-fan-out path: the consolidator can hand a
/// list of {sessions, concepts, decisions} pages and trust that
/// either the whole batch supersedes or the wiki is unchanged.
pub fn upsert_pages_batch(conn: &mut Connection, pages: &[NewPage]) -> StoreResult<Vec<PageId>> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(pages.len());
    for page in pages {
        let id = upsert_page_in_tx(&tx, page, now)?;
        out.push(id);
    }
    tx.commit()?;
    Ok(out)
}

struct ExistingPageVersion {
    id: Vec<u8>,
    body_sha256: Vec<u8>,
    frontmatter_json: String,
    title: String,
    tier: String,
    pinned: i64,
}

/// Normalise a page path into FTS-friendly search text, indexing BOTH forms
/// so either a whole-slug or a single-word query hits:
/// - segments: `/` and `.` → space, KEEPING `-`/`_` (FTS token chars) so the
///   full hyphenated slug stays one token (`foo-bar` matches a `"foo-bar"`
///   query);
/// - words: also split `-`/`_` so each word is its own token (`bar` matches).
///
/// `notes/foo-bar.md` → `notes foo-bar md notes foo bar md`.
///
/// MUST stay byte-identical to the backfill expression in migration V17 so
/// the `rebuild` and live-write paths index the same text (matching bm25
/// term frequencies, not just the same match set).
pub(crate) fn path_search_text(path: &str) -> String {
    let segments = path.replace(['/', '.'], " ");
    let words = segments.replace(['-', '_'], " ");
    format!("{segments} {words}")
}

pub(crate) fn upsert_page_in_tx(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    now: i64,
) -> StoreResult<PageId> {
    let path_search = path_search_text(page.path.as_str());
    let body_sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(page.body.as_bytes());
        hasher.finalize().into()
    };
    let frontmatter_str = serde_json::to_string(&page.frontmatter_json)?;
    let tier_str = page.tier.as_str();

    let existing: Option<ExistingPageVersion> = tx
        .query_row(
            "SELECT id, body_sha256, frontmatter_json, title, tier, pinned FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
            ],
            |row| {
                Ok(ExistingPageVersion {
                    id: row.get(0)?,
                    body_sha256: row.get(1)?,
                    frontmatter_json: row.get(2)?,
                    title: row.get(3)?,
                    tier: row.get(4)?,
                    pinned: row.get(5)?,
                })
            },
        )
        .optional()?;

    if let Some(existing) = existing {
        if existing.body_sha256 == body_sha256
            && existing.frontmatter_json == frontmatter_str
            && existing.title == page.title
            && existing.tier == tier_str
            && existing.pinned == i64::from(page.pinned)
        {
            return PageId::from_slice(&existing.id).map_err(StoreError::from);
        }
        let new_id = PageId::new();
        tx.execute(
            "UPDATE pages SET is_latest = 0 WHERE id = ?1",
            params![&existing.id],
        )?;
        tx.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, supersedes, pinned, author_id, \
              created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?14, ?14)",
            params![
                new_id.as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
                path_search,
                page.title,
                tier_str,
                page.body,
                body_sha256.as_slice(),
                frontmatter_str,
                &existing.id,
                i64::from(page.pinned),
                page.author_id.map(|id| id.as_bytes().to_vec()),
                now,
            ],
        )?;
        replace_links_in_tx(tx, &new_id, page)?;
        refresh_incoming_links_for_path(tx, page, &new_id)?;
        audit(
            tx,
            "supersede_page",
            Some(page.workspace_id.as_bytes()),
            Some(page.project_id.as_bytes()),
            Some(new_id.as_bytes()),
            page.author_id.as_ref().map(engram_core::UserId::as_bytes),
            now,
        )?;
        return Ok(new_id);
    }
    let new_id = PageId::new();
    tx.execute(
        "INSERT INTO pages \
         (id, workspace_id, project_id, path, path_search, title, tier, body, body_sha256, \
          frontmatter_json, is_latest, pinned, author_id, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13, ?13)",
        params![
            new_id.as_bytes(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
            page.path.as_str(),
            path_search,
            page.title,
            tier_str,
            page.body,
            body_sha256.as_slice(),
            frontmatter_str,
            i64::from(page.pinned),
            page.author_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    replace_links_in_tx(tx, &new_id, page)?;
    refresh_incoming_links_for_path(tx, page, &new_id)?;
    audit(
        tx,
        "create_page",
        Some(page.workspace_id.as_bytes()),
        Some(page.project_id.as_bytes()),
        Some(new_id.as_bytes()),
        page.author_id.as_ref().map(engram_core::UserId::as_bytes),
        now,
    )?;
    Ok(new_id)
}

fn replace_links_in_tx(
    tx: &rusqlite::Transaction<'_>,
    from_page_id: &PageId,
    page: &NewPage,
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM links WHERE from_page_id = ?1",
        params![from_page_id.as_bytes()],
    )?;

    let mut seen = BTreeSet::new();
    for link in &page.links {
        let key = (
            link.workspace.clone(),
            link.project.clone(),
            link.path.as_str().to_string(),
        );
        if !seen.insert(key) {
            continue;
        }
        let to_page_id = latest_page_id_for_link(tx, page, link)?;
        let to_page_blob = to_page_id.as_ref().map(|id| &id.as_bytes()[..]);
        tx.execute(
            "INSERT INTO links \
                 (from_page_id, to_page_id, to_workspace, to_project, to_path, link_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'references')",
            params![
                from_page_id.as_bytes(),
                to_page_blob,
                link.workspace,
                link.project,
                link.path.as_str(),
            ],
        )?;
    }
    Ok(())
}

/// Resolve a link target to the latest page id it points at, or `None` if the
/// target workspace / project / page does not exist yet (an unresolved forward
/// link). A bare link resolves within the source page's own project; a
/// `[[project:path]]` / `[[workspace/project:path]]` link resolves against the
/// named project (same workspace when only the project is given).
fn latest_page_id_for_link(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    link: &LinkTarget,
) -> StoreResult<Option<PageId>> {
    let (workspace_blob, project_blob): (Vec<u8>, Vec<u8>) = match &link.project {
        None => (
            page.workspace_id.as_bytes().to_vec(),
            page.project_id.as_bytes().to_vec(),
        ),
        Some(project_name) => {
            let workspace_blob: Vec<u8> = match &link.workspace {
                None => page.workspace_id.as_bytes().to_vec(),
                Some(workspace_name) => {
                    let found: Option<Vec<u8>> = tx
                        .query_row(
                            "SELECT id FROM workspaces WHERE name = ?1",
                            params![workspace_name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match found {
                        Some(id) => id,
                        None => return Ok(None),
                    }
                }
            };
            let project_blob: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT id FROM projects WHERE workspace_id = ?1 AND name = ?2",
                    params![workspace_blob, project_name],
                    |row| row.get(0),
                )
                .optional()?;
            match project_blob {
                Some(id) => (workspace_blob, id),
                None => return Ok(None),
            }
        }
    };

    let bytes: Option<Vec<u8>> = tx
        .query_row(
            "SELECT id FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![workspace_blob, project_blob, link.path.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    bytes
        .map(|bytes| PageId::from_slice(&bytes).map_err(StoreError::from))
        .transpose()
}

fn refresh_incoming_links_for_path(
    tx: &rusqlite::Transaction<'_>,
    page: &NewPage,
    latest_page_id: &PageId,
) -> StoreResult<()> {
    // (1) Bare (same-project) links: from_page lives in this page's project and
    // the target carries no scope. Repoints all matches (not only unresolved):
    // a new page version changes the latest id, so resolved links must follow.
    tx.execute(
        "UPDATE links \
         SET to_page_id = ?1 \
         WHERE to_project IS NULL AND to_path = ?2 \
           AND EXISTS ( \
               SELECT 1 FROM pages from_page \
               WHERE from_page.id = links.from_page_id \
                 AND from_page.workspace_id = ?3 \
                 AND from_page.project_id = ?4 \
           )",
        params![
            latest_page_id.as_bytes(),
            page.path.as_str(),
            page.workspace_id.as_bytes(),
            page.project_id.as_bytes(),
        ],
    )?;

    // (2) Cross-project links naming this page's project by name. `to_workspace`
    // may be explicit (cross-workspace) or NULL (same workspace as the source).
    let project_name: Option<String> = tx
        .query_row(
            "SELECT name FROM projects WHERE id = ?1",
            params![page.project_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    let workspace_name: Option<String> = tx
        .query_row(
            "SELECT name FROM workspaces WHERE id = ?1",
            params![page.workspace_id.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    if let (Some(project_name), Some(workspace_name)) = (project_name, workspace_name) {
        tx.execute(
            "UPDATE links \
             SET to_page_id = ?1 \
             WHERE to_project = ?2 AND to_path = ?3 \
               AND ( \
                   to_workspace = ?4 \
                   OR ( \
                       to_workspace IS NULL \
                       AND EXISTS ( \
                           SELECT 1 FROM pages from_page \
                           WHERE from_page.id = links.from_page_id \
                             AND from_page.workspace_id = ?5 \
                       ) \
                   ) \
               )",
            params![
                latest_page_id.as_bytes(),
                project_name,
                page.path.as_str(),
                workspace_name,
                page.workspace_id.as_bytes(),
            ],
        )?;
    }
    Ok(())
}

/// Begin (or re-affirm) a session row keyed on the caller-supplied id.
/// Idempotent: a second call with the same id leaves the row untouched.
pub fn begin_session(conn: &mut Connection, session: &NewSession) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let agent = session.agent_kind.as_str();
    let cwd: Option<String> = session
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(id) DO NOTHING",
        params![
            session.id.as_bytes(),
            session.workspace_id.as_bytes(),
            session.project_id.as_bytes(),
            agent,
            cwd,
            now,
        ],
    )?;
    Ok(())
}

/// Stamp a session as ended, optionally linking the synthesised summary
/// page.
pub fn end_session(
    conn: &mut Connection,
    session_id: &SessionId,
    summary_page_id: Option<&PageId>,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let page_blob: Option<&[u8]> = summary_page_id.map(|p| &p.as_bytes()[..]);
    conn.execute(
        "UPDATE sessions SET ended_at = ?1, summary_page_id = ?2 WHERE id = ?3",
        params![now, page_blob, session_id.as_bytes()],
    )?;
    Ok(())
}

/// Append a single observation. Caller is expected to have already
/// inserted the parent session via [`begin_session`].
pub fn insert_observation(
    conn: &mut Connection,
    obs: &NewObservation,
) -> StoreResult<ObservationId> {
    let id = ObservationId::new();
    let now = Timestamp::now().as_microsecond();
    let kind = observation_kind_as_str(obs.kind);
    let importance: i64 = i64::from(obs.importance.clamp(1, 10));
    let (extension, source_event) = match (&obs.extension, &obs.source_event) {
        (Some(extension), Some(source_event)) => {
            (Some(extension.as_str()), Some(source_event.as_str()))
        }
        _ => (None, None),
    };
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, extension, source_event, title, body, \
          importance, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.as_bytes(),
            obs.session_id.as_bytes(),
            obs.workspace_id.as_bytes(),
            obs.project_id.as_bytes(),
            kind,
            extension,
            source_event,
            obs.title,
            obs.body,
            importance,
            now,
        ],
    )?;
    Ok(id)
}

/// Store / replace one page's embedding chunk set. Each entry in
/// `vectors` is the little-endian `f32` packing of one chunk's
/// unit-normalised vector; the slice index becomes `chunk_index`.
/// Existing rows for the page are deleted first, so a page shrinking
/// from N chunks to M < N leaves no stale tail rows.
/// Provider/model/dim are denormalised onto each row so a single
/// SELECT can detect heterogeneity (refuse-on-mismatch path).
pub fn store_embedding(
    conn: &mut Connection,
    page_id: &PageId,
    vectors: &[Vec<u8>],
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    replace_page_embedding_rows(&tx, page_id, vectors, provider, model, dim, now)?;
    tx.commit()?;
    Ok(())
}

/// Store / replace a batch of page embeddings in one transaction.
pub fn store_embeddings(conn: &mut Connection, embeddings: &[EmbeddingWrite]) -> StoreResult<()> {
    if embeddings.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    for embedding in embeddings {
        replace_page_embedding_rows(
            &tx,
            &embedding.page_id,
            &embedding.vectors,
            &embedding.provider,
            &embedding.model,
            embedding.dim,
            now,
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Delete + insert one page's chunk rows inside the caller's transaction.
fn replace_page_embedding_rows(
    tx: &rusqlite::Transaction<'_>,
    page_id: &PageId,
    vectors: &[Vec<u8>],
    provider: &str,
    model: &str,
    dim: u32,
    now: i64,
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM page_embeddings WHERE page_id = ?1",
        params![page_id.as_bytes()],
    )?;
    let mut stmt = tx.prepare_cached(
        "INSERT INTO page_embeddings \
             (page_id, chunk_index, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for (chunk_index, vector_bytes) in vectors.iter().enumerate() {
        stmt.execute(params![
            page_id.as_bytes(),
            chunk_index as i64,
            vector_bytes.as_slice(),
            provider,
            model,
            dim,
            now,
        ])?;
    }
    Ok(())
}

/// Bump `access_count` + `last_accessed_at` for the pages whose ids
/// appear in `page_ids`. Idempotent for unknown ids (no-op).
/// Used by the read path to feed the M8 reinforcement term.
pub fn bump_access_for_pages(conn: &mut Connection, page_ids: &[PageId]) -> StoreResult<()> {
    if page_ids.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE pages \
             SET access_count = access_count + 1, last_accessed_at = ?1 \
             WHERE id = ?2 AND is_latest = 1",
        )?;
        for id in page_ids {
            stmt.execute(params![now, id.as_bytes()])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Mark a set of `is_latest=1` pages as soft-deleted by the forget
/// sweep. Distinguished from M7 supersession by `supersedes IS NULL`.
pub fn soft_delete_for_decay(conn: &mut Connection, page_ids: &[PageId]) -> StoreResult<usize> {
    if page_ids.is_empty() {
        return Ok(0);
    }
    let now = Timestamp::now().as_microsecond();
    let mut affected = 0usize;
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE pages \
             SET is_latest = 0, superseded_at = ?1 \
             WHERE id = ?2 AND is_latest = 1",
        )?;
        for id in page_ids {
            affected += stmt.execute(params![now, id.as_bytes()])?;
        }
    }
    audit(
        &tx,
        "soft_delete_for_decay",
        None,
        None,
        None,
        // Decay sweep is a system op (scheduled / admin-triggered) — no
        // user-attributable actor at the row level.
        None,
        Timestamp::now().as_microsecond(),
    )?;
    tx.commit()?;
    Ok(affected)
}

/// Delete every version of a page (by path) from the index. Used when the
/// wiki file is removed (`Wiki::delete_page`): the watcher does not handle
/// file deletions, so the derived rows must be dropped explicitly or the
/// page keeps surfacing in search/recent with stale content. FK cascades
/// drop outgoing links + embeddings; the `pages_fts_ad` trigger keeps FTS in
/// sync; incoming links are set to NULL (unresolved). Idempotent.
pub fn delete_page(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    path: &PagePath,
) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM pages WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            path.as_str()
        ],
    )?;
    Ok(())
}

/// Hard-delete rows that were soft-deleted by an earlier sweep at
/// least `hard_delete_after_days` ago AND received zero subsequent
/// accesses. Safe: M7 supersedes-chain pages have a non-null
/// `supersedes` so they never match.
pub fn hard_delete_decayed_pages(
    conn: &mut Connection,
    hard_delete_after_days: i64,
) -> StoreResult<usize> {
    let cutoff = Timestamp::now().as_microsecond() - hard_delete_after_days * 86_400_000_000;
    let n = conn.execute(
        "DELETE FROM pages \
         WHERE is_latest = 0 \
           AND supersedes IS NULL \
           AND superseded_at IS NOT NULL \
           AND superseded_at < ?1 \
           AND access_count = 0",
        params![cutoff],
    )?;
    Ok(n)
}

/// Insert a new handoff in state=open.
pub fn insert_handoff(conn: &mut Connection, h: &NewHandoff) -> StoreResult<HandoffId> {
    let id = HandoffId::new();
    let now = Timestamp::now().as_microsecond();
    let open_q = serde_json::to_string(&h.open_questions)?;
    let next_s = serde_json::to_string(&h.next_steps)?;
    let files = serde_json::to_string(&h.files_touched)?;
    let from_session: Option<&[u8]> = h.from_session_id.as_ref().map(|s| &s.as_bytes()[..]);
    // Normalize the stored cwd: strip trailing path separators (keep a bare root
    // as "/"). The hook extractor preserves whatever the agent payload sent,
    // so this single write point guarantees a consistent stored form for both
    // manual and auto (SessionEnd) handoffs, keeping the next session's
    // path-boundary match robust to trailing slash/backslash drift.
    let cwd: Option<String> = h.cwd.as_ref().map(|p| {
        let s = p.to_string_lossy();
        let trimmed = s.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    });
    let from_agent = h.from_agent.as_str();
    let to_agent = h.to_agent.map(AgentKind::as_str);
    conn.execute(
        "INSERT INTO handoffs \
         (id, workspace_id, project_id, from_session_id, from_agent, to_agent, cwd, summary, \
          open_questions, next_steps, files_touched, state, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'open', ?12)",
        params![
            id.as_bytes(),
            h.workspace_id.as_bytes(),
            h.project_id.as_bytes(),
            from_session,
            from_agent,
            to_agent,
            cwd,
            h.summary,
            open_q,
            next_s,
            files,
            now,
        ],
    )?;
    Ok(id)
}

/// Mark a handoff accepted by `accepting_agent` / `accepting_session`.
pub fn accept_handoff(
    conn: &mut Connection,
    handoff_id: &HandoffId,
    accepting_agent: AgentKind,
    accepting_session: Option<&SessionId>,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let agent = accepting_agent.as_str();
    let session: Option<&[u8]> = accepting_session.map(|s| &s.as_bytes()[..]);
    conn.execute(
        "UPDATE handoffs SET state = 'accepted', accepted_by = ?1, accepted_at = ?2, \
         accepted_by_session = ?3 \
         WHERE id = ?4 AND state = 'open'",
        params![agent, now, session, handoff_id.as_bytes()],
    )?;
    Ok(())
}

/// Mark an open handoff expired so it will no longer be consumed.
pub fn cancel_handoff(conn: &mut Connection, handoff_id: &HandoffId) -> StoreResult<bool> {
    let changed = conn.execute(
        "UPDATE handoffs SET state = 'expired' WHERE id = ?1 AND state = 'open'",
        params![handoff_id.as_bytes()],
    )?;
    Ok(changed > 0)
}

fn observation_kind_as_str(kind: ObservationKind) -> &'static str {
    kind.as_str()
}

fn audit(
    tx: &rusqlite::Transaction<'_>,
    op: &str,
    workspace_id: Option<&[u8; 16]>,
    project_id: Option<&[u8; 16]>,
    page_id: Option<&[u8; 16]>,
    author_id: Option<&[u8; 16]>,
    at: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO audit_log (at, op, workspace_id, project_id, page_id, author_id, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, '{}')",
        params![
            at,
            op,
            workspace_id.map(|b| &b[..]),
            project_id.map(|b| &b[..]),
            page_id.map(|b| &b[..]),
            author_id.map(|b| &b[..]),
        ],
    )?;
    Ok(())
}

/// Retro-fit sessions + observations to per-cwd projects and graveyard
/// any `is_latest=1` pages (which are mash-ups across the old single-project
/// bucket). Executes atomically in one transaction.
///
/// `plan` contains `(session_id, new_project_id)` pairs. Sessions not in
/// the plan are left untouched. Pages are graveyarded unconditionally so a
/// fresh consolidation can regenerate clean per-project pages.
pub fn reorg_sessions(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    plan: &[(SessionId, ProjectId)],
) -> StoreResult<ReorgSummary> {
    if plan.is_empty() {
        return Ok(ReorgSummary::default());
    }
    let tx = conn.transaction()?;
    let mut sessions_moved = 0usize;
    let mut observations_updated = 0usize;
    for (session_id, new_project_id) in plan {
        let rows = tx.execute(
            "UPDATE sessions
             SET project_id = ?1
             WHERE id = ?2 AND workspace_id = ?3 AND project_id != ?1",
            params![
                new_project_id.as_bytes(),
                session_id.as_bytes(),
                workspace_id.as_bytes()
            ],
        )?;
        sessions_moved += rows;
        // Update observations whose session_id matches, keeping project_id
        // in sync with the session row we just moved.
        let obs_rows = tx.execute(
            "UPDATE observations SET project_id = ?1 WHERE session_id = ?2 AND workspace_id = ?3",
            params![
                new_project_id.as_bytes(),
                session_id.as_bytes(),
                workspace_id.as_bytes()
            ],
        )?;
        observations_updated += obs_rows;
    }
    // Graveyard only this workspace's latest pages; sibling workspaces may
    // have already-consolidated pages that must remain current.
    let pages_graveyarded: usize = tx.execute(
        "UPDATE pages SET is_latest = 0 WHERE workspace_id = ?1 AND is_latest = 1",
        params![workspace_id.as_bytes()],
    )?;
    tx.commit()?;
    Ok(ReorgSummary {
        sessions_moved,
        observations_updated,
        pages_graveyarded,
    })
}

/// Rename a project within its workspace.
///
/// Only the `name` column is updated — all pages, sessions, observations,
/// and handoffs remain associated with the same `project_id`. No files
/// move on disk (the wiki is flat: every page from every project lives
/// under `wiki/`; only the `project_id` foreign key distinguishes them).
///
/// # Errors
/// - [`StoreError::InvalidProjectName`] when `new_name` is empty,
///   contains a `/` character, or is all whitespace.
/// - [`StoreError::ProjectNameTaken`] when a project with `new_name`
///   already exists in the same workspace.
/// - [`StoreError::Sqlite`] on any other SQL failure.
pub fn rename_project(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    new_name: &str,
) -> StoreResult<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidProjectName(
            "project name must not be empty or all whitespace".into(),
        ));
    }
    if trimmed.contains('/') {
        return Err(StoreError::InvalidProjectName(
            "project name must not contain '/' (it appears in URL paths)".into(),
        ));
    }

    let rows = conn.execute(
        "UPDATE projects SET name = ?1 WHERE id = ?2 AND workspace_id = ?3",
        params![trimmed, project_id.as_bytes(), workspace_id.as_bytes()],
    );

    match rows {
        // Zero rows affected means the project row vanished between the
        // admin handler's `lookup_ws_proj_no_create` and this UPDATE —
        // the classic shape is a concurrent `purge-project` racing the
        // rename. Without this check, the rename would happily return
        // `Ok(())` and the admin handler would respond `200 OK` for an
        // operation that touched nothing, contradicting the purge's
        // (also `200 OK`) destruction of the same row.
        Ok(0) => Err(StoreError::NotFound(format!(
            "project id {project_id} no longer exists in workspace {workspace_id} \
             (race with concurrent purge or delete)",
        ))),
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(StoreError::ProjectNameTaken(trimmed.to_string()))
        }
        Err(e) => Err(StoreError::Sqlite(e)),
    }
}

/// Record a successfully-applied wiki-structure migration.
///
/// Uses `INSERT OR IGNORE` so re-running the same name is a no-op
/// (idempotent by design — the runner already skips known names, but
/// this guards against any concurrent writes).
pub fn insert_wiki_migration(
    conn: &mut Connection,
    name: &str,
    applied_at: i64,
) -> StoreResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO wiki_migrations (name, applied_at) VALUES (?1, ?2)",
        params![name, applied_at],
    )?;
    Ok(())
}

/// Delete a project and all its data inside one transaction.
///
/// Execution order:
/// 1. Count rows in each dependent table (pages/all versions, sessions,
///    observations, handoffs, embeddings) before the delete so we can
///    report how many rows were removed.
/// 2. Collect all distinct page paths stored under the project — these are
///    the on-disk files the caller must clean up after this function returns.
/// 3. DELETE FROM projects WHERE id = ? — the ON DELETE CASCADE clauses in
///    V01 + V02 propagate the delete to pages, sessions, observations,
///    handoffs, and page_embeddings automatically.
/// 4. Commit and return the [`PurgeSummary`].
///
/// The `workspace_project_label` string is passed in by the caller (the
/// admin handler has the human-readable names; the writer only has IDs) and
/// forwarded verbatim into [`PurgeSummary::label`] for logging.
///
/// # Errors
/// Returns [`StoreError`] if any SQL statement fails. The transaction is
/// rolled back automatically on error.
pub fn purge_project(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: &ProjectId,
    workspace_project_label: &str,
) -> StoreResult<PurgeSummary> {
    let tx = conn.transaction()?;

    let count = |sql: &str, id: &[u8]| -> StoreResult<u64> {
        let n: Option<i64> = tx
            .query_row(sql, rusqlite::params![id], |row| row.get(0))
            .optional()?;
        Ok(u64::try_from(n.unwrap_or(0)).unwrap_or(0))
    };

    let pid = project_id.as_bytes();
    let pages_deleted = count("SELECT COUNT(*) FROM pages WHERE project_id = ?1", &pid[..])?;
    let sessions_deleted = count(
        "SELECT COUNT(*) FROM sessions WHERE project_id = ?1",
        &pid[..],
    )?;
    let observations_deleted = count(
        "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
        &pid[..],
    )?;
    let handoffs_deleted = count(
        "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
        &pid[..],
    )?;
    // page_embeddings cascade through pages; count pages that have them.
    // DISTINCT because a page holds one row per document chunk — the
    // report renders this next to page/session counts, so it must stay
    // page-granular.
    let embeddings_deleted = count(
        "SELECT COUNT(DISTINCT page_id) FROM page_embeddings \
         WHERE page_id IN (SELECT id FROM pages WHERE project_id = ?1)",
        &pid[..],
    )?;

    // Collect all distinct on-disk paths for the caller to clean up.
    // We use DISTINCT because multiple versions of the same logical page
    // share a path; the file only exists once. The statement must be
    // dropped before we call tx.commit() to release the borrow on `tx`.
    let page_paths: Vec<String> = {
        let mut path_stmt = tx.prepare("SELECT DISTINCT path FROM pages WHERE project_id = ?1")?;
        path_stmt
            .query_map(rusqlite::params![&pid[..]], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?
    };

    // Cascade handles pages / sessions / observations / handoffs /
    // page_embeddings. The workspace row is intentionally left intact —
    // other projects may still live there.
    tx.execute(
        "DELETE FROM projects WHERE id = ?1 AND workspace_id = ?2",
        rusqlite::params![&pid[..], workspace_id.as_bytes()],
    )?;

    tx.commit()?;
    Ok(PurgeSummary {
        label: workspace_project_label.to_string(),
        page_paths,
        pages_deleted,
        sessions_deleted,
        observations_deleted,
        handoffs_deleted,
        embeddings_deleted,
    })
}

/// Summary returned by [`move_project_workspace`] and exposed via
/// [`crate::writer::WriterHandle::move_project_workspace`].
#[derive(Debug, Default, Clone)]
pub struct MoveSummary {
    /// `pages` rows re-stamped (all versions, not just latest).
    pub pages_moved: u64,
    /// `sessions` rows re-stamped.
    pub sessions_moved: u64,
    /// `observations` rows re-stamped.
    pub observations_moved: u64,
    /// `handoffs` rows re-stamped.
    pub handoffs_moved: u64,
    /// `audit_log` rows re-stamped.
    pub audit_log_moved: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs_moved: u64,
    /// `auto_improve_proposals` rows re-stamped.
    pub auto_improve_proposals_moved: u64,
    /// `auto_improve_scheduler_state` rows re-stamped.
    pub auto_improve_scheduler_state_moved: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_scheduler_claims_moved: u64,
}

/// Re-stamp a project's `workspace_id` across every domain table in ONE
/// transaction, keeping the same `project_id`. This is a lossless "true move":
/// pages, sessions, observations, handoffs and supersession history all stay
/// attached to the project — unlike a copy+purge, which drops everything but
/// the latest pages.
///
/// `page_embeddings` and `links` are keyed by `page_id` (not `workspace_id`),
/// so they follow automatically with no re-stamp.
///
/// The destination workspace row MUST already exist (FK on
/// `projects.workspace_id`); the caller get-or-creates it first. A same-named
/// project already living in the destination workspace makes the `projects`
/// UPDATE violate `UNIQUE (workspace_id, name)` and the whole transaction
/// rolls back — the caller must detect that merge case and route it through
/// copy+purge instead.
pub fn move_project_workspace(
    conn: &mut Connection,
    project_id: &ProjectId,
    from_workspace: &WorkspaceId,
    to_workspace: &WorkspaceId,
) -> StoreResult<MoveSummary> {
    let tx = conn.transaction()?;

    let pid = project_id.as_bytes();
    let from = from_workspace.as_bytes();
    let to = to_workspace.as_bytes();

    // Re-stamp child tables first (they carry the denormalized workspace_id),
    // then the project row last. Order is irrelevant inside the transaction,
    // but doing projects last keeps the UNIQUE(workspace_id, name) failure —
    // the merge-collision signal — as the final, cheapest check.
    let pages_moved = tx.execute(
        "UPDATE pages SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let sessions_moved = tx.execute(
        "UPDATE sessions SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let observations_moved = tx.execute(
        "UPDATE observations SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let handoffs_moved = tx.execute(
        "UPDATE handoffs SET workspace_id = ?1 WHERE project_id = ?2",
        params![&to[..], &pid[..]],
    )? as u64;
    let audit_log_moved = tx.execute(
        "UPDATE audit_log SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_runs_moved = tx.execute(
        "UPDATE auto_improve_runs SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_proposals_moved = tx.execute(
        "UPDATE auto_improve_proposals SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_scheduler_state_moved = tx.execute(
        "UPDATE auto_improve_scheduler_state SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;
    let auto_improve_scheduler_claims_moved = tx.execute(
        "UPDATE auto_improve_scheduler_claims SET workspace_id = ?1 WHERE project_id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )? as u64;

    let projects_updated = tx.execute(
        "UPDATE projects SET workspace_id = ?1 WHERE id = ?2 AND workspace_id = ?3",
        params![&to[..], &pid[..], &from[..]],
    )?;
    if projects_updated != 1 {
        return Err(StoreError::NotFound(format!(
            "project {project_id} not found in source workspace {from_workspace}"
        )));
    }

    tx.commit()?;
    Ok(MoveSummary {
        pages_moved,
        sessions_moved,
        observations_moved,
        handoffs_moved,
        audit_log_moved,
        auto_improve_runs_moved,
        auto_improve_proposals_moved,
        auto_improve_scheduler_state_moved,
        auto_improve_scheduler_claims_moved,
    })
}

/// Remove embedding rows in a workspace/project scope whose `(provider, model, dim)`
/// does not match the configured triple, plus rows tied to superseded pages.
pub fn delete_stale_page_embeddings(
    conn: &mut Connection,
    workspace_id: &WorkspaceId,
    project_id: Option<&ProjectId>,
    provider: &str,
    model: &str,
    dim: u32,
) -> StoreResult<u64> {
    let tx = conn.transaction()?;
    let (n, orphans) = if let Some(project_id) = project_id {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1\
             ) \
               AND NOT (provider = ?3 AND model = ?4 AND dim = CAST(?5 AS INTEGER))",
            params![
                workspace_id.as_bytes(),
                project_id.as_bytes(),
                provider,
                model,
                dim
            ],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
        )?;
        (n, orphans)
    } else {
        let n = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 1\
             ) \
               AND NOT (provider = ?2 AND model = ?3 AND dim = CAST(?4 AS INTEGER))",
            params![workspace_id.as_bytes(), provider, model, dim],
        )?;
        let orphans = tx.execute(
            "DELETE FROM page_embeddings \
             WHERE page_id IN (\
                SELECT id FROM pages \
                WHERE workspace_id = ?1 AND is_latest = 0\
             )",
            params![workspace_id.as_bytes()],
        )?;
        (n, orphans)
    };
    tx.commit()?;
    Ok(u64::try_from(n.saturating_add(orphans)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    //! Focused unit tests for the load-bearing mutating SQL paths.
    //!
    //! `Store::open` exercises these incidentally through
    //! integration tests, but specific edges — supersession on body
    //! change, no-op on identical body, handoff state transitions,
    //! end_session summary linkage, embedding PK-replacement —
    //! deserve direct coverage so a regression surfaces with a
    //! one-line diff instead of a cascading e2e failure.
    use super::*;
    use engram_core::{
        LinkTarget, NewHandoff, NewPage, NewSession, PagePath, ProjectId, Tier, WorkspaceId,
    };
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Open a fresh DB with migrations applied + a default workspace
    /// and "scratch" project pre-created. Tuple-return keeps the
    /// tempdir alive for the duration of the test.
    fn fresh_db() -> (
        TempDir,
        Connection,
        engram_core::WorkspaceId,
        engram_core::ProjectId,
    ) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        (tmp, conn, ws, proj)
    }

    fn page(
        ws: engram_core::WorkspaceId,
        proj: engram_core::ProjectId,
        path: &str,
        body: &str,
    ) -> NewPage {
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

    /// Trickier path: upserting a page with a CHANGED body must
    /// produce a NEW row and mark the previous row `is_latest = 0`.
    /// This is the M7 supersession chain — the entire wiki versioning
    /// guarantee rides on it.
    /// V16: every page write lands an `audit_log` row whose
    /// `author_id` mirrors the NewPage's. Anonymous writes leave it
    /// NULL (the entire audit-log-by-author query pattern relies on
    /// the partial index covering only the non-NULL minority).
    #[test]
    fn audit_log_records_author_for_attributed_create_page() {
        use engram_core::UserId;

        let (_tmp, mut conn, ws, proj) = fresh_db();

        // Seed a synthetic user row so the FK on author_id resolves.
        let user_id = UserId::new();
        let now = jiff::Timestamp::now().as_microsecond();
        conn.execute(
            "INSERT INTO users \
             (id, username, name, email, token_hash, created_at) \
             VALUES (?1, 'alice', NULL, NULL, X'00', ?2)",
            params![user_id.as_bytes(), now],
        )
        .unwrap();

        let mut np = page(ws, proj, "notes/by-alice.md", "alice body");
        np.author_id = Some(user_id);
        let page_id = upsert_page(&mut conn, &np).unwrap();

        let author_bytes: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM audit_log \
                 WHERE op = 'create_page' AND page_id = ?1",
                params![page_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let recorded = UserId::from_slice(&author_bytes).unwrap();
        assert_eq!(
            recorded, user_id,
            "create_page audit row must carry the writer's user_id"
        );
    }

    /// Backward-compat gate (and the headline of the "no behaviour
    /// change for legacy installs" promise): anonymous writes leave
    /// audit_log.author_id NULL — the partial index stays empty for
    /// pre-multi-user history.
    #[test]
    fn audit_log_records_null_author_for_anonymous_create_page() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let np = page(ws, proj, "notes/anon.md", "anon body");
        assert!(np.author_id.is_none());
        let page_id = upsert_page(&mut conn, &np).unwrap();

        let author: Option<Vec<u8>> = conn
            .query_row(
                "SELECT author_id FROM audit_log \
                 WHERE op = 'create_page' AND page_id = ?1",
                params![page_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            author.is_none(),
            "anonymous writes must record audit_log.author_id = NULL"
        );
    }

    /// Supersession rows carry the SUPERSEDING author, not the
    /// original. Two consecutive attributed writes (alice then bob)
    /// yield a create_page row tagged alice and a supersede_page row
    /// tagged bob — point-in-time truth, not "latest author".
    #[test]
    fn audit_log_supersede_records_new_authors_identity() {
        use engram_core::UserId;

        let (_tmp, mut conn, ws, proj) = fresh_db();
        let now = jiff::Timestamp::now().as_microsecond();
        let alice = UserId::new();
        let bob = UserId::new();
        conn.execute(
            "INSERT INTO users (id, username, name, email, token_hash, created_at) \
             VALUES (?1, 'alice', NULL, NULL, X'01', ?2), \
                    (?3, 'bob',   NULL, NULL, X'02', ?2)",
            params![alice.as_bytes(), now, bob.as_bytes()],
        )
        .unwrap();

        let mut np1 = page(ws, proj, "notes/shared.md", "v1");
        np1.author_id = Some(alice);
        upsert_page(&mut conn, &np1).unwrap();

        let mut np2 = page(ws, proj, "notes/shared.md", "v2 — different body");
        np2.author_id = Some(bob);
        let v2_id = upsert_page(&mut conn, &np2).unwrap();

        let bob_bytes: Vec<u8> = conn
            .query_row(
                "SELECT author_id FROM audit_log \
                 WHERE op = 'supersede_page' AND page_id = ?1",
                params![v2_id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            UserId::from_slice(&bob_bytes).unwrap(),
            bob,
            "supersede_page audit row must carry the SUPERSEDING author"
        );
    }

    #[test]
    fn upsert_page_supersedes_on_body_change() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let id1 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v1 body")).unwrap();
        let id2 = upsert_page(&mut conn, &page(ws, proj, "notes/foo.md", "v2 body")).unwrap();

        assert_ne!(id1, id2, "supersession must produce a new row id");

        let latest_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1 AND is_latest = 1",
                params!["notes/foo.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(latest_count, 1, "exactly one latest version expected");

        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1",
                params!["notes/foo.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 2, "old version must remain on disk for history");

        // The newest row should point at the older as its predecessor
        // (supersedes column), so chains are reconstructible.
        let supersedes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT supersedes FROM pages WHERE id = ?1",
                params![&id2.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(supersedes.is_some(), "new row must link to its predecessor");
    }

    /// Idempotency: re-upserting the same body should NOT create a
    /// second row. The watcher's reconciliation calls upsert_page on
    /// every file on every tick — without this, a quiet repo would
    /// accumulate spurious history every 30 seconds.
    #[test]
    fn upsert_page_is_noop_when_body_unchanged() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let p = page(ws, proj, "notes/foo.md", "same body");
        let id1 = upsert_page(&mut conn, &p).unwrap();
        let id2 = upsert_page(&mut conn, &p).unwrap();

        assert_eq!(id1, id2, "identical body should not supersede");
        conn.execute(
            "UPDATE pages SET updated_at = 123 WHERE id = ?1",
            params![id1.as_bytes()],
        )
        .unwrap();
        let id3 = upsert_page(&mut conn, &p).unwrap();
        assert_eq!(id1, id3, "identical body should keep the same page id");
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at FROM pages WHERE id = ?1",
                params![id1.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            updated_at, 123,
            "unchanged content should not dirty the row"
        );
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE path = ?1",
                params!["notes/foo.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 1, "no duplicate row for unchanged content");
    }

    #[test]
    fn upsert_page_supersedes_on_frontmatter_change() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p1 = page(ws, proj, "_slots/project_context.md", "same body");
        p1.frontmatter_json = serde_json::json!({
            "title": "Project context",
            "slot_kind": "state",
        });
        let id1 = upsert_page(&mut conn, &p1).unwrap();

        let mut p2 = p1.clone();
        p2.frontmatter_json = serde_json::json!({
            "title": "Project context",
            "slot_kind": "invariant",
        });
        let id2 = upsert_page(&mut conn, &p2).unwrap();

        assert_ne!(id1, id2, "frontmatter-only changes must supersede");
        let latest_frontmatter: String = conn
            .query_row(
                "SELECT frontmatter_json FROM pages WHERE id = ?1 AND is_latest = 1",
                params![id2.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            latest_frontmatter.contains("invariant"),
            "latest row should store the updated slot_kind"
        );
    }

    #[test]
    fn upsert_page_persists_and_resolves_links() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut source = page(ws, proj, "concepts/source.md", "see target");
        source.links = vec![PagePath::new("decisions/target.md").unwrap().into()];
        let source_id = upsert_page(&mut conn, &source).unwrap();

        let unresolved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM links \
                 WHERE from_page_id = ?1 AND to_path = ?2 AND to_page_id IS NULL",
                params![source_id.as_bytes(), "decisions/target.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 1, "forward link should persist unresolved");

        let target_id = upsert_page(
            &mut conn,
            &page(ws, proj, "decisions/target.md", "target body"),
        )
        .unwrap();

        let resolved: Option<Vec<u8>> = conn
            .query_row(
                "SELECT to_page_id FROM links WHERE from_page_id = ?1 AND to_path = ?2",
                params![source_id.as_bytes(), "decisions/target.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved.as_deref(), Some(&target_id.as_bytes()[..]));
    }

    /// A `[[infra:runbooks/02.md]]` link from one project resolves to a page
    /// in a sibling project once that page exists — the cross-project edge.
    #[test]
    fn upsert_page_resolves_cross_project_link() {
        let (_tmp, mut conn, ws, scratch) = fresh_db();
        let infra = get_or_create_project(&mut conn, &ws, "infra", None).unwrap();

        let mut source = page(ws, scratch, "concepts/dep.md", "depends on infra runbook");
        source.links = vec![LinkTarget {
            workspace: None,
            project: Some("infra".into()),
            path: PagePath::new("runbooks/02.md").unwrap(),
        }];
        let source_id = upsert_page(&mut conn, &source).unwrap();

        // Persisted with the scope, unresolved until the target project's page exists.
        let (to_project, resolved): (Option<String>, Option<Vec<u8>>) = conn
            .query_row(
                "SELECT to_project, to_page_id FROM links \
                 WHERE from_page_id = ?1 AND to_path = ?2",
                params![source_id.as_bytes(), "runbooks/02.md"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(to_project.as_deref(), Some("infra"));
        assert!(
            resolved.is_none(),
            "cross-project link is unresolved before the target exists"
        );

        // Create the target in `infra` → the forward link repoints across projects.
        let target_id =
            upsert_page(&mut conn, &page(ws, infra, "runbooks/02.md", "the runbook")).unwrap();
        let resolved: Option<Vec<u8>> = conn
            .query_row(
                "SELECT to_page_id FROM links WHERE from_page_id = ?1 AND to_path = ?2",
                params![source_id.as_bytes(), "runbooks/02.md"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            resolved.as_deref(),
            Some(&target_id.as_bytes()[..]),
            "link must resolve across projects once the target lands"
        );
    }

    /// Handoff state machine: insert → Open; accept_handoff → Accepted
    /// with accepted_by stamped. Calling accept again must be safe
    /// (idempotent at the DB level) because hooks fire-and-forget.
    #[test]
    fn accept_handoff_transitions_open_to_accepted() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "test summary".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        };
        let id = insert_handoff(&mut conn, &new).unwrap();

        // Pre-state: Open, accepted_by NULL.
        let (state, accepted_by): (String, Option<String>) = conn
            .query_row(
                "SELECT state, accepted_by FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "open");
        assert!(accepted_by.is_none());

        accept_handoff(&mut conn, &id, AgentKind::Codex, None).unwrap();
        let (state, accepted_by): (String, Option<String>) = conn
            .query_row(
                "SELECT state, accepted_by FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "accepted");
        assert_eq!(accepted_by.as_deref(), Some("codex"));

        // Idempotency: accepting an already-accepted handoff must
        // either succeed silently or fail clearly, never corrupt
        // the row. (Current impl is a no-op UPDATE with a state
        // guard.)
        let second = accept_handoff(&mut conn, &id, AgentKind::Codex, None);
        assert!(second.is_ok(), "double-accept must not error");
    }

    /// The stored cwd is normalized (trailing path separator stripped) at insert time
    /// so trailing-slash drift between agent payloads cannot break the next
    /// session's path-boundary match. Covers both manual and auto handoffs,
    /// since both go through `insert_handoff`.
    #[test]
    fn insert_handoff_strips_trailing_separator_from_cwd() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: Some(std::path::PathBuf::from("/home/u/repo/")),
            summary: "trailing slash".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        };
        let id = insert_handoff(&mut conn, &new).unwrap();
        let cwd: Option<String> = conn
            .query_row(
                "SELECT cwd FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cwd.as_deref(), Some("/home/u/repo"));

        let windows = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: Some(std::path::PathBuf::from(r"C:\repo\")),
            summary: "trailing backslash".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        };
        let id = insert_handoff(&mut conn, &windows).unwrap();
        let cwd: Option<String> = conn
            .query_row(
                "SELECT cwd FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cwd.as_deref(), Some(r"C:\repo"));
    }

    #[test]
    fn cancel_handoff_transitions_open_to_expired() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let new = NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "accidental handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        };
        let id = insert_handoff(&mut conn, &new).unwrap();

        assert!(cancel_handoff(&mut conn, &id).unwrap());
        let state: String = conn
            .query_row(
                "SELECT state FROM handoffs WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "expired");

        assert!(
            !cancel_handoff(&mut conn, &id).unwrap(),
            "double-cancel should be a no-op"
        );
    }

    /// Supported hook agents persist concrete agent_kind values. V01's CHECK
    /// omitted agents added after launch; regression for hook-router WARNs.
    #[test]
    fn begin_session_accepts_all_supported_agent_kinds() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        for agent_kind in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::OpenCode,
            AgentKind::Cursor,
            AgentKind::GeminiCli,
            AgentKind::ClaudeDesktop,
            AgentKind::OpenClaw,
            AgentKind::AntigravityCli,
            AgentKind::Omp,
            AgentKind::Pi,
            AgentKind::Grok,
            AgentKind::Other,
        ] {
            let sid = SessionId::new();
            begin_session(
                &mut conn,
                &NewSession {
                    id: sid,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind,
                    cwd: Some(std::path::PathBuf::from(r"C:\GIT\engram")),
                },
            )
            .unwrap();

            let stored: String = conn
                .query_row(
                    "SELECT agent_kind FROM sessions WHERE id = ?1",
                    params![&sid.as_bytes()[..]],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(stored, agent_kind.as_str());
        }
    }

    /// end_session links the synthesised summary page so callers can
    /// jump straight from session row to summary.
    #[test]
    fn end_session_links_summary_page_when_provided() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
            },
        )
        .unwrap();
        let page_id = upsert_page(
            &mut conn,
            &page(ws, proj, "sessions/abc.md", "summary body"),
        )
        .unwrap();
        end_session(&mut conn, &sid, Some(&page_id)).unwrap();

        let summary: Option<Vec<u8>> = conn
            .query_row(
                "SELECT summary_page_id FROM sessions WHERE id = ?1",
                params![&sid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            summary.is_some(),
            "summary_page_id must persist when supplied"
        );
        let bytes = summary.unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[..], &page_id.as_bytes()[..]);
    }

    /// end_session without a summary leaves the column NULL — the
    /// session ended but no page was synthesised (e.g. zero
    /// observations recorded). This must not be confused with the
    /// summary-linked case.
    #[test]
    fn end_session_without_summary_page_id_leaves_null() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
            },
        )
        .unwrap();
        end_session(&mut conn, &sid, None).unwrap();
        let summary: Option<Vec<u8>> = conn
            .query_row(
                "SELECT summary_page_id FROM sessions WHERE id = ?1",
                params![&sid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(summary.is_none());
    }

    /// Re-storing for the same page must REPLACE its whole chunk set,
    /// not duplicate or leave stale tail chunks — otherwise `engram
    /// embed --reembed` would multiply rows on each run, and a page
    /// shrinking from N chunks to M < N would keep orphan chunk rows.
    #[test]
    fn store_embedding_replaces_existing_chunk_set() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let pid = upsert_page(&mut conn, &page(ws, proj, "notes/x.md", "body")).unwrap();
        store_embedding(
            &mut conn,
            &pid,
            &[
                vec![0u8; 1536 * 4],
                vec![1u8; 1536 * 4],
                vec![2u8; 1536 * 4],
            ],
            "test",
            "model-a",
            1536,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &pid,
            &[vec![3u8; 1536 * 4]],
            "test",
            "model-b",
            1536,
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM page_embeddings WHERE page_id = ?1",
                params![&pid.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "chunk set must shrink to the new chunk count");

        let (chunk_index, model): (i64, String) = conn
            .query_row(
                "SELECT chunk_index, model FROM page_embeddings WHERE page_id = ?1",
                params![&pid.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(chunk_index, 0);
        assert_eq!(model, "model-b", "latest model metadata wins");
    }

    /// A long document stores one row per chunk, indexed in order.
    #[test]
    fn store_embedding_stores_one_row_per_chunk() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let pid = upsert_page(&mut conn, &page(ws, proj, "notes/long.md", "body")).unwrap();
        store_embedding(
            &mut conn,
            &pid,
            &[vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]],
            "test",
            "model",
            1,
        )
        .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT chunk_index FROM page_embeddings WHERE page_id = ?1 ORDER BY chunk_index",
            )
            .unwrap();
        let indexes: Vec<i64> = stmt
            .query_map(params![&pid.as_bytes()[..]], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(indexes, vec![0, 1, 2]);
    }

    #[test]
    fn store_embeddings_batches_rows_in_one_call() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let p1 = upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body a")).unwrap();
        let p2 = upsert_page(&mut conn, &page(ws, proj, "notes/b.md", "body b")).unwrap();

        store_embeddings(
            &mut conn,
            &[
                EmbeddingWrite {
                    page_id: p1,
                    vectors: vec![vec![0u8; 4]],
                    provider: "test".into(),
                    model: "model".into(),
                    dim: 1,
                },
                EmbeddingWrite {
                    page_id: p2,
                    vectors: vec![vec![1u8; 4], vec![2u8; 4]],
                    provider: "test".into(),
                    model: "model".into(),
                    dim: 1,
                },
            ],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM page_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "one row per chunk across the batch");
    }

    /// `PurgeSummary.embeddings_deleted` is rendered beside page and
    /// session counts, so it must stay page-granular even though a
    /// long page now holds several chunk rows.
    #[test]
    fn purge_project_counts_embedded_pages_not_chunk_rows() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let long = upsert_page(&mut conn, &page(ws, proj, "long.md", "long body")).unwrap();
        let short = upsert_page(&mut conn, &page(ws, proj, "short.md", "short body")).unwrap();
        store_embedding(
            &mut conn,
            &long,
            &[vec![0u8; 4], vec![1u8; 4], vec![2u8; 4]],
            "test",
            "model",
            1,
        )
        .unwrap();
        store_embedding(&mut conn, &short, &[vec![3u8; 4]], "test", "model", 1).unwrap();

        let summary = purge_project(&mut conn, &ws, &proj, "default/scratch").unwrap();
        assert_eq!(
            summary.embeddings_deleted, 2,
            "two embedded pages (4 chunk rows), not 4"
        );
    }

    #[test]
    fn delete_stale_page_embeddings_removes_mismatched_rows() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other = get_or_create_project(&mut conn, &ws, "other", None).unwrap();
        let p1 = upsert_page(&mut conn, &page(ws, proj, "a.md", "body a")).unwrap();
        let p2 = upsert_page(&mut conn, &page(ws, proj, "b.md", "body b")).unwrap();
        let p3 = upsert_page(&mut conn, &page(ws, other, "c.md", "body c")).unwrap();
        let old = upsert_page(&mut conn, &page(ws, proj, "old.md", "old body")).unwrap();
        let _new = upsert_page(&mut conn, &page(ws, proj, "old.md", "new body")).unwrap();
        store_embedding(
            &mut conn,
            &p1,
            &[vec![0u8; 4]],
            "google",
            "models/gemini-embedding-001",
            768,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &p3,
            &[vec![2u8; 4]],
            "google",
            "models/gemini-embedding-001",
            768,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &p2,
            &[vec![1u8; 4]],
            "openai",
            "openai/text-embedding-3-small",
            1536,
        )
        .unwrap();
        store_embedding(
            &mut conn,
            &old,
            &[vec![3u8; 4]],
            "openai",
            "openai/text-embedding-3-small",
            1536,
        )
        .unwrap();
        let n = super::delete_stale_page_embeddings(
            &mut conn,
            &ws,
            Some(&proj),
            "openai",
            "openai/text-embedding-3-small",
            1536,
        )
        .unwrap();
        assert_eq!(n, 2);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM page_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 2);
        let model: String = conn
            .query_row(
                "SELECT model FROM page_embeddings WHERE page_id = ?1",
                params![&p2.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(model, "openai/text-embedding-3-small");
        let other_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM page_embeddings WHERE page_id = ?1",
                params![&p3.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            other_rows, 1,
            "explicit project purge must not touch siblings"
        );
    }

    #[test]
    fn path_search_text_indexes_slug_and_words() {
        // Both forms: hyphenated slug kept whole, plus split into words.
        assert_eq!(
            path_search_text("notes/foo-bar.md"),
            "notes foo-bar md notes foo bar md"
        );
        assert_eq!(path_search_text("a/b_c.md"), "a b_c md a b c md");
    }

    /// A page is findable by its PATH slug even when the slug appears in
    /// neither the title nor the body — the V17 `path_search` FTS column.
    #[test]
    fn fts_matches_page_by_path_slug_not_in_body() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        // Title + body deliberately avoid the slug words.
        let mut p = page(
            ws,
            proj,
            "notes/followup-bulk-rename-runbook-titles.md",
            "totally unrelated prose about elephants",
        );
        p.title = "Elephants".into();
        upsert_page(&mut conn, &p).unwrap();

        // The slug, as a quoted single token (how prepare_fts5_query renders a
        // hyphenated term), matches via the path_search column.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts \
                 WHERE pages_fts MATCH ?1",
                params!["\"followup-bulk-rename-runbook-titles\""],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "slug in path must be searchable");

        // A distinct path segment is independently searchable too.
        let seg: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'runbook'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seg, 1, "path segment token must match");

        // Body words still match (regression: body stays indexed at col 1).
        let body_hit: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'elephants'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(body_hit, 1, "body must remain searchable");
    }

    #[test]
    fn pages_fts_path_migration_preserves_accent_folding() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        let mut p = page(ws, proj, "notes/descricao.md", "descrição do projeto");
        p.title = "Descrição".into();
        upsert_page(&mut conn, &p).unwrap();

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH 'descricao'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "page FTS should remain accent-insensitive");
    }

    #[test]
    fn pages_fts_update_trigger_ignores_access_counter_updates() {
        let (_tmp, conn, _ws, _proj) = fresh_db();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'pages_fts_au'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("AFTER UPDATE OF title, body, path_search ON pages"),
            "pages_fts_au must not fire on access_count/last_accessed_at updates: {sql}"
        );
    }

    /// True move: re-stamping a project's workspace_id keeps the same
    /// project_id and carries pages, sessions and observations along —
    /// the whole point of the lossless move. The summary counts must
    /// match what actually moved.
    #[test]
    fn move_project_workspace_restamps_all_domain_rows() {
        use engram_core::ObservationKind;

        let (_tmp, mut conn, src_ws, proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "djalmajr").unwrap();

        // Seed a page, a session and an observation under the source ws.
        let page_id = upsert_page(&mut conn, &page(src_ws, proj, "notes/a.md", "body")).unwrap();
        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: src_ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
            },
        )
        .unwrap();
        insert_observation(
            &mut conn,
            &NewObservation {
                session_id: sid,
                workspace_id: src_ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "t".into(),
                body: "b".into(),
                importance: 5,
            },
        )
        .unwrap();

        let summary = move_project_workspace(&mut conn, &proj, &src_ws, &dst_ws).unwrap();
        assert_eq!(summary.pages_moved, 1);
        assert_eq!(summary.sessions_moved, 1);
        assert_eq!(summary.observations_moved, 1);

        // The project_id is unchanged; every row now points at dst_ws.
        // `projects` keys the project by `id`; child tables by `project_id`.
        let count_in = |table: &str, ws: &engram_core::WorkspaceId| -> i64 {
            let id_col = if table == "projects" {
                "id"
            } else {
                "project_id"
            };
            conn.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {id_col} = ?1 AND workspace_id = ?2"),
                params![&proj.as_bytes()[..], ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap()
        };
        for table in ["projects", "pages", "sessions", "observations"] {
            assert_eq!(count_in(table, &dst_ws), 1, "{table} must move to dst ws");
            assert_eq!(count_in(table, &src_ws), 0, "{table} must leave src ws");
        }
        // The page keeps its id (embeddings/links follow via page_id).
        let still_there: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE id = ?1 AND workspace_id = ?2",
                params![&page_id.as_bytes()[..], dst_ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_there, 1);
    }

    /// A same-named project already in the destination workspace makes the
    /// projects UPDATE collide with UNIQUE(workspace_id, name); the whole
    /// transaction must roll back, leaving the source intact. The admin
    /// layer detects this merge case up front and routes it to copy+purge.
    #[test]
    fn move_project_workspace_rolls_back_on_name_collision() {
        let (_tmp, mut conn, src_ws, proj) = fresh_db();
        let dst_ws = get_or_create_workspace(&mut conn, "djalmajr").unwrap();
        // Destination already holds a project named "scratch".
        get_or_create_project(&mut conn, &dst_ws, "scratch", None).unwrap();
        upsert_page(&mut conn, &page(src_ws, proj, "notes/a.md", "body")).unwrap();

        let err = move_project_workspace(&mut conn, &proj, &src_ws, &dst_ws);
        assert!(err.is_err(), "name collision must fail the move");

        // Source rows untouched after rollback.
        let src_pages: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE project_id = ?1 AND workspace_id = ?2",
                params![&proj.as_bytes()[..], src_ws.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src_pages, 1, "rollback must preserve source pages");
    }

    #[test]
    fn ensure_project_workspace_rejects_stale_pair_before_disk_write() {
        let (_tmp, conn, ws, proj) = fresh_db();
        let other_ws = WorkspaceId::new();

        ensure_project_workspace(&conn, &ws, &proj).unwrap();
        assert!(
            matches!(
                ensure_project_workspace(&conn, &other_ws, &proj),
                Err(StoreError::NotFound(_))
            ),
            "a stale workspace/project pair must fail before wiki writes touch disk"
        );
    }

    #[test]
    fn ensure_workspace_with_id_rejects_id_name_mismatch() {
        let (_tmp, mut conn, _ws, _proj) = fresh_db();
        let id = WorkspaceId::new();

        ensure_workspace_with_id(&mut conn, id, "from-manifest").unwrap();
        let err = ensure_workspace_with_id(&mut conn, id, "other-name").unwrap_err();

        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "same workspace id with different name must fail loudly; got {err:?}"
        );
    }

    #[test]
    fn ensure_project_with_id_rejects_existing_id_mismatch() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();
        let id = ProjectId::new();

        ensure_project_with_id(&mut conn, id, ws, "from-manifest", Some("/repo/a")).unwrap();
        let err =
            ensure_project_with_id(&mut conn, id, ws, "renamed", Some("/repo/a")).unwrap_err();

        assert!(
            matches!(err, StoreError::Duplicate(_)),
            "same project id with different manifest data must fail loudly; got {err:?}"
        );
    }

    /// V19 data-repair migration: observations whose `project_id`
    /// disagrees with their session's `project_id` are re-attributed
    /// to the session's project. Handoffs that carry a session id are
    /// repaired the same way. Project rows that become truly empty
    /// after repair are deleted. The migration is idempotent: re-run
    /// on a repaired DB updates / deletes nothing.
    #[test]
    fn v19_repairs_orphan_observation_attribution_and_purges_empty_projects() {
        use engram_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

        // Apply migrations through V18 (not V19) so we can seed the
        // orphaned-attribution state V19 is designed to repair. If we
        // ran the full chain via `fresh_db`, V19 would already be in
        // the refinery history and re-invoking `migrations::run` below
        // would be a no-op.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 18).unwrap();

        // Seed the bug shape with V18-and-earlier semantics: parent
        // project `manga-plus` and fragment project `reader` co-exist
        // in the same workspace; a session lives under `manga-plus`
        // and an observation was misattributed to `reader`.
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let parent = get_or_create_project(
            &mut conn,
            &ws,
            "manga-plus",
            Some("/mnt/data/Projects/manga-plus"),
        )
        .unwrap();
        let fragment = get_or_create_project(
            &mut conn,
            &ws,
            "reader",
            Some("/mnt/data/Projects/manga-plus/reader"),
        )
        .unwrap();

        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: parent,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some("/mnt/data/Projects/manga-plus".into()),
            },
        )
        .unwrap();

        // Three misattributed observations on the fragment.
        for i in 0..3 {
            insert_observation(
                &mut conn,
                &NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: fragment,
                    kind: ObservationKind::PreToolUse,
                    extension: None,
                    source_event: None,
                    title: format!("call {i}"),
                    body: "body".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }

        // Run the repair migration (V19). Target V19 explicitly rather than
        // the open-ended `run`: this test seeds rows first (leaving cached
        // statements on `sessions`), so letting a later table-rebuild
        // migration (V20+) run here would trip SQLITE_LOCKED on its
        // `DROP TABLE sessions`. Production runs migrations before any query,
        // so the rebuild is unaffected there.
        crate::migrations::run_to(&mut conn, 19).unwrap();

        // All observations now point at the parent.
        let cnt_parent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
                params![&parent.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt_parent, 3, "observations re-attributed to parent");

        // The fragment row is gone — it's truly empty post-repair.
        let frag_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![&fragment.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(frag_rows, 0, "fragment project row deleted");

        // Parent survives; it owns its rows.
        let parent_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![&parent.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parent_rows, 1);
    }

    // Hollow-project sweep: deletes only rows with zero data of any kind
    // and past the age cutoff; reserved names survive even when hollow.
    #[test]
    fn sweep_hollow_projects_deletes_only_old_dataless_rows() {
        use engram_core::{AgentKind, NewSession, SessionId};
        let (_tmp, mut conn, ws, _scratch) = fresh_db();

        // Hollow + old: delete. (Backdate created_at 8 days.)
        let hollow = get_or_create_project(&mut conn, &ws, "zt", None).unwrap();
        // Hollow + fresh: keep (inside the grace window).
        let fresh = get_or_create_project(&mut conn, &ws, "new-probe", None).unwrap();
        // Old but has a session: keep (not hollow).
        let with_data = get_or_create_project(&mut conn, &ws, "one-off", None).unwrap();
        // Reserved + hollow + old: keep.
        let global =
            get_or_create_project(&mut conn, &ws, engram_core::GLOBAL_SCOPE_PROJECT, None).unwrap();

        let eight_days_us: i64 = 8 * 24 * 60 * 60 * 1_000_000;
        for id in [&hollow, &with_data, &global] {
            conn.execute(
                "UPDATE projects SET created_at = created_at - ?1 WHERE id = ?2",
                params![eight_days_us, &id.as_bytes()[..]],
            )
            .unwrap();
        }
        begin_session(
            &mut conn,
            &NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: with_data,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
            },
        )
        .unwrap();

        let deleted = sweep_hollow_projects(&mut conn, 7).unwrap();
        assert_eq!(deleted, vec!["zt".to_string()]);

        let exists = |id: &engram_core::ProjectId| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM projects WHERE id = ?1",
                params![&id.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(exists(&hollow), 0, "old hollow row deleted");
        assert_eq!(exists(&fresh), 1, "fresh hollow row kept (grace window)");
        assert_eq!(exists(&with_data), 1, "data-bearing row kept");
        assert_eq!(exists(&global), 1, "reserved _global kept even when hollow");

        // Idempotent: a second pass deletes nothing.
        assert!(sweep_hollow_projects(&mut conn, 7).unwrap().is_empty());
    }

    /// V27 re-runs the V19 repair for the fragments that accumulated
    /// after V19: non-git parents have no repo_path, so the v0.12.2
    /// prefix guard couldn't anchor subdirectory cwds and per-event
    /// basename derivation kept minting fragment projects. Idempotent:
    /// a second pass repairs nothing.
    #[test]
    fn v27_reattributes_nongit_fragments_and_preserves_reserved_projects() {
        use engram_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::migrations::run_to(&mut conn, 26).unwrap();

        // Non-git parent (repo_path NULL — the production shape) plus a
        // basename fragment holding misattributed observations, and the
        // reserved projects that must survive even when empty.
        let ws = get_or_create_workspace(&mut conn, "default").unwrap();
        let parent = get_or_create_project(&mut conn, &ws, "tiktok_analysis", None).unwrap();
        let fragment = get_or_create_project(&mut conn, &ws, "sources", None).unwrap();
        let scratch = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        let global = get_or_create_project(&mut conn, &ws, "_global", None).unwrap();

        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: parent,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some("/home/user/tiktok_analysis".into()),
            },
        )
        .unwrap();
        for i in 0..4 {
            insert_observation(
                &mut conn,
                &NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: fragment,
                    kind: ObservationKind::PostToolUse,
                    extension: None,
                    source_event: None,
                    title: format!("call {i}"),
                    body: "body".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }

        crate::migrations::run_to(&mut conn, 27).unwrap();

        let count = |sql: &str, id: &engram_core::ProjectId| -> i64 {
            conn.query_row(sql, params![&id.as_bytes()[..]], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
                &parent
            ),
            4,
            "observations re-attributed to the non-git parent"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &fragment),
            0,
            "emptied fragment row deleted"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &scratch),
            1,
            "scratch is reserved and survives empty"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &global),
            1,
            "_global is reserved and survives empty"
        );

        // Idempotency: replay V27's statements directly (refinery won't
        // re-run an applied version); zero rows change.
        let sql = include_str!("../migrations/V27__repair_nongit_fragment_attribution.sql");
        conn.execute_batch(sql).unwrap();
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM observations WHERE project_id = ?1",
                &parent
            ),
            4
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM projects WHERE id = ?1", &scratch),
            1
        );
    }

    #[test]
    fn v20_adds_grok_and_preserves_sessions_invariants_on_upgraded_db() {
        use engram_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let ws;
        let proj;
        let existing_sid = SessionId::new();

        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            crate::migrations::run_to(&mut conn, 19).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            ws = get_or_create_workspace(&mut conn, "default").unwrap();
            proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
            begin_session(
                &mut conn,
                &NewSession {
                    id: existing_sid,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                },
            )
            .unwrap();
            insert_observation(
                &mut conn,
                &NewObservation {
                    session_id: existing_sid,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "before v20".into(),
                    body: "existing observation survives table rebuild".into(),
                    importance: 5,
                },
            )
            .unwrap();
        }

        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 20).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        begin_session(
            &mut conn,
            &NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Grok,
                cwd: None,
            },
        )
        .unwrap();

        let obs_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                params![existing_sid.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(obs_count, 1, "V20 must preserve existing observations");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 3, "V20 must recreate sessions indexes");

        let trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            trigger_count, 1,
            "V20 must recreate the V18 pairing trigger"
        );

        let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let other_proj =
            get_or_create_project(&mut conn, &other_ws, "other-project", None).unwrap();
        let err = begin_session(
            &mut conn,
            &NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: other_proj,
                agent_kind: AgentKind::Grok,
                cwd: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("sessions.workspace_id does not match"),
            "pairing trigger must reject split-brain sessions after V20: {err}"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "V20 must leave foreign keys clean");
    }

    #[test]
    fn v25_adds_pi_and_preserves_sessions_invariants_on_upgraded_db() {
        use engram_core::{AgentKind, NewSession, SessionId};

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let ws;
        let proj;
        let existing_sid = SessionId::new();

        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            crate::migrations::run_to(&mut conn, 24).unwrap();
            conn.pragma_update(None, "foreign_keys", "ON").unwrap();
            ws = get_or_create_workspace(&mut conn, "default").unwrap();
            proj = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
            begin_session(
                &mut conn,
                &NewSession {
                    id: existing_sid,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                },
            )
            .unwrap();
        }

        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        crate::migrations::run_to(&mut conn, 25).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        begin_session(
            &mut conn,
            &NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Pi,
                cwd: None,
            },
        )
        .unwrap();

        let session_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(session_count, 2, "V25 must preserve existing sessions");

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' \
                   AND name IN ('idx_sessions_recent', 'idx_sessions_project', 'idx_sessions_started_at', 'idx_sessions_scope_ended')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 4, "V25 must recreate sessions indexes");

        let trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'sessions_ws_proj_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            trigger_count, 1,
            "V25 must recreate the V18 pairing trigger"
        );

        let scheduler_trigger_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'auto_improve_scheduler_claims_session_pairing_ai'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            scheduler_trigger_count, 1,
            "V25 must recreate the V22 scheduler/session pairing trigger"
        );

        let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();
        let other_proj =
            get_or_create_project(&mut conn, &other_ws, "other-project", None).unwrap();
        let err = begin_session(
            &mut conn,
            &NewSession {
                id: SessionId::new(),
                workspace_id: ws,
                project_id: other_proj,
                agent_kind: AgentKind::Pi,
                cwd: None,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("sessions.workspace_id does not match"),
            "pairing trigger must reject split-brain sessions after V25: {err}"
        );

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "V25 must leave foreign keys clean");
    }

    /// V19 is idempotent: re-running on a repaired DB is a no-op.
    /// Also asserts the initial run on a clean DB (no orphans, no
    /// empty fragments) is a no-op.
    #[test]
    fn v19_is_idempotent() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        // fresh_db already ran the full chain (including V19). Seed a
        // few valid rows to ensure they survive a re-run.
        upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();
        let before: (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM projects), \
                        (SELECT COUNT(*) FROM observations), \
                        (SELECT COUNT(*) FROM pages)",
                params![],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        crate::migrations::run(&mut conn).unwrap();
        let after: (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM projects), \
                        (SELECT COUNT(*) FROM observations), \
                        (SELECT COUNT(*) FROM pages)",
                params![],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            before, after,
            "V19 must be a no-op on already-repaired data"
        );
    }

    /// `scratch` keeps its standalone handoffs even when it would
    /// otherwise look empty. CLAUDE.md invariant #15a names it as the
    /// defensive default for hook events that arrive without a usable
    /// cwd; the V19 DELETE explicitly carves it out.
    #[test]
    fn v19_preserves_scratch_with_standalone_handoffs() {
        use engram_core::{AgentKind, NewHandoff};

        let (_tmp, mut conn, ws, _proj) = fresh_db();
        // Add a standalone handoff to scratch (no from_session_id).
        let scratch = get_or_create_project(&mut conn, &ws, "scratch", None).unwrap();
        insert_handoff(
            &mut conn,
            &NewHandoff {
                workspace_id: ws,
                project_id: scratch,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: "standalone".into(),
                open_questions: vec![],
                next_steps: vec![],
                files_touched: vec![],
            },
        )
        .unwrap();

        crate::migrations::run(&mut conn).unwrap();

        let scratch_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM projects WHERE name = 'scratch'",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            scratch_rows, 1,
            "scratch must survive even if it looks empty"
        );
        let scratch_handoffs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM handoffs WHERE project_id = ?1",
                params![&scratch.as_bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scratch_handoffs, 1);
    }

    #[test]
    fn v18_migration_refuses_existing_split_brain_rows() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();
        crate::migrations::run_to(&mut conn, 17).unwrap();

        let src_ws = get_or_create_workspace(&mut conn, "src").unwrap();
        let stale_ws = get_or_create_workspace(&mut conn, "stale").unwrap();
        let proj = get_or_create_project(&mut conn, &src_ws, "scratch", None).unwrap();
        let mut bad_page = page(src_ws, proj, "notes/split.md", "body");
        bad_page.workspace_id = stale_ws;
        upsert_page(&mut conn, &bad_page).unwrap();

        let err = crate::migrations::run_to(&mut conn, 18).unwrap_err();
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "V18 must abort instead of preserving split-brain rows: {err}"
        );
    }

    /// V18 integrity triggers: an INSERT whose `workspace_id` disagrees with
    /// the project's actual workspace ABORTs (the split-brain a stale hook
    /// cache would otherwise create), while the consistent pair inserts fine.
    #[test]
    fn insert_with_mismatched_workspace_is_rejected() {
        use engram_core::ObservationKind;

        let (_tmp, mut conn, ws, proj) = fresh_db();
        let other_ws = get_or_create_workspace(&mut conn, "other").unwrap();

        // A page under the WRONG workspace (project lives in `ws`) is refused.
        let mut bad_page = page(ws, proj, "notes/a.md", "body");
        bad_page.workspace_id = other_ws;
        assert!(
            upsert_page(&mut conn, &bad_page).is_err(),
            "page insert with mismatched workspace must abort"
        );

        // The consistent pair inserts fine.
        upsert_page(&mut conn, &page(ws, proj, "notes/a.md", "body")).unwrap();

        // The session insert is guarded too: a mismatched pair aborts.
        let bad_sid = SessionId::new();
        assert!(
            begin_session(
                &mut conn,
                &NewSession {
                    id: bad_sid,
                    workspace_id: other_ws,
                    project_id: proj,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                },
            )
            .is_err(),
            "session insert with mismatched workspace must abort"
        );

        let sid = SessionId::new();
        begin_session(
            &mut conn,
            &NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
            },
        )
        .unwrap();

        // The split-brain case the maintainer flagged: a hook writes an
        // observation with a stale workspace id for a moved project.
        let mismatched_obs = NewObservation {
            session_id: sid,
            workspace_id: other_ws,
            project_id: proj,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "t".into(),
            body: "b".into(),
            importance: 5,
        };
        assert!(
            insert_observation(&mut conn, &mismatched_obs).is_err(),
            "observation insert with mismatched workspace must abort"
        );

        // Same observation under the correct workspace is accepted.
        let good_obs = NewObservation {
            workspace_id: ws,
            ..mismatched_obs
        };
        insert_observation(&mut conn, &good_obs).unwrap();

        // The handoff insert is the fourth INSERT trigger; audit flagged
        // that the original V18 test omitted it, so the only coverage
        // for the handoffs trigger was the temp-table CHECK on migration.
        // Assert the BEFORE INSERT trigger fires on a stale pair, and
        // a corrected pair lands cleanly.
        let mismatched_handoff = NewHandoff {
            workspace_id: other_ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "stale".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        };
        assert!(
            insert_handoff(&mut conn, &mismatched_handoff).is_err(),
            "handoff insert with mismatched workspace must abort"
        );
        let good_handoff = NewHandoff {
            workspace_id: ws,
            ..mismatched_handoff
        };
        insert_handoff(&mut conn, &good_handoff).unwrap();
    }

    /// Regression for the rename-vs-purge race that live exploration
    /// caught: a `rename_project` for a row that was deleted between
    /// the admin handler's `lookup_ws_proj_no_create` and the
    /// `UPDATE projects` used to silently return `Ok(())` — the admin
    /// endpoint then responded `200 OK` for an operation that touched
    /// zero rows, contradicting the concurrent purge's (also `200 OK`)
    /// destruction of the same project. After the fix, the writer
    /// returns `StoreError::NotFound`, which the admin handler maps to
    /// `404 Not Found`. Pins both the writer-side semantic and a
    /// concrete recipe for the failure shape so a future refactor
    /// can't quietly downgrade the error back to a silent Ok.
    #[test]
    fn rename_project_after_purge_returns_not_found() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        // Simulate the post-purge state: project row gone.
        // `purge_project` drives the cascading deletes we want here.
        let _ = purge_project(&mut conn, &ws, &proj, "default/scratch")
            .expect("purge of fresh project should succeed");
        // Now try to rename the project that no longer exists. The
        // pre-fix code returned `Ok(())` because `UPDATE` affected
        // zero rows. The fix returns `NotFound` so admin handlers
        // can respond 404 honestly.
        let err = rename_project(&mut conn, &ws, &proj, "renamed")
            .expect_err("rename of purged project must error");
        match err {
            StoreError::NotFound(_) => {}
            other => panic!("expected StoreError::NotFound, got {other:?}"),
        }
    }

    /// Belt-and-suspenders for the common path: rename of an existing
    /// project still succeeds. Without this, a future "always return
    /// NotFound" regression would also pass the test above by accident.
    #[test]
    fn rename_project_of_live_project_succeeds() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        rename_project(&mut conn, &ws, &proj, "renamed-live")
            .expect("rename of live project must succeed");
    }

    /// Run the reader's exact FTS5 `MATCH` against the real, populated
    /// `pages_fts` index — the path the web search / MCP query take.
    /// Returns the matched paths (and surfaces any FTS5 syntax error as
    /// an `Err`, the way the bug originally manifested).
    fn fts_match_paths(conn: &Connection, raw: &str) -> rusqlite::Result<Vec<String>> {
        let fts_query = crate::fts_query::prepare_fts5_query(raw);
        let mut stmt = conn.prepare(
            "SELECT pages.path \
             FROM pages_fts \
             JOIN pages ON pages.rowid = pages_fts.rowid \
             WHERE pages_fts MATCH ?1 AND pages.is_latest = 1 \
             ORDER BY pages_fts.rank",
        )?;
        let rows = stmt.query_map(params![fts_query], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// End-to-end regression for the dotted-filename search bug (PR #81).
    /// Searching `current.md` used to reach FTS5 **bare** and SQLite
    /// errored with `fts5: syntax error near "."`, so the web UI showed
    /// "No results" and the MCP surfaced the raw error. The string-level
    /// `fts_query` unit tests only proved the *output* was quoted — they
    /// never exercised real FTS5. This drives the actual indexed
    /// `pages_fts` (via `upsert_page` → `path_search` triggers) to prove
    /// the prepared query (a) does not error and (b) matches the page at
    /// `reference/architecture-current.md`. This is the scenario that
    /// would have caught the bug *before* it shipped.
    #[test]
    fn dotted_filename_search_matches_indexed_path() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        upsert_page(
            &mut conn,
            &page(ws, proj, "reference/architecture-current.md", "body text"),
        )
        .unwrap();

        // The prepared query must not error AND must find the page.
        let hits = fts_match_paths(&conn, "current.md")
            .expect("dotted-filename search must not raise an FTS5 syntax error");
        assert!(
            hits.iter()
                .any(|p| p == "reference/architecture-current.md"),
            "search for `current.md` should match the indexed path; got {hits:?}"
        );

        // Guard the sanitizer is load-bearing: the same token reaching
        // FTS5 bare (the pre-fix behaviour) is a hard syntax error.
        let bare = conn
            .prepare("SELECT rowid FROM pages_fts WHERE pages_fts MATCH ?1")
            .unwrap()
            .query_map(params!["current.md"], |r| r.get::<_, i64>(0))
            .and_then(Iterator::collect::<rusqlite::Result<Vec<i64>>>);
        assert!(
            bare.is_err(),
            "raw `current.md` should error in FTS5 — if this passes, the \
             quoting sanitizer is no longer load-bearing and the test above \
             proves nothing"
        );
    }

    /// Regression for the live-found hyphen bug: searching `ui-refresh`
    /// returned nothing in prod even though
    /// `follow-ups/ui-refresh-scroll-restoration.md` exists. The first fix
    /// quoted it as `"ui-refresh"`, which **does not error but also does not
    /// match** the indexed `ui refresh` — only `"ui refresh"` (sub-token
    /// phrase) does. The string-level test can't see this; this drives real
    /// FTS5 against the real `path_search` index. It would have caught the
    /// bug the dotted-only fix left behind.
    #[test]
    fn hyphenated_filename_search_matches_indexed_path() {
        let (_tmp, mut conn, ws, proj) = fresh_db();
        upsert_page(
            &mut conn,
            &page(
                ws,
                proj,
                "follow-ups/ui-refresh-scroll-restoration.md",
                "body text",
            ),
        )
        .unwrap();

        let hits = fts_match_paths(&conn, "ui-refresh")
            .expect("hyphenated search must not raise an FTS5 syntax error");
        assert!(
            hits.iter()
                .any(|p| p == "follow-ups/ui-refresh-scroll-restoration.md"),
            "search for `ui-refresh` should match the indexed path; got {hits:?}"
        );

        // Pin the exact FTS5 quirk the fix works around: the keeps-the-hyphen
        // phrase matches nothing, the spaces phrase matches. If this ever
        // flips, the sub-token quoting is no longer load-bearing.
        let count = |q: &str| -> i64 {
            conn.query_row(
                "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH ?1",
                params![q],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            count("\"ui-refresh\""),
            0,
            "kept-hyphen phrase must not match"
        );
        assert_eq!(count("\"ui refresh\""), 1, "sub-token phrase must match");
    }

    /// Issue #103 legacy heal: the two broad sentinels ($HOME and `/`) are
    /// always NULLed. Here the inputs are non-existent fake paths, so the
    /// nested row is preserved by the multi-user/unmounted safety rule (a
    /// `repo_path` absent on this host is left alone), not merely because it
    /// is non-sentinel -- the filesystem broadening is covered separately by
    /// `heal_catch_all_repo_paths_uses_filesystem_to_judge_catch_alls`.
    #[test]
    fn heal_catch_all_repo_paths_nulls_home_and_root_only() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();

        let read_repo_path = |conn: &Connection, id: &ProjectId| -> Option<String> {
            conn.query_row(
                "SELECT repo_path FROM projects WHERE id = ?1",
                params![id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap()
        };

        let home = get_or_create_project(&mut conn, &ws, "home", Some("/home/tester")).unwrap();
        let root = get_or_create_project(&mut conn, &ws, "root", Some("/")).unwrap();
        let nested =
            get_or_create_project(&mut conn, &ws, "app", Some("/home/tester/projects/app"))
                .unwrap();
        let none = get_or_create_project(&mut conn, &ws, "none", None).unwrap();

        let healed = heal_catch_all_repo_paths(&mut conn, Some("/home/tester")).unwrap();
        assert_eq!(healed, 2, "only the $HOME and `/` rows should be healed");

        assert_eq!(
            read_repo_path(&conn, &home),
            None,
            "$HOME row must be NULLed"
        );
        assert_eq!(read_repo_path(&conn, &root), None, "`/` row must be NULLed");
        assert_eq!(
            read_repo_path(&conn, &nested),
            Some("/home/tester/projects/app".to_string()),
            "nested fake path does not exist on disk, so the safety rule preserves it"
        );
        assert_eq!(read_repo_path(&conn, &none), None, "NULL row stays NULL");

        // Idempotent: a second pass over a healed DB changes nothing.
        assert_eq!(
            heal_catch_all_repo_paths(&mut conn, Some("/home/tester")).unwrap(),
            0,
            "re-running the heal must be a no-op"
        );
    }

    /// Filesystem broadening of the #103 heal: a real cwd captured as a
    /// catch-all (a non-git ancestor like `~/projects`) is NULLed, a real git
    /// work-tree root is preserved, and a path absent on this host is left
    /// alone (multi-user/unmounted safety).
    #[test]
    fn heal_catch_all_repo_paths_uses_filesystem_to_judge_catch_alls() {
        let (_tmp, mut conn, ws, _proj) = fresh_db();
        let fixtures = TempDir::new().unwrap();

        let read_repo_path = |conn: &Connection, id: &ProjectId| -> Option<String> {
            conn.query_row(
                "SELECT repo_path FROM projects WHERE id = ?1",
                params![id.as_bytes()],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Real non-git ancestor (the legacy `~/projects`-style catch-all).
        let non_git = fixtures.path().join("projects");
        std::fs::create_dir_all(&non_git).unwrap();
        let non_git_path = non_git.to_str().unwrap().to_string();
        let non_git_proj =
            get_or_create_project(&mut conn, &ws, "non_git", Some(&non_git_path)).unwrap();

        // Real git work-tree root (legitimate prefix key).
        let git_root = fixtures.path().join("repo");
        std::fs::create_dir_all(git_root.join(".git")).unwrap();
        let git_path = git_root.to_str().unwrap().to_string();
        let git_proj = get_or_create_project(&mut conn, &ws, "git_root", Some(&git_path)).unwrap();

        // Path absent on this host (do NOT create it).
        let gone = fixtures.path().join("gone");
        let gone_path = gone.to_str().unwrap().to_string();
        let gone_proj = get_or_create_project(&mut conn, &ws, "gone", Some(&gone_path)).unwrap();

        let healed = heal_catch_all_repo_paths(&mut conn, None).unwrap();
        assert_eq!(
            healed, 1,
            "only the real non-git catch-all should be healed"
        );

        assert_eq!(
            read_repo_path(&conn, &non_git_proj),
            None,
            "real non-git ancestor must be healed"
        );
        assert_eq!(
            read_repo_path(&conn, &git_proj),
            Some(normalize_repo_path_key(&git_path)),
            "real git work-tree root must be preserved"
        );
        assert_eq!(
            read_repo_path(&conn, &gone_proj),
            Some(normalize_repo_path_key(&gone_path)),
            "path absent on this host must be preserved (multi-user/unmounted safety)"
        );
    }
}
