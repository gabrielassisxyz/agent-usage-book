//! Schema step: the session working directory (`aub-4ow0`).
//!
//! The session row carries the working directory its transcript reports, so
//! project and repository resolve from a stored fact rather than from a
//! re-read of the corpus. The column is nullable: transcripts ingested before
//! this step, and sources that state no directory, leave it absent, and an
//! absent directory keeps the session in the unknown buckets rather than in
//! an invented one.
//!
//! The directory is machine-local evidence: it is what the logical keys were
//! resolved from, it never leaves the ledger inside an export, and `aub
//! rebuild sessions` re-resolves the keys from it after an alias change.
//!
//! Recovery: the framework is forward-only, so there is no down step to run.
//! The manual reversal below drops the column again; it is exercised by this
//! module's own round-trip test, never by production code.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 38;

const ADD_WORKING_DIRECTORY: &str = "
ALTER TABLE session ADD COLUMN working_directory TEXT
    CHECK (working_directory IS NULL OR length(working_directory) > 0);";

/// The manual reversal, for the round-trip test below. Production code never
/// runs it: the framework is forward-only.
#[cfg(test)]
const DROP_WORKING_DIRECTORY: &str = "ALTER TABLE session DROP COLUMN working_directory;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_WORKING_DIRECTORY).map_err(|e| {
        Error::Store(format!(
            "cannot add the session working_directory column: {e}"
        ))
    })
}

/// This step, for the registry.
///
/// Additive only: one nullable column, so no irreplaceable data is at risk
/// and the verified-backup guard does not apply. Existing rows read back
/// with no directory, which is the unknown bucket, exactly as before.
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
    /// carries a stored directory, the reversal removes it, and re-applying
    /// the step brings it back. The reversal is manual SQL under test, never
    /// a framework path. The database opens through the one setup function,
    /// never around it.
    #[test]
    fn working_directory_round_trips_through_up_and_down() {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-migration-0038-test-{}-{suffix}.sqlite3",
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
                UNIQUE (source, native_session_id)
            ) STRICT;",
        )
        .expect("pre-step session table must create");

        apply(&conn).expect("up must apply");
        conn.execute(
            "INSERT INTO session (source, native_session_id, start, project_key, repository_key, \
             working_directory) VALUES ('claude-code', 's1', 1, 'unknown-project', \
             'unknown-repository', '/tmp/aub-fixture-project')",
            [],
        )
        .expect("a directory must store");
        let stored: Option<String> = conn
            .query_row(
                "SELECT working_directory FROM session WHERE native_session_id = 's1'",
                [],
                |row| row.get(0),
            )
            .expect("the column must read back");
        assert_eq!(stored.as_deref(), Some("/tmp/aub-fixture-project"));

        conn.execute_batch(DROP_WORKING_DIRECTORY)
            .expect("down must apply");
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('session')")
            .expect("pragma must prepare")
            .query_map([], |row| row.get(0))
            .expect("pragma must query")
            .map(|name| name.expect("column name must read"))
            .collect();
        assert!(
            !columns.contains(&"working_directory".to_string()),
            "down removes the column: {columns:?}"
        );

        apply(&conn).expect("up must re-apply after down");
        let stored: Option<String> = conn
            .query_row(
                "SELECT working_directory FROM session WHERE native_session_id = 's1'",
                [],
                |row| row.get(0),
            )
            .expect("the column must read back after re-apply");
        assert_eq!(
            stored, None,
            "down drops the stored directories with the column"
        );

        drop(conn);
        std::fs::remove_file(&path).expect("round-trip database must clean up");
    }
}
