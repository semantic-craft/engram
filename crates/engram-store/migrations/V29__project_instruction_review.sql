-- Human review state for project-instruction proposals. Wiki proposal rows keep
-- their existing approval/apply semantics and leave these columns NULL.

ALTER TABLE auto_improve_proposals
    ADD COLUMN base_content TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN approval_sha256 BLOB;

CREATE TABLE project_instruction_proposal_revisions (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    proposal_id              BLOB NOT NULL REFERENCES auto_improve_proposals(id) ON DELETE CASCADE,
    revision                 INTEGER NOT NULL,
    proposed_content         TEXT NOT NULL,
    proposed_content_sha256  BLOB NOT NULL,
    unified_diff             TEXT NOT NULL,
    estimated_token_delta    INTEGER NOT NULL,
    approval_sha256          BLOB NOT NULL,
    actor_json               TEXT NOT NULL,
    author_id                BLOB REFERENCES users(id) ON DELETE SET NULL,
    at                       INTEGER NOT NULL,
    UNIQUE(proposal_id, revision)
);

CREATE INDEX idx_project_instruction_revisions_proposal
    ON project_instruction_proposal_revisions(proposal_id, revision ASC);
