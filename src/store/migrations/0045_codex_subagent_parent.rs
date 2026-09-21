//! Schema step: the Codex subagent parent thread (`aub-wvrw`).
//!
//! A Codex subagent thread gets its own session id and rollout, but Codex
//! fires `SubagentStart` for it rather than `SessionStart`, so the marker
//! hook never sees the child id and no marker is written for it. The
//! rollout's first line already records the parent
//! (`source.subagent.thread_spawn.parent_thread_id`), so attribution inherits
//! the parent session's account-marker timeline instead of guessing.
//!
//! The column is nullable: transcripts ingested before this step, top-level
//! sessions, and sources that never name a parent leave it absent, and an
//! absent parent keeps the session's own marker timeline (or the unknown
//! bucket) exactly as before. The parent lives in the same source namespace
//! as the child, so only the native id is stored.
//!
//! Recovery: the framework is forward-only, so there is no down step to run.
//! The manual reversal below drops the column again; it is exercised by this
//! module's own round-trip test, never by production code.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 45;

const ADD_SUBAGENT_PARENT: &str = "
ALTER TABLE session ADD COLUMN parent_native_session_id TEXT
    CHECK (parent_native_session_id IS NULL OR length(parent_native_session_id) > 0);";

/// The manual reversal, for the round-trip test below. Production code never
/// runs it: the framework is forward-only.
#[cfg(test)]
const DROP_SUBAGENT_PARENT: &str = "ALTER TABLE session DROP COLUMN parent_native_session_id;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_SUBAGENT_PARENT).map_err(|e| {
        Error::Store(format!(
            "cannot add the session parent_native_session_id column: {e}"
        ))
    })
}

/// This step, for the registry.
///
/// Additive only: one nullable column, so no irreplaceable data is at risk
/// and the verified-backup guard does not apply. Existing rows read back
/// with no parent, which keeps their own attribution exactly as before.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: false,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::domain::time::MonotonicDuration;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// The bead's migration round-trip: the column arrives with the step,
    /// carries a stored parent link, the reversal removes it, and re-applying
    /// the step brings it back. The reversal is manual SQL under test, never
    /// a framework path. The database opens through the one setup function,
    /// never around it.
    #[test]
    fn subagent_parent_round_trips_through_up_and_down() {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-migration-0045-test-{}-{suffix}.sqlite3",
            std::process::id()
        ));
        let conn = open(
            &path,
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1000),
            },
        )
        .expect("round-trip database must open");
        conn.execute_batch(
            "CREATE TABLE session (
                id INTEGER PRIMARY KEY,
                source TEXT NOT NULL,
                native_session_id TEXT NOT NULL,
                start INTEGER NOT NULL,
                end INTEGER,
                project_key TEXT NOT NULL,
                repository_key TEXT NOT NULL,
                run_id TEXT,
                working_directory TEXT,
                UNIQUE (source, native_session_id)
            ) STRICT;",
        )
        .expect("pre-step session table must create");

        apply(&conn).expect("up must apply");
        conn.execute(
            "INSERT INTO session (source, native_session_id, start, project_key, repository_key, \
             parent_native_session_id) VALUES ('codex', 'child-1', 1, 'unknown-project', \
             'unknown-repository', 'parent-1')",
            [],
        )
        .expect("a parent link must store");
        let stored: Option<String> = conn
            .query_row(
                "SELECT parent_native_session_id FROM session WHERE native_session_id = 'child-1'",
                [],
                |row| row.get(0),
            )
            .expect("the column must read back");
        assert_eq!(stored.as_deref(), Some("parent-1"));

        conn.execute_batch(DROP_SUBAGENT_PARENT)
            .expect("down must apply");
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('session')")
            .expect("pragma must prepare")
            .query_map([], |row| row.get(0))
            .expect("pragma must query")
            .map(|name| name.expect("column name must read"))
            .collect();
        assert!(
            !columns.contains(&"parent_native_session_id".to_string()),
            "down removes the column: {columns:?}"
        );

        apply(&conn).expect("up must re-apply after down");
        let stored: Option<String> = conn
            .query_row(
                "SELECT parent_native_session_id FROM session WHERE native_session_id = 'child-1'",
                [],
                |row| row.get(0),
            )
            .expect("the column must read back after re-apply");
        assert_eq!(
            stored, None,
            "down drops the stored parent links with the column"
        );

        drop(conn);
        std::fs::remove_file(&path).expect("round-trip database must clean up");
    }
}
