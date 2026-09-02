-- Attach a bounded continuation brief and revisioned ContextRefs to Handoffs.
-- Canonical wiki/observation bodies stay in their source rows; claim-time
-- assembly resolves these locators. V103 is reserved for ArtifactRefs (#42).

ALTER TABLE handoffs ADD COLUMN brief TEXT NOT NULL DEFAULT '';
ALTER TABLE handoffs ADD COLUMN context_refs TEXT NOT NULL DEFAULT '[]';

UPDATE handoffs SET brief = summary WHERE brief = '';
