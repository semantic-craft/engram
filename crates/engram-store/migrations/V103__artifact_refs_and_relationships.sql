-- Typed ArtifactRefs and explicit WorkItem relationships. Engram records
-- observed delivery status; it never mutates Git or external systems.

CREATE TABLE artifacts (
    id                  BLOB PRIMARY KEY NOT NULL,
    identity_hash       BLOB NOT NULL UNIQUE CHECK (length(identity_hash) = 32),
    kind                TEXT NOT NULL CHECK (kind IN ('file','git','worktree','external')),
    locator             TEXT NOT NULL,
    observed_revision   TEXT,
    content_hash        TEXT,
    repository_identity TEXT,
    git_ref             TEXT,
    commit_id           TEXT,
    tree_hash           TEXT,
    workspace_id        BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id          BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_artifacts_scope
    ON artifacts(workspace_id, project_id, kind);

CREATE TABLE artifact_attachments (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    artifact_id     BLOB NOT NULL REFERENCES artifacts(id) ON DELETE CASCADE,
    owner_kind      TEXT NOT NULL CHECK (owner_kind IN ('handoff','checkpoint','parent_result')),
    owner_id        BLOB NOT NULL,
    source_run_id   BLOB NOT NULL,
    observed_at     INTEGER NOT NULL,
    provenance      TEXT NOT NULL,
    dirty           INTEGER CHECK (dirty IS NULL OR dirty IN (0,1)),
    local_path_hint TEXT,
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

CREATE INDEX idx_artifact_attachments_owner
    ON artifact_attachments(owner_kind, owner_id);

CREATE TABLE artifact_verification (
    id                  BLOB PRIMARY KEY NOT NULL,
    attachment_id       INTEGER NOT NULL REFERENCES artifact_attachments(id) ON DELETE CASCADE,
    check_name          TEXT NOT NULL,
    observed_result     TEXT NOT NULL,
    observed_at         INTEGER NOT NULL,
    source_run_id       BLOB NOT NULL,
    applies_to_revision TEXT NOT NULL
);

CREATE TABLE work_item_relationships (
    id                  BLOB PRIMARY KEY NOT NULL,
    kind                TEXT NOT NULL CHECK (kind IN ('depends_on','derived_from','child_of')),
    from_work_item_id   BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    to_work_item_id     BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    from_workspace_id   BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    from_project_id     BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    to_workspace_id     BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    to_project_id       BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_by_actor    TEXT NOT NULL,
    created_by_run_id   BLOB NOT NULL,
    created_at          INTEGER NOT NULL,
    UNIQUE (kind, from_work_item_id, to_work_item_id),
    CHECK (from_work_item_id != to_work_item_id)
);

CREATE INDEX idx_work_item_relationships_from
    ON work_item_relationships(from_work_item_id, kind);
CREATE INDEX idx_work_item_relationships_to
    ON work_item_relationships(to_work_item_id, kind);

CREATE TABLE parent_results (
    id                  BLOB PRIMARY KEY NOT NULL,
    child_work_item_id  BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    parent_work_item_id BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    checkpoint_id       BLOB NOT NULL REFERENCES checkpoints(id) ON DELETE CASCADE,
    summary             TEXT NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE INDEX idx_parent_results_parent
    ON parent_results(parent_work_item_id, created_at DESC);

CREATE TRIGGER artifacts_ws_proj_pairing_ai
BEFORE INSERT ON artifacts
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'artifacts.workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER work_item_relationships_from_ws_proj_pairing_ai
BEFORE INSERT ON work_item_relationships
FOR EACH ROW
WHEN NEW.from_workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.from_project_id)
BEGIN
    SELECT RAISE(ABORT, 'work_item_relationships.from_workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER work_item_relationships_to_ws_proj_pairing_ai
BEFORE INSERT ON work_item_relationships
FOR EACH ROW
WHEN NEW.to_workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.to_project_id)
BEGIN
    SELECT RAISE(ABORT, 'work_item_relationships.to_workspace_id does not match the project''s workspace');
END;
