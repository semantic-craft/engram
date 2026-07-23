//! End-to-end retention-lifecycle integration test.
//!
//! Demonstrates that M8's hybrid policy actually does what the docs
//! claim: episodic pages decay over time unless they get queried;
//! semantic concept pages compound forever; pinned anything survives;
//! and after sweep, the FTS5 index actually loses the evicted content
//! while keeping everything else searchable.
//!
//! Time travel is simulated by backdating the `updated_at` /
//! `access_count` / `last_accessed_at` columns via a secondary
//! `rusqlite::Connection`. WAL mode + a busy_timeout means we can
//! safely write while the writer actor is idle between operations.

use std::collections::HashSet;

use engram_consolidate::{run_lint, run_sweep};
use engram_core::{PageId, PagePath, Tier};
use engram_store::{DecayParams, Store};
use engram_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const US_PER_DAY: i64 = 86_400_000_000;

/// One page in the lifecycle fixture.
struct Fixture {
    /// Wiki-relative path.
    path: &'static str,
    /// Body — chosen to give FTS5 a distinct keyword per page.
    body: &'static str,
    /// Tier.
    tier: Tier,
    /// `true` if frontmatter should carry `pinned: true`.
    pinned: bool,
    /// Days since `updated_at`.
    age_days: i64,
    /// Total accesses simulated.
    access_count: u32,
    /// Days since `last_accessed_at`, or `None` to leave NULL.
    days_since_access: Option<i64>,
    /// Whether the sweep should evict this page.
    expected_evicted: bool,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        path: "sessions/fresh.md",
        body: "Started exploring rmcp tool routing patterns today",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 2,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    // Mid-term untouched: 60 days old, never queried. The point of
    // including this is to demonstrate the formula isn't too eager:
    // an unused episodic page should still survive in the 2-month
    // window when default params are in play. With lambda=0.02 the
    // time-term at 60d is exp(-1.2) = 0.30, still well above the
    // cold_threshold of 0.20.
    Fixture {
        path: "sessions/midterm-untouched.md",
        body: "Notes on dyn dispatch trade-offs and downcast overhead, midterm reference",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 60,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    Fixture {
        path: "sessions/hot-old.md",
        body: "Investigation of writer-actor backpressure design and tokio mpsc bounded channels",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 120,
        access_count: 50,
        days_since_access: Some(2),
        expected_evicted: false,
    },
    Fixture {
        path: "sessions/cold-old.md",
        body: "Quick spike on swapping jiff for an older datetime crate, abandoned mid-experiment",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 120,
        access_count: 0,
        days_since_access: None,
        expected_evicted: true,
    },
    Fixture {
        path: "sessions/very-cold.md",
        body: "Tried bolting cognee's pipeline shape onto the rust workspace, didn't pan out",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 200,
        access_count: 0,
        days_since_access: None,
        expected_evicted: true,
    },
    Fixture {
        path: "sessions/pinned-ancient.md",
        body: "Decision log: never re-add the iii-engine sidecar dependency",
        tier: Tier::Episodic,
        pinned: true,
        age_days: 300,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    Fixture {
        path: "concepts/karpathy-wiki.md",
        body: "Karpathy LLM Wiki principle: compile knowledge into the artifact, do not re-retrieve",
        tier: Tier::Semantic,
        pinned: false,
        age_days: 300,
        access_count: 5,
        days_since_access: Some(60),
        expected_evicted: false,
    },
    Fixture {
        path: "concepts/single-writer.md",
        body: "All SQLite mutations flow through one writer actor backed by mpsc",
        tier: Tier::Semantic,
        pinned: false,
        age_days: 200,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    Fixture {
        path: "concepts/wiki-conventions.md",
        body: "Wiki path conventions: sessions/, concepts/, decisions/, gotchas/",
        tier: Tier::Semantic,
        pinned: false,
        age_days: 30,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
];

/// Backdate one page's timestamps + access counters via a secondary
/// SQLite connection. WAL mode lets us write while the writer actor
/// is idle between operations.
fn backdate(db_path: &std::path::Path, now_us: i64, fixture: &Fixture, id: PageId) {
    let conn = rusqlite::Connection::open(db_path).expect("open aux conn");
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    let updated_at = now_us - fixture.age_days * US_PER_DAY;
    let last_access_us = fixture.days_since_access.map(|d| now_us - d * US_PER_DAY);
    conn.execute(
        "UPDATE pages \
         SET created_at = ?1, updated_at = ?1, access_count = ?2, last_accessed_at = ?3 \
         WHERE id = ?4",
        params![
            updated_at,
            i64::from(fixture.access_count),
            last_access_us,
            id.as_bytes(),
        ],
    )
    .expect("backdate update");
}

#[tokio::test]
async fn m8_retention_lifecycle_end_to_end() {
    // ── Phase 1 — bootstrap a fresh wiki + store ─────────────────
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "lifecycle-test", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");

    // ── Phase 2 — seed the 8 fixtures through the normal write path ─
    let mut ids: Vec<(&'static str, PageId)> = Vec::new();
    for fx in FIXTURES {
        let title = format!("Page {}", fx.path);
        let mut frontmatter = serde_json::Map::new();
        frontmatter.insert("title".into(), serde_json::Value::String(title.clone()));
        if fx.pinned {
            frontmatter.insert("pinned".into(), serde_json::Value::Bool(true));
        }
        let id = wiki
            .write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(fx.path.to_string()).expect("page path"),
                frontmatter: serde_json::Value::Object(frontmatter),
                body: fx.body.to_string(),
                tier: fx.tier,
                pinned: false,
                title: Some(title),
                admission_ctx: None,
                author_id: None,
                actor: engram_core::ActorContext::anonymous(),
            })
            .await
            .expect("write page");
        ids.push((fx.path, id));
    }

    // ── Phase 3 — time-travel via direct SQL ────────────────────
    let now_us = jiff::Timestamp::now().as_microsecond();
    let db_path = tmp.path().join("db/memory.sqlite");
    for (fx, (_, id)) in FIXTURES.iter().zip(&ids) {
        backdate(&db_path, now_us, fx, *id);
    }

    // ── Phase 4 — dry-run sweep; verify the formula's verdict ───
    let params = DecayParams::default();
    let dry = run_sweep(
        &store.reader,
        &store.writer,
        ws,
        proj,
        &params,
        /* dry_run */ true,
    )
    .await
    .expect("dry sweep");
    assert!(dry.dry_run);
    assert_eq!(
        dry.candidates_evaluated,
        FIXTURES.len(),
        "all {} pages should be considered as candidates",
        FIXTURES.len(),
    );

    let evicted: HashSet<&str> = dry.evicted.iter().map(|e| e.path.as_str()).collect();
    for fx in FIXTURES {
        let got = evicted.contains(fx.path);
        assert_eq!(
            got,
            fx.expected_evicted,
            "page {} expected_evicted={} but sweep said {} \
             (tier={:?}, pinned={}, age={}, access={}, days_since_access={:?})",
            fx.path,
            fx.expected_evicted,
            got,
            fx.tier,
            fx.pinned,
            fx.age_days,
            fx.access_count,
            fx.days_since_access,
        );
    }

    // ── Phase 5 — real sweep; verify row counts ─────────────────
    let counts_before = store.reader.status_counts().await.expect("counts before");
    assert_eq!(counts_before.pages_latest as usize, FIXTURES.len());
    assert_eq!(counts_before.pages_all as usize, FIXTURES.len());

    let real = run_sweep(
        &store.reader,
        &store.writer,
        ws,
        proj,
        &params,
        /* dry_run */ false,
    )
    .await
    .expect("real sweep");
    assert!(!real.dry_run);
    let expected_evicted_count = FIXTURES.iter().filter(|f| f.expected_evicted).count();
    assert_eq!(real.evicted.len(), expected_evicted_count);

    let counts_after = store.reader.status_counts().await.expect("counts after");
    assert_eq!(
        counts_after.pages_latest as usize,
        FIXTURES.len() - expected_evicted_count,
        "is_latest=1 should drop by exactly the evicted count",
    );
    assert_eq!(
        counts_after.pages_all as usize,
        FIXTURES.len(),
        "soft-delete preserves the row (kept for 180d hard-delete window)",
    );

    // ── Phase 6 — FTS5 invariants ───────────────────────────────
    // Keywords unique to evicted pages should disappear from search.
    let cognee_hits = store
        .reader
        .search_pages("cognee".into(), 5)
        .await
        .expect("search cognee");
    assert!(
        cognee_hits.is_empty(),
        "evicted page 'very-cold' mentioned cognee; should be unsearchable now, got: {cognee_hits:?}",
    );

    let jiff_hits = store
        .reader
        .search_pages("jiff".into(), 5)
        .await
        .expect("search jiff");
    assert!(
        jiff_hits.is_empty(),
        "evicted page 'cold-old' mentioned jiff; should be unsearchable",
    );

    // Hot + semantic + pinned pages should still be searchable.
    // Note on FTS5: our tokenizer is `unicode61 tokenchars '/_-'`, so
    // `writer-actor` and `single-writer` are *single* tokens. We pick
    // distinct standalone keywords to test each page independently.
    let backpressure_hits = store
        .reader
        .search_pages("backpressure".into(), 5)
        .await
        .expect("search backpressure");
    let bp_paths: HashSet<&str> = backpressure_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        bp_paths.contains("sessions/hot-old.md"),
        "hot reinforced page (with 'backpressure') should remain searchable: {bp_paths:?}",
    );

    let mutations_hits = store
        .reader
        .search_pages("mutations".into(), 5)
        .await
        .expect("search mutations");
    let mut_paths: HashSet<&str> = mutations_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        mut_paths.contains("concepts/single-writer.md"),
        "semantic concept page (with 'mutations') should remain searchable: {mut_paths:?}",
    );

    let karpathy_hits = store
        .reader
        .search_pages("karpathy".into(), 5)
        .await
        .expect("search karpathy");
    let karpathy_paths: HashSet<&str> = karpathy_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        karpathy_paths.contains("concepts/karpathy-wiki.md"),
        "300-day-old semantic page survives forever (no tier decay)",
    );

