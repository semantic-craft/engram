-- Immutable audit record for a locally-applied project-instruction proposal.
-- Repository mutation remains outside the server; this table records the
-- result after the local host has completed its compare-and-swap write.

CREATE TABLE project_instruction_applications (
    proposal_id           BLOB PRIMARY KEY NOT NULL
        REFERENCES auto_improve_proposals(id) ON DELETE CASCADE,
    approval_sha256       BLOB NOT NULL,
    before_sha256         BLOB NOT NULL,
    after_sha256          BLOB NOT NULL,
    outcome               TEXT NOT NULL
        CHECK (outcome IN ('created', 'updated', 'no_op')),
    backup_path           TEXT,
    proposing_actor_json  TEXT NOT NULL,
    approving_actor_json  TEXT NOT NULL,
    applying_actor_json   TEXT NOT NULL,
    applied_by_author_id  BLOB REFERENCES users(id) ON DELETE SET NULL,
    applied_at            INTEGER NOT NULL
);

-- An application has exactly one terminal audit event. The application row
-- itself is already proposal-unique; this index keeps the shared event stream
-- equally strict without changing proposal lifecycle status.
CREATE UNIQUE INDEX idx_auto_improve_one_applied_event
    ON auto_improve_proposal_events(proposal_id, event)
    WHERE event = 'applied';
