-- Add proposal-only project-instruction targets without changing existing
-- Wiki proposal rows or their approval path.

ALTER TABLE auto_improve_proposals
    ADD COLUMN target_kind TEXT NOT NULL DEFAULT 'wiki_page'
        CHECK (target_kind IN ('wiki_page', 'project_instruction'));

ALTER TABLE auto_improve_proposals
    ADD COLUMN proposal_operation TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN logical_target TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN target_context_layer TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN base_sha256 BLOB;

ALTER TABLE auto_improve_proposals
    ADD COLUMN boundary_kind TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN boundary_value TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN unified_diff TEXT;

ALTER TABLE auto_improve_proposals
    ADD COLUMN estimated_token_delta INTEGER;

ALTER TABLE auto_improve_proposals
    ADD COLUMN provenance_json TEXT NOT NULL DEFAULT '[]';
