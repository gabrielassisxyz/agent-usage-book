//! Schema step: one index on `meter_attempt(account_id, id)`, the ordering the
//! projection's per-account reads need so that their `LIMIT 1` stops at the
//! first row instead of sorting the account's whole history (`aub-hgsv`).
//!
//! Three reads take the newest attempt of one account: the projection's latest
//! attempt, its newest successful observation, and the authentication-backoff
//! streak. All three constrain `account_id` by equality and order by
//! `meter_attempt.id` descending, and the only index on that column pair was
//! `idx_meter_attempt_open (account_id, request_started_at)`, whose key order
//! says nothing about `id`. SQLite therefore answered each of them with
//! `USE TEMP B-TREE FOR ORDER BY`: every successful attempt of the account was
//! joined, materialised and sorted before one row came back, so the `LIMIT 1`
//! bounded the result and not the work. Measured on a copy of the live ledger
//! (370 MB, 13k observations), one `aub sample --account` tick read
//! `meter_attempt` 39,772 times, which did not move when the evidence lookup
//! beside it was indexed in migration 0040.
//!
//! With `(account_id, id)` the same reads become a reverse seek on the index
//! and stop at the first row that satisfies the joins, which is what makes the
//! per-account read bounded rather than merely un-scanned.
//!
//! `idx_meter_attempt_open` stays: it serves the request-started-at ordering
//! that the run and window reads use, which this index does not answer.
//!
//! Additive only: no row changes, no table is rebuilt.
//!
//! Recovery: the framework is forward-only, so there is no down step to run.
//! The manual reversal below drops the index again; it is exercised by this
//! module's round-trip test, never by production code. Dropping it restores
//! the sort this step came to remove and nothing else.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 41;

const CREATE_METER_ATTEMPT_ACCOUNT_NEWEST_INDEX: &str = "\
CREATE INDEX idx_meter_attempt_account_newest ON meter_attempt (account_id, id);";

/// The manual reversal, for the round-trip test below. Production code never
/// runs it: the framework is forward-only.
#[cfg(test)]
const DROP_METER_ATTEMPT_ACCOUNT_NEWEST_INDEX: &str =
    "DROP INDEX idx_meter_attempt_account_newest;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_METER_ATTEMPT_ACCOUNT_NEWEST_INDEX)
        .map_err(|e| {
            Error::Store(format!(
                "cannot create the meter_attempt account newest index: {e}"
            ))
        })
}

/// This step, for the registry.
///
/// Additive only: one index on an existing table, so no irreplaceable data is
/// at risk and the verified-backup guard does not apply.
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

    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "aub-migration-0041-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A database migrated through the full registry, so the index lands on
    /// the real `meter_attempt` table rather than on a hand-built stand-in
    /// whose shape could drift from the schema.
    fn open_migrated() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let mut conn = open(
            &scratch.path().join("meter.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1000),
            },
        )
        .expect("round-trip database must open");
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .expect("migrations must run");
        (scratch, conn)
    }

    fn account_newest_index_exists(conn: &rusqlite::Connection) -> bool {
        let names: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_list('meter_attempt')")
            .expect("index list must prepare")
            .query_map([], |row| row.get(0))
            .expect("index list must query")
            .collect::<Result<Vec<_>, _>>()
            .expect("index names must read");
        names
            .iter()
            .any(|name| name == "idx_meter_attempt_account_newest")
    }

    /// The bead's migration round-trip: the index arrives with the step, the
    /// manual reversal removes it, and re-applying the step brings it back.
    /// The reversal is manual SQL under test, never a framework path.
    #[test]
    fn the_account_newest_index_arrives_and_is_removed_by_the_manual_reversal() {
        let (_scratch, conn) = open_migrated();
        assert!(
            account_newest_index_exists(&conn),
            "the step creates the account newest index on meter_attempt"
        );

        conn.execute_batch(DROP_METER_ATTEMPT_ACCOUNT_NEWEST_INDEX)
            .expect("the manual reversal must run");
        assert!(
            !account_newest_index_exists(&conn),
            "the manual reversal removes the index"
        );

        apply(&conn).expect("the step must re-apply after the reversal");
        assert!(
            account_newest_index_exists(&conn),
            "re-applying the step brings the index back"
        );
    }

    /// The ordering the step exists for: with the index present the newest
    /// attempt of one account is a reverse seek, and with it dropped SQLite
    /// falls back to sorting the account's whole history. The plan line that
    /// decides this is `USE TEMP B-TREE FOR ORDER BY`, because that is the
    /// line that says `LIMIT 1` bounded the result and not the work.
    #[test]
    fn the_newest_attempt_of_an_account_orders_through_the_index_and_sorts_without_it() {
        let (_scratch, conn) = open_migrated();
        let newest_attempt_sql = "SELECT id FROM meter_attempt \
             WHERE account_id = ?1 ORDER BY id DESC LIMIT 1";

        assert!(
            !sorts_in_a_temp_btree(&conn, newest_attempt_sql),
            "the index serves the ordering, so no sort is planned"
        );

        conn.execute_batch(DROP_METER_ATTEMPT_ACCOUNT_NEWEST_INDEX)
            .expect("the manual reversal must run");
        assert!(
            sorts_in_a_temp_btree(&conn, newest_attempt_sql),
            "without the index the planner sorts the account's history, \
             which is the cost this step removes"
        );
    }

    fn sorts_in_a_temp_btree(conn: &rusqlite::Connection, sql: &str) -> bool {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("the plan must prepare")
            .query_map(rusqlite::params![1i64], |row| row.get::<_, String>(3))
            .expect("the plan must query")
            .collect::<Result<Vec<String>, _>>()
            .expect("the plan must read")
            .iter()
            .any(|line| line.contains("USE TEMP B-TREE FOR ORDER BY"))
    }
}
