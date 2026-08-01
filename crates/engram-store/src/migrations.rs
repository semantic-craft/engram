//! refinery-driven schema migrations.

use std::collections::HashSet;

use crate::error::{StoreError, StoreResult};
use refinery::Migration;
use rusqlite::params;

refinery::embed_migrations!("migrations");

/// Run all pending migrations against an open connection.
///
/// Before handing the connection to refinery, two preparatory passes run:
///
/// 1. [`ensure_store_not_ahead`] rejects a store whose history contains a
///    version this binary does not embed (data written by a newer build),
///    with the actionable [`StoreError::DataSchemaAhead`] error.
/// 2. [`backfill_version_gaps`] applies embedded migrations that sit *below*
///    the store's current high-water mark but were never applied. V28–V30
///    shipped in 2.1.0 numbered below the already-released V100/V101, so a
///    2.0.0-era store has `current = 101` with V28–V30 unapplied — a state
///    refinery's `abort_missing` verifier refuses to open. The backfill
///    executes each gapped migration and records it in
///    `refinery_schema_history` exactly as refinery would have, after which
///    refinery's own verification passes untouched.
///
/// # Errors
/// Propagates the underlying refinery error if a migration fails.
pub fn run(conn: &mut rusqlite::Connection) -> StoreResult<()> {
    ensure_store_not_ahead(conn)?;
    backfill_version_gaps(conn)?;
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

/// True once refinery has created its history table (i.e. the store has been
/// opened by some engram build before). A fresh database has nothing to guard
/// or backfill.
fn history_table_exists(conn: &rusqlite::Connection) -> StoreResult<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'refinery_schema_history')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

/// Reject a store written by a newer engram build: any applied history row
/// whose version is absent from the compiled-in set means this binary does
/// not understand the schema, so the store is left untouched. Runs before
/// [`backfill_version_gaps`] so no DDL is executed against a store we are
/// about to refuse.
fn ensure_store_not_ahead(conn: &rusqlite::Connection) -> StoreResult<()> {
    if !history_table_exists(conn)? {
        return Ok(());
    }
    let embedded: HashSet<u32> = migrations::runner()
        .get_migrations()
        .iter()
        .map(Migration::version)
        .collect();
    let mut stmt = conn.prepare("SELECT version, name FROM refinery_schema_history")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (version, name) = row?;
        if !embedded.contains(&version) {
            return Err(StoreError::DataSchemaAhead {
                applied: format!("V{version} ({name})"),
                supported: max_supported_version(),
            });
        }
    }
    Ok(())
}

/// Apply embedded migrations that were released *out of order*: numbered
/// below the store's current high-water mark yet never applied (the V28–V30
/// vs V100/V101 incident — see [`run`]). Each gapped migration executes in
/// its own transaction and is recorded in `refinery_schema_history` with the
/// same row shape refinery writes (RFC3339 `applied_on`, `checksum` as the
/// embedded migration's own u64), so refinery's divergence check over the
/// row passes on every later startup.
fn backfill_version_gaps(conn: &mut rusqlite::Connection) -> StoreResult<()> {
    if !history_table_exists(conn)? {
        return Ok(());
    }
    let applied: HashSet<u32> = {
        let mut stmt = conn.prepare("SELECT version FROM refinery_schema_history")?;
        let rows = stmt.query_map([], |row| row.get::<_, u32>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let Some(&high_water) = applied.iter().max() else {
        return Ok(());
    };
    let mut gapped: Vec<Migration> = migrations::runner()
        .get_migrations()
        .iter()
        .filter(|m| m.version() < high_water && !applied.contains(&m.version()))
        .cloned()
        .collect();
    gapped.sort();
    for migration in gapped {
        // Embedded migrations always carry their SQL; `sql()` is `None` only
        // for rows read back from a database. Fail closed rather than record
        // a migration we could not actually execute.
        let Some(sql) = migration.sql() else {
            return Err(StoreError::Io(std::io::Error::other(format!(
                "embedded migration V{} ({}) has no SQL to backfill",
                migration.version(),
                migration.name()
            ))));
        };
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO refinery_schema_history (version, name, applied_on, checksum) \
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?3)",
            params![
                migration.version(),
                migration.name(),
                migration.checksum().to_string()
            ],
        )?;
        tx.commit()?;
        tracing::info!(
            version = migration.version(),
            name = migration.name(),
            "backfilled migration released below the schema high-water mark"
        );
    }
    Ok(())
}

