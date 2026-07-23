//! refinery-driven schema migrations.

use crate::error::{StoreError, StoreResult};

refinery::embed_migrations!("migrations");

/// Run all pending migrations against an open connection.
///
/// # Errors
/// Propagates the underlying refinery error if a migration fails. When the
/// store is *ahead* of this binary — an applied migration is absent from the
/// compiled-in set, which refinery reports as `MissingVersion` with the
/// misleading text "migration V… is missing from the filesystem" — the error
/// is remapped to [`StoreError::DataSchemaAhead`], which names the offending
/// migration and points the operator at the fix.
pub fn run(conn: &mut rusqlite::Connection) -> StoreResult<()> {
    migrations::runner().run(conn).map_err(classify_run_error)?;
    Ok(())
}

/// Highest schema version baked into this binary (the max embedded migration).
fn max_supported_version() -> u32 {
    migrations::runner()
        .get_migrations()
        .iter()
        .map(refinery::Migration::version)
        .max()
        .unwrap_or(0)
}

/// Translate refinery's raw error into a store-domain error. The only variant
/// reshaped is `MissingVersion` (the store's schema is ahead of this binary);
/// every other refinery failure passes through as [`StoreError::Migration`].
fn classify_run_error(err: refinery::Error) -> StoreError {
    if let refinery::error::Kind::MissingVersion(applied) = err.kind() {
        return StoreError::DataSchemaAhead {
            applied: format!("V{} ({})", applied.version(), applied.name()),
            supported: max_supported_version(),
        };
    }
    StoreError::Migration(err)
}

#[cfg(test)]
pub(crate) fn run_to(conn: &mut rusqlite::Connection, target: u32) -> Result<(), refinery::Error> {
    migrations::runner()
        .set_target(refinery::Target::Version(target))
        .run(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    /// A store migrated by a newer build (an applied version above anything
    /// this binary embeds) must fail to open with the actionable
    /// `DataSchemaAhead` error, not refinery's raw "missing from the
    /// filesystem" wording.
    #[test]
    fn data_ahead_of_binary_reports_schema_ahead_not_raw_refinery() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("memory.sqlite");
        let mut conn = Connection::open(&db_path).unwrap();

        // Bring the store up to this binary's current schema.
        run(&mut conn).unwrap();

        // Simulate data written by a *newer* build: forge an applied migration
        // whose version sits above the embedded ceiling. refinery stores
        // `applied_on` as RFC3339 and `checksum` as a u64 string, and parses
        // both eagerly, so the row must be well-formed.
        let future = max_supported_version() + 100;
        conn.execute(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) \
             VALUES (?1, ?2, ?3, ?4)",
            params![future, "future_feature", "2026-07-14T00:00:00Z", "0"],
        )
        .unwrap();

        let err = run(&mut conn).unwrap_err();
        match err {
            StoreError::DataSchemaAhead { applied, supported } => {
                assert!(applied.contains(&format!("V{future}")), "applied={applied}");
                assert!(applied.contains("future_feature"), "applied={applied}");
                assert_eq!(supported, max_supported_version());
            }
            other => panic!("expected DataSchemaAhead, got: {other:?}"),
        }
    }

    /// V101 rebuilds `page_embeddings` for chunked embeddings: legacy
    /// single-vector rows must survive as `chunk_index = 0`, and the
    /// widened `(page_id, chunk_index)` PK must accept a second chunk
    /// for the same page.
    #[test]
    fn v101_promotes_single_vector_rows_to_chunk_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut conn = Connection::open(tmp.path().join("memory.sqlite")).unwrap();
        // Mirror Store::open: FK enforcement stays off while migrations
        // rebuild tables (some earlier migrations flip the pragma on).
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        run_to(&mut conn, 100).unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();

        // Legacy shape (FKs are off, so no pages parent row is needed).
        conn.execute(
            "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
             VALUES (x'01', x'00000000', 'openai', 'text-embedding-3-small', 1, 0)",
            [],
        )
        .unwrap();

        run(&mut conn).unwrap();

        let (chunk_index, provider): (i64, String) = conn
            .query_row(
                "SELECT chunk_index, provider FROM page_embeddings",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(chunk_index, 0, "legacy row must become chunk 0");
        assert_eq!(provider, "openai");

        conn.execute(
            "INSERT INTO page_embeddings \
                 (page_id, chunk_index, vector, provider, model, dim, created_at) \
             VALUES (x'01', 1, x'00000000', 'openai', 'text-embedding-3-small', 1, 0)",
            [],
        )
        .expect("composite PK must accept a second chunk for the same page");
    }

    /// The rendered message must drop refinery's misleading phrasing and carry
    /// the operator-facing explanation and remedy.
    #[test]
    fn schema_ahead_message_is_actionable() {
        let rendered = StoreError::DataSchemaAhead {
            applied: "V99 (future_feature)".to_string(),
            supported: 28,
        }
        .to_string();

        assert!(
            !rendered.contains("missing from the filesystem"),
            "must not leak refinery's raw wording: {rendered}"
        );
        assert!(
            rendered.contains("newer than this engram build"),
            "{rendered}"
        );
        assert!(rendered.contains("V99 (future_feature)"), "{rendered}");
        assert!(rendered.contains("through V28"), "{rendered}");
    }
}
