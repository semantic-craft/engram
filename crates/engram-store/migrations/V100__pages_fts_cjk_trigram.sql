-- V100 opens the FORK-LOCAL migration band: upstream ai-memory owns V1–V99
-- (it is at V28 and counting), engram-only migrations start at V100 so future
-- upstream cherry-picks never collide on a version number. refinery orders
-- numerically and tolerates gaps.
--
-- CJK shadow FTS index (#14). The primary `pages_fts` uses unicode61, which
-- tokenizes a CJK run as ONE long token — any Chinese query has zero FTS
-- recall, leaving zero-embedding installs blind for Chinese text. This
-- content-backed trigram shadow gives ≥3-char CJK substrings real
-- MATCH/bm25/snippet; 1–2 char CJK terms are served by a LIKE fallback leg in
-- the query router (trigram cannot match substrings under 3 chars by design).
--
-- The shadow is an ADDITION, not a replacement: swapping `pages_fts` itself to
-- trigram would regress short-English word queries (`ai`, `go`) to zero and
-- change `mem`-style queries to substring semantics. unicode61 keeps word
-- semantics; the router picks legs per term.
--
-- Deliberately NO `remove_diacritics` / `case_sensitive` options: trigram
-- defaults keep the index usable for FTS5's indexed-LIKE optimization should
-- the router ever want it. `path_search` stays unicode61-only (paths are
-- ASCII).

CREATE VIRTUAL TABLE pages_fts_cjk USING fts5(
    title, body,
    content='pages',
    content_rowid='rowid',
    tokenize='trigram'
);

CREATE TRIGGER pages_fts_cjk_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts_cjk(rowid, title, body)
        VALUES (new.rowid, new.title, new.body);
END;

CREATE TRIGGER pages_fts_cjk_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts_cjk(pages_fts_cjk, rowid, title, body)
        VALUES ('delete', old.rowid, old.title, old.body);
END;

CREATE TRIGGER pages_fts_cjk_au AFTER UPDATE OF title, body ON pages BEGIN
    INSERT INTO pages_fts_cjk(pages_fts_cjk, rowid, title, body)
        VALUES ('delete', old.rowid, old.title, old.body);
    INSERT INTO pages_fts_cjk(rowid, title, body)
        VALUES (new.rowid, new.title, new.body);
END;

INSERT INTO pages_fts_cjk(pages_fts_cjk) VALUES('rebuild');