/// Translate refinery's raw error into a store-domain error. The only variant
/// reshaped is `MissingVersion`. refinery raises it in both directions —
/// store ahead of binary, and shipped-but-unapplied below the high-water
/// mark — but [`ensure_store_not_ahead`] catches the first before refinery
/// runs and [`backfill_version_gaps`] eliminates the second, so by the time
/// refinery executes this can only be a backstop for the ahead case.
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

    /// Regression for the 2.0.0 → 2.1.0 upgrade failure: V28–V30 shipped in
    /// 2.1.0 numbered *below* the already-released V100/V101, so a store
    /// written by 2.0.0 sits at high-water 101 with V28–V30 unapplied and
    /// refinery refuses to open it. Reproduce that exact store by running the
    /// embedded set minus V28–V30, then assert `run` backfills the gap,
    /// creates the missing schema objects, and stays clean on re-run (which
    /// exercises refinery's checksum verification over the backfilled rows).
    #[test]
    fn v2_0_0_store_with_gap_below_high_water_is_backfilled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut conn = Connection::open(tmp.path().join("memory.sqlite")).unwrap();
        // Mirror Store::open: FK enforcement stays off during migrations.
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();

        // The 2.0.0 release set: everything embedded today except V28–V30.
        let released_2_0_0: Vec<Migration> = migrations::runner()
            .get_migrations()
            .iter()
            .filter(|m| !(28..=30).contains(&m.version()))
            .cloned()
            .collect();
        refinery::Runner::new(&released_2_0_0)
            .run(&mut conn)
            .unwrap();
        let gap_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refinery_schema_history WHERE version IN (28, 29, 30)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gap_rows, 0, "precondition: V28–V30 unapplied");

        run(&mut conn).expect("upgrade from a 2.0.0 store must succeed");

        let gap_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refinery_schema_history WHERE version IN (28, 29, 30)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gap_rows, 3, "V28–V30 must be recorded as applied");

        // The objects those migrations create must actually exist.
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('auto_improve_proposals') \
                 WHERE name IN ('target_kind', 'provenance_json', 'repository_identity_sha256')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cols, 3, "V28/V30 columns must exist after backfill");
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN \
                 ('project_instruction_proposal_revisions', 'project_instruction_applications')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2, "V29/V30 tables must exist after backfill");

        // Second run: refinery now verifies the backfilled history rows
        // (version + name + checksum). A mismatch would abort as divergent.
        run(&mut conn).expect("store must stay openable after backfill");
    }

    /// Tripwire for the incident class itself: every embedded migration
    /// numbered below the released high-water mark must be part of a shipped
    /// release. A new migration slotted into a historical gap (the V28
    /// mistake) fails here at CI time instead of failing at startup on every
    /// already-migrated store. Append to `RELEASED` when cutting a release.
    #[test]
    fn no_new_migrations_below_released_high_water_mark() {
        const RELEASED: &[u32] = &[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 27, 28, 29, 30, 100, 101,
        ];
        let released_max = *RELEASED.iter().max().unwrap();
        let embedded: Vec<(u32, String)> = migrations::runner()
            .get_migrations()
            .iter()
            .map(|m| (m.version(), m.name().to_string()))
            .collect();
        for (version, name) in &embedded {
            if *version < released_max {
                assert!(
                    RELEASED.contains(version),
                    "migration V{version} ({name}) is numbered below the released high-water \
                     mark V{released_max} but is not part of any release. Stores migrated by \
                     an earlier release can never apply it in order — number it above the \
                     highest embedded migration instead."
                );
            }
        }
        for version in RELEASED {
            assert!(
                embedded.iter().any(|(v, _)| v == version),
                "released migration V{version} is missing from the embedded set — shipped \
                 migrations must never be deleted or renumbered."
            );
        }
    }

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