    // Note: our FTS5 tokenizer keeps `-` inside tokens, but the FTS5
    // *query parser* still treats a bare `-` as the NOT prefix. Search
    // for a standalone token from the body instead of `iii-engine`.
    let sidecar_hits = store
        .reader
        .search_pages("sidecar".into(), 5)
        .await
        .expect("search sidecar");
    let sidecar_paths: HashSet<&str> = sidecar_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        sidecar_paths.contains("sessions/pinned-ancient.md"),
        "pinned ancient page survives regardless of age",
    );

    // The mid-term untouched page deserves its own assertion to make
    // the "don't forget too fast" property explicit — searching for a
    // term unique to that page should still return it.
    let midterm_hits = store
        .reader
        .search_pages("dyn".into(), 5)
        .await
        .expect("search dyn");
    let midterm_paths: HashSet<&str> = midterm_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        midterm_paths.contains("sessions/midterm-untouched.md"),
        "60-day-old untouched episodic page should still be discoverable \
         (the formula isn't supposed to forget mid-term knowledge): {midterm_paths:?}",
    );

    // ── Phase 7 — lint catches the residual stale + duplicate signals ─
    let lint_report = run_lint(
        &store.reader,
        &wiki,
        None,
        ws,
        proj,
        /* dry_run */ true,
        /* use_llm */ true,
    )
    .await
    .expect("lint dry-run");
    // We added rule-based 'stale' detection for episodic pages >30d
    // with zero accesses. After sweep, the cold pages are no longer
    // is_latest=1 so they don't appear; but lint is a safety net for
    // anything that slipped through (e.g. user disabled sweep). Just
    // confirm the report shape is well-formed.
    for f in &lint_report.findings {
        assert!(
            !f.message.is_empty(),
            "every finding must have a human-readable message",
        );
        assert!(
            ["info", "warning", "error"].contains(&f.severity.as_str()),
            "severity must be one of info/warning/error, got {}",
            f.severity,
        );
    }

    // ── Phase 8 — retention scores ordering sanity check ────────
    // The dry-run report carries the actual scores. Verify they're
    // ordered the way the docs imply: hot pages > pinned-skipped
    // (which is absent) > cold pages (which got evicted).
    if let (Some(very_cold), Some(cold_old)) = (
        dry.evicted
            .iter()
            .find(|e| e.path == "sessions/very-cold.md"),
        dry.evicted
            .iter()
            .find(|e| e.path == "sessions/cold-old.md"),
    ) {
        assert!(
            very_cold.retention < cold_old.retention,
            "200d cold should score below 120d cold: very_cold={} cold_old={}",
            very_cold.retention,
            cold_old.retention,
        );
    }
}
