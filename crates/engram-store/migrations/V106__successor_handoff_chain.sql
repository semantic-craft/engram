-- Successor Handoff chains. A receiving agent publishes the next transfer
-- from the latest durable Checkpoint instead of overwriting the offer it
-- received, so one WorkItem can cross many agents and Runs while every
-- earlier transfer stays readable history.
--
-- Upgrading also retires any transfer left live on already-terminal work; see
-- the backfill at the end of this file.
--
-- `state` gains `superseded` (an unclaimed predecessor replaced by a
-- successor) and `cancelled` (source-owner discard). Splitting `cancelled`
-- out of `expired` keeps the four terminal outcomes - cancellation, claim
-- release, lease expiry, and supersession - distinguishable in the state
-- machine as well as in `audit_log`.
--
-- SQLite cannot widen a CHECK in place, so `handoffs` is rebuilt with the
-- documented create/copy/drop/rename procedure. `Store::open` keeps
-- `foreign_keys` OFF for the whole migration run, so the dependent
-- `handoff_claims` / `checkpoints` / `continuity_attempts` rows survive the
-- swap and their `REFERENCES handoffs(id)` clauses bind again to the renamed
-- table. `predecessor_handoff_id` is declared against `handoffs` while the
-- old table still exists; after the rename it is the self-reference the
-- chain needs.

CREATE TABLE handoffs_v106 (
    id                        BLOB PRIMARY KEY NOT NULL,
    work_item_id              BLOB NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    workspace_id              BLOB NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    project_id                BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    from_session_id           BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    source_run_id             BLOB NOT NULL,
    from_agent                TEXT NOT NULL,
    source_actor              TEXT NOT NULL,
    to_agent                  TEXT,
    cwd                       TEXT,
    summary                   TEXT NOT NULL,
    open_questions            TEXT NOT NULL DEFAULT '[]',
    next_steps                TEXT NOT NULL DEFAULT '[]',
    files_touched             TEXT NOT NULL DEFAULT '[]',
    state                     TEXT NOT NULL DEFAULT 'open'
                              CHECK (state IN ('open','claimed','acknowledged','expired',
                                               'cancelled','superseded')),
    revision                  INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at                INTEGER NOT NULL,
    acknowledged_by           TEXT,
    acknowledged_at           INTEGER,
    acknowledged_by_session   BLOB REFERENCES sessions(id) ON DELETE SET NULL,
    -- Bounded continuation brief and revisioned ContextRefs (V104).
    brief                     TEXT NOT NULL DEFAULT '',
    context_refs              TEXT NOT NULL DEFAULT '[]',
    -- Transfer chain. A successor names the predecessor it continues and the
    -- exact Checkpoint revision it was constructed from; the predecessor
    -- names the successor that superseded it, when it was still unclaimed.
    predecessor_handoff_id    BLOB REFERENCES handoffs(id) ON DELETE SET NULL,
    source_checkpoint_id      BLOB REFERENCES checkpoints(id) ON DELETE SET NULL,
    source_checkpoint_revision INTEGER,
    superseded_by_handoff_id  BLOB REFERENCES handoffs(id) ON DELETE SET NULL,
    superseded_at             INTEGER
);

INSERT INTO handoffs_v106 (
    id, work_item_id, workspace_id, project_id, from_session_id, source_run_id,
    from_agent, source_actor, to_agent, cwd, summary, open_questions, next_steps,
    files_touched, state, revision, created_at, acknowledged_by, acknowledged_at,
    acknowledged_by_session, brief, context_refs
)
SELECT id, work_item_id, workspace_id, project_id, from_session_id, source_run_id,
       from_agent, source_actor, to_agent, cwd, summary, open_questions, next_steps,
       files_touched, state, revision, created_at, acknowledged_by, acknowledged_at,
       acknowledged_by_session, brief, context_refs
FROM handoffs;

DROP TABLE handoffs;
ALTER TABLE handoffs_v106 RENAME TO handoffs;

CREATE INDEX idx_handoffs_claimable_recent
    ON handoffs(workspace_id, project_id, created_at DESC)
    WHERE state IN ('open','claimed');
CREATE INDEX idx_handoffs_work_item
    ON handoffs(work_item_id, created_at DESC);
CREATE INDEX idx_handoffs_cwd_claimable
    ON handoffs(workspace_id, project_id, cwd, created_at DESC)
    WHERE state IN ('open','claimed');
-- Chain traversal walks predecessor links inside one WorkItem.
CREATE INDEX idx_handoffs_predecessor
    ON handoffs(predecessor_handoff_id)
    WHERE predecessor_handoff_id IS NOT NULL;

CREATE TRIGGER handoffs_ws_proj_pairing_ai
BEFORE INSERT ON handoffs
FOR EACH ROW
WHEN NEW.workspace_id IS NOT (SELECT workspace_id FROM projects WHERE id = NEW.project_id)
BEGIN
    SELECT RAISE(ABORT, 'handoffs.workspace_id does not match the project''s workspace');
END;

-- Backfill the invariant this release starts enforcing at checkpoint time.
-- A store upgraded from V105 can already hold `open` or `claimed` transfers on
-- a WorkItem that was completed or abandoned before terminal retirement
-- existed. Left as they are, those rows land in exactly the dead end the
-- runtime change closes: once the lease lapses discovery offers the transfer
-- again, while claiming rejects the terminal WorkItem, releasing rejects the
-- expired lease, and cancelling rejects the non-`open` state. Retire them and
-- resolve their leases here so no upgraded store starts out stranded.
UPDATE handoffs
   SET state = 'expired', revision = revision + 1
 WHERE state IN ('open', 'claimed')
   AND work_item_id IN (
       SELECT id FROM work_items WHERE state IN ('completed', 'abandoned')
   );

UPDATE handoff_claims
   SET state = 'expired',
       resolved_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000000
 WHERE state = 'live'
   AND work_item_id IN (
       SELECT id FROM work_items WHERE state IN ('completed', 'abandoned')
   );
