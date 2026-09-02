-- Recoverable WorkItem continuity replaces the single-use handoff state
-- machine. Existing handoffs become one-WorkItem revision histories; runtime
-- code has no branch for the old accepted-on-read contract.

CREATE TABLE work_items (
    id                  BLOB PRIMARY KEY NOT NULL,
    workspace_id        BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id          BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    objective           TEXT NOT NULL,
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    state               TEXT NOT NULL DEFAULT 'active'
                        CHECK (state IN ('active','blocked','completed','abandoned')),
    revision            INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    owner_actor         TEXT NOT NULL,
    owner_run_id        BLOB,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX idx_work_items_scope_state
    ON work_items(workspace_id, project_id, state, updated_at DESC);

-- Preserve each legacy Handoff as a distinct WorkItem. Reusing the legacy
-- handoff UUID bytes for the generated WorkItem is deterministic and makes a
-- partially interrupted upgrade impossible to duplicate semantic rows.
INSERT INTO work_items (
    id, workspace_id, project_id, objective, acceptance_criteria, state,
    revision, owner_actor, owner_run_id, created_at, updated_at
)
SELECT id, workspace_id, project_id, summary, '[]', 'active', 1,
       from_agent, COALESCE(from_session_id, id), created_at,
       COALESCE(accepted_at, created_at)
FROM handoffs;

ALTER TABLE handoffs RENAME TO handoffs_legacy_v102;

CREATE TABLE handoffs (
    id                      BLOB PRIMARY KEY NOT NULL,
    work_item_id            BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    workspace_id            BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id              BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    from_session_id         BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    source_run_id           BLOB NOT NULL,
    from_agent              TEXT NOT NULL,
    source_actor            TEXT NOT NULL,
    to_agent                TEXT,
    cwd                     TEXT,
    summary                 TEXT NOT NULL,
    open_questions          TEXT NOT NULL DEFAULT '[]',
    next_steps              TEXT NOT NULL DEFAULT '[]',
    files_touched           TEXT NOT NULL DEFAULT '[]',
    state                   TEXT NOT NULL DEFAULT 'open'
                            CHECK (state IN ('open','claimed','acknowledged','expired')),
    revision                INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at              INTEGER NOT NULL,
    acknowledged_by         TEXT,
    acknowledged_at         INTEGER,
    acknowledged_by_session BLOB REFERENCES sessions(id) ON DELETE SET NULL
);

INSERT INTO handoffs (
    id, work_item_id, workspace_id, project_id, from_session_id, source_run_id, from_agent,
    source_actor, to_agent, cwd, summary, open_questions, next_steps,
    files_touched, state, revision, created_at, acknowledged_by,
    acknowledged_at, acknowledged_by_session
)
SELECT id, id, workspace_id, project_id, from_session_id,
       COALESCE(from_session_id, id), from_agent,
       from_agent, to_agent, cwd, summary, open_questions, next_steps,
       files_touched,
       CASE state WHEN 'accepted' THEN 'acknowledged' ELSE state END,
       1, created_at, accepted_by, accepted_at, accepted_by_session
FROM handoffs_legacy_v102;

DROP TABLE handoffs_legacy_v102;

CREATE INDEX idx_handoffs_claimable_recent
    ON handoffs(workspace_id, project_id, created_at DESC)
    WHERE state IN ('open','claimed');
CREATE INDEX idx_handoffs_work_item
    ON handoffs(work_item_id, created_at DESC);
CREATE INDEX idx_handoffs_cwd_claimable
    ON handoffs(workspace_id, project_id, cwd, created_at DESC)
    WHERE state IN ('open','claimed');

CREATE TABLE handoff_claims (
    id               BLOB PRIMARY KEY NOT NULL,
    handoff_id       BLOB NOT NULL REFERENCES handoffs(id) ON DELETE CASCADE,
    work_item_id     BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    workspace_id     BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id       BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    handoff_revision INTEGER NOT NULL,
    actor_key        TEXT NOT NULL,
    run_id           BLOB NOT NULL,
    state            TEXT NOT NULL CHECK (state IN ('live','released','expired','acknowledged')),
    claimed_at       INTEGER NOT NULL,
    lease_expires_at INTEGER NOT NULL,
    resolved_at      INTEGER
);

CREATE UNIQUE INDEX idx_handoff_claims_one_live
    ON handoff_claims(handoff_id) WHERE state = 'live';
CREATE INDEX idx_handoff_claims_lease
    ON handoff_claims(state, lease_expires_at) WHERE state = 'live';

CREATE TABLE checkpoints (
    id                  BLOB PRIMARY KEY NOT NULL,
    work_item_id        BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    workspace_id        BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id          BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id              BLOB NOT NULL,
    handoff_id          BLOB REFERENCES handoffs(id) ON DELETE SET NULL,
    sequence            INTEGER NOT NULL CHECK (sequence >= 1),
    work_item_revision  INTEGER NOT NULL CHECK (work_item_revision >= 1),
    summary             TEXT NOT NULL,
    work_item_state     TEXT NOT NULL
                        CHECK (work_item_state IN ('active','blocked','completed','abandoned')),
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    actor_key           TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    UNIQUE (work_item_id, sequence)
);

CREATE INDEX idx_checkpoints_work_item
    ON checkpoints(work_item_id, sequence);

-- Retry resolution is intentionally operational SQLite state. `outcome_json`
-- may contain the opaque claim id because an identical lost-response retry
-- must receive the original result; audit_log detail never contains it.
CREATE TABLE continuity_attempts (
    id                           BLOB PRIMARY KEY NOT NULL,
    operation                    TEXT NOT NULL
                                 CHECK (operation IN ('claim','release','checkpoint')),
    actor_key                    TEXT NOT NULL,
    workspace_id                 BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id                   BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    work_item_id                 BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    handoff_id                   BLOB REFERENCES handoffs(id) ON DELETE CASCADE,
    expected_work_item_revision  INTEGER,
    expected_handoff_revision    INTEGER,
    request_digest               BLOB NOT NULL CHECK (length(request_digest) = 32),
    outcome_json                 TEXT NOT NULL,
    created_at                   INTEGER NOT NULL
);

CREATE INDEX idx_continuity_attempts_target
    ON continuity_attempts(workspace_id, project_id, work_item_id, created_at DESC);

-- Re-establish V18's insert-time workspace/project invariant after rebuilding
-- `handoffs`, and extend it to every new scope-bearing continuity table.
CREATE TRIGGER work_items_ws_proj_pairing_ai
BEFORE INSERT ON work_items
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'work_items.workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER handoffs_ws_proj_pairing_ai
BEFORE INSERT ON handoffs
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'handoffs.workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER handoff_claims_ws_proj_pairing_ai
BEFORE INSERT ON handoff_claims
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'handoff_claims.workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER checkpoints_ws_proj_pairing_ai
BEFORE INSERT ON checkpoints
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'checkpoints.workspace_id does not match the project''s workspace');
END;

CREATE TRIGGER continuity_attempts_ws_proj_pairing_ai
BEFORE INSERT ON continuity_attempts
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'continuity_attempts.workspace_id does not match the project''s workspace');
END;
