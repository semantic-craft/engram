-- Shared artifact identity is global (repository + revision, or file
-- locator). Scope lifecycle and per-observation fields belong on
-- attachments so deleting the first writer's project cannot CASCADE
-- through a shared identity row into another project's evidence.
-- Engram records observed status; it never mutates Git or external systems.

PRAGMA foreign_keys = OFF;

DROP TRIGGER IF EXISTS artifacts_ws_proj_pairing_ai;

CREATE TABLE artifacts_new (
    id                  BLOB PRIMARY KEY NOT NULL,
    identity_hash       BLOB NOT NULL UNIQUE CHECK (length(identity_hash) = 32),
    kind                TEXT NOT NULL CHECK (kind IN ('file','git','worktree','external')),
    locator             TEXT NOT NULL,
    observed_revision   TEXT,
    repository_identity TEXT,
    commit_id           TEXT
);

INSERT INTO artifacts_new (
    id, identity_hash, kind, locator, observed_revision, repository_identity, commit_id
)
SELECT id, identity_hash, kind, locator, observed_revision, repository_identity, commit_id
FROM artifacts;

CREATE TABLE artifact_attachments_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    artifact_id     BLOB NOT NULL REFERENCES artifacts_new(id) ON DELETE CASCADE,
    owner_kind      TEXT NOT NULL CHECK (owner_kind IN ('handoff','checkpoint','parent_result')),
    owner_id        BLOB NOT NULL,
    source_run_id   BLOB NOT NULL,
    observed_at     INTEGER NOT NULL,
    provenance      TEXT NOT NULL,
    content_hash    TEXT,
    git_ref         TEXT,
    tree_hash       TEXT,
    dirty           INTEGER CHECK (dirty IS NULL OR dirty IN (0,1)),
    local_path_hint TEXT,
    workspace_id    BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id      BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    fact_changed    INTEGER NOT NULL DEFAULT 0 CHECK (fact_changed IN (0,1)),
    fact_verified   INTEGER NOT NULL DEFAULT 0 CHECK (fact_verified IN (0,1)),
    fact_committed  INTEGER NOT NULL DEFAULT 0 CHECK (fact_committed IN (0,1)),
    fact_pushed     INTEGER NOT NULL DEFAULT 0 CHECK (fact_pushed IN (0,1)),
    fact_reviewed   INTEGER NOT NULL DEFAULT 0 CHECK (fact_reviewed IN (0,1)),
    fact_merged     INTEGER NOT NULL DEFAULT 0 CHECK (fact_merged IN (0,1)),
    fact_released   INTEGER NOT NULL DEFAULT 0 CHECK (fact_released IN (0,1)),
    fact_deployed   INTEGER NOT NULL DEFAULT 0 CHECK (fact_deployed IN (0,1)),
    fact_submitted  INTEGER NOT NULL DEFAULT 0 CHECK (fact_submitted IN (0,1)),
    fact_approved   INTEGER NOT NULL DEFAULT 0 CHECK (fact_approved IN (0,1)),
    UNIQUE (owner_kind, owner_id, artifact_id)
);

INSERT INTO artifact_attachments_new (
    id, artifact_id, owner_kind, owner_id, source_run_id, observed_at, provenance,
    content_hash, git_ref, tree_hash, dirty, local_path_hint, workspace_id, project_id,
    fact_changed, fact_verified, fact_committed, fact_pushed, fact_reviewed, fact_merged,
    fact_released, fact_deployed, fact_submitted, fact_approved
)
-- Scope comes from each attachment's own owner, never from the shared
-- identity row. One identity can already be attached in several projects, and
-- taking the identity's (first writer's) scope would leave every other
-- project's attachment cascading off that project -- the exact loss this
-- migration exists to stop. The identity's scope is only a last-resort default
-- for an attachment whose owner row is already gone.
SELECT t.id, t.artifact_id, t.owner_kind, t.owner_id, t.source_run_id, t.observed_at,
       t.provenance, a.content_hash, a.git_ref, a.tree_hash, t.dirty, t.local_path_hint,
       COALESCE(h.workspace_id, c.workspace_id, pc.workspace_id, a.workspace_id),
       COALESCE(h.project_id, c.project_id, pc.project_id, a.project_id),
       t.fact_changed, t.fact_verified, t.fact_committed,
       t.fact_pushed, t.fact_reviewed, t.fact_merged, t.fact_released, t.fact_deployed,
       t.fact_submitted, t.fact_approved
FROM artifact_attachments t
JOIN artifacts a ON a.id = t.artifact_id
LEFT JOIN handoffs h
       ON t.owner_kind = 'handoff' AND h.id = t.owner_id
LEFT JOIN checkpoints c
       ON t.owner_kind = 'checkpoint' AND c.id = t.owner_id
LEFT JOIN parent_results p
       ON t.owner_kind = 'parent_result' AND p.id = t.owner_id
LEFT JOIN checkpoints pc
       ON pc.id = p.checkpoint_id;

CREATE TABLE artifact_verification_new (
    id                  BLOB PRIMARY KEY NOT NULL,
    attachment_id       INTEGER NOT NULL REFERENCES artifact_attachments_new(id) ON DELETE CASCADE,
    check_name          TEXT NOT NULL,
    observed_result     TEXT NOT NULL,
    observed_at         INTEGER NOT NULL,
    source_run_id       BLOB NOT NULL,
    applies_to_revision TEXT NOT NULL
);

INSERT INTO artifact_verification_new
SELECT * FROM artifact_verification;

DROP TABLE artifact_verification;
DROP TABLE artifact_attachments;
DROP TABLE artifacts;

ALTER TABLE artifacts_new RENAME TO artifacts;
ALTER TABLE artifact_attachments_new RENAME TO artifact_attachments;
ALTER TABLE artifact_verification_new RENAME TO artifact_verification;

CREATE INDEX idx_artifact_attachments_owner
    ON artifact_attachments(owner_kind, owner_id);
CREATE INDEX idx_artifact_attachments_scope
    ON artifact_attachments(workspace_id, project_id, artifact_id);

CREATE TRIGGER artifact_attachments_ws_proj_pairing_ai
BEFORE INSERT ON artifact_attachments
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'artifact_attachments.workspace_id does not match the project''s workspace');
END;

PRAGMA foreign_keys = ON;
