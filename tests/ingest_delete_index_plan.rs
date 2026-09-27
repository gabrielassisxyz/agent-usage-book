//! The two ingest deletes stay cheap because of the `0023` indexes (`aub-p7o9`).
//!
//! Both deletes in `persist_ingest_batch` run before the wall-clock batch
//! bound can cut in, so they stay short only because the planner searches
//! `usage_occurrence` through `idx_usage_occurrence_source_file` (the
//! whole-file delete) and `idx_usage_occurrence_event_id` (the correlated
//! subquery of the orphaned-event delete). Every other test uses a small
//! ledger, so nothing else would notice if a plan stopped using them. A
//! wall-clock assertion on a large ledger would be flaky on a shared machine,
//! so this pins the property with SQLite's `EXPLAIN QUERY PLAN`, which is
//! deterministic.
//!
//! Both statements are read from the production constants in
//! `src/store/ingest.rs`, never copied here, so an edit to either statement
//! is what this test checks.

use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::time::MonotonicDuration;
use agent_usage_book::store::connection::PragmaPolicy;
use agent_usage_book::store::ingest::{
    INGEST_ORPHANED_EVENT_DELETE_SQL, INGEST_WHOLE_FILE_OCCURRENCE_DELETE_SQL,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn migrated_ledger() -> (std::path::PathBuf, rusqlite::Connection) {
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "aub-p7o9-index-plan-{}-{suffix}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");
    let db_path = dir.join("ledger.db");
    let conn = test_support::open_migrated(
        &db_path,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    );
    (dir, conn)
}

fn plan_of(conn: &rusqlite::Connection, sql: &str, param: Option<&str>) -> String {
    let explained = format!("EXPLAIN QUERY PLAN {sql}");
    let mut stmt = conn.prepare(&explained).expect("the plan must prepare");
    let rows: Vec<String> = match param {
        Some(value) => stmt
            .query_map(rusqlite::params![value], |row| row.get::<_, String>(3))
            .expect("the plan must query")
            .collect::<Result<Vec<String>, _>>()
            .expect("the plan must read"),
        None => stmt
            .query_map([], |row| row.get::<_, String>(3))
            .expect("the plan must query")
            .collect::<Result<Vec<String>, _>>()
            .expect("the plan must read"),
    };
    rows.join("\n")
}

/// Both ingest deletes reach `usage_occurrence` through the `0023` indexes:
/// the whole-file delete searches by `source_file`, and the orphaned-event
/// delete's correlated subquery searches by `event_id`. The outer `SCAN` of
/// `usage_event` in the orphan plan is expected: that delete necessarily
/// visits every event row, and what the index buys is that the per-event
/// subquery seeks into `usage_occurrence` instead of scanning it per event.
#[test]
fn ingest_deletes_use_the_0023_occurrence_indexes() {
    let (_dir, conn) = migrated_ledger();

    let whole_file_plan = plan_of(
        &conn,
        INGEST_WHOLE_FILE_OCCURRENCE_DELETE_SQL,
        Some("probe-source-file"),
    );
    assert!(
        whole_file_plan.contains("idx_usage_occurrence_source_file"),
        "the whole-file delete must use idx_usage_occurrence_source_file, plan was:\n{whole_file_plan}"
    );
    assert!(
        !whole_file_plan.contains("SCAN"),
        "the whole-file delete must not scan usage_occurrence, plan was:\n{whole_file_plan}"
    );

    let orphan_plan = plan_of(&conn, INGEST_ORPHANED_EVENT_DELETE_SQL, None);
    assert!(
        orphan_plan.contains("idx_usage_occurrence_event_id"),
        "the orphaned-event delete's subquery must use idx_usage_occurrence_event_id, plan was:\n{orphan_plan}"
    );
    assert!(
        !orphan_plan.contains("SCAN usage_occurrence") && !orphan_plan.contains("SCAN o"),
        "the orphaned-event delete must not scan usage_occurrence in its subquery, plan was:\n{orphan_plan}"
    );
    assert!(
        orphan_plan.contains("SCAN usage_event"),
        "the orphaned-event delete still visits every event row (outer SCAN of usage_event is expected), plan was:\n{orphan_plan}"
    );
}
