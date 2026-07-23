-- Long-document chunked embeddings.
--
-- Pages larger than one embedding request used to be head-truncated
-- (OpenAI path) or rejected (Voyage), so only a page's first few KB
-- reached the vector index. `embed_document_chunked` now stores one
-- vector per markdown-aligned chunk; this rebuild widens the table's
-- primary key from `page_id` to `(page_id, chunk_index)` (SQLite
-- cannot alter a PK in place). Existing single-vector rows become
-- chunk 0. `(provider, model, dim)` stays denormalised per row for
-- the same heterogeneity-detection reason as V04.
--
-- Numbered V101 (not V28): refinery only applies versions above the
-- store's high-water mark, and deployed stores are already at V100.

CREATE TABLE page_embeddings_v101 (
    page_id     BLOB NOT NULL REFERENCES pages(id) ON DELETE CASCADE,
    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
    vector      BLOB NOT NULL,
    provider    TEXT NOT NULL,
    model       TEXT NOT NULL,
    dim         INTEGER NOT NULL CHECK (dim > 0),
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (page_id, chunk_index)
);

INSERT INTO page_embeddings_v101
    (page_id, chunk_index, vector, provider, model, dim, created_at)
SELECT page_id, 0, vector, provider, model, dim, created_at
FROM page_embeddings;

DROP TABLE page_embeddings;

ALTER TABLE page_embeddings_v101 RENAME TO page_embeddings;

-- Same roles as the V04 index: refuse-on-mismatch startup check +
-- `engram embed` stale-triple scans.
CREATE INDEX idx_page_embeddings_provider
    ON page_embeddings(provider, model, dim);
