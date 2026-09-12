//! Schema step: one index on `meter_response_evidence(attempt_id)`, the
//! lookup the projection's per-account observation read and the spool drain's
//! evidence check both need to stay off a table scan (`aub-hgsv`).
//!
//! Two readers queried `meter_response_evidence` by `attempt_id` with no
//! index on that column, so every call paid a full scan of the table. The
//! projection's read looked up the newest evidence row of every successful
//! attempt while walking the account's history for the newest full-window
//! observation, and the spool drain's `evidence_exists_for_attempt` checked
//! every pending bundle the same way. Measured on the live ledger (360 MB,
//! 13k observations), one `aub sample --account` tick issued about 67,500
//! page reads over 4,663 distinct pages, 3,588 of them in
//! `meter_response_evidence`, which that tick scanned about fourteen times;
//! the same shape reached `journalctl` as a 494.5 MB memory peak, the scan
//! faulting the table's pages into the service's cgroup. A fixture ledger
//! never showed this because a fixture holds no history to walk.
//!
//! The index turns both lookups into index seeks. The projection's read is
//! bounded separately in the same bead: the store now answers the newest
//! full-window observation per account in one query instead of walking every
//! successful attempt the account ever had.
//!
//! Additive only: no row changes, no table is rebuilt, and the queries that
//! scanned `meter_response_evidence` before this migration search the index
//! instead.
//!
//! Recovery: the framework is forward-only, so there is no down step to run.
//! The manual reversal below drops the index again; it is exercised by this
//! module's round-trip test, never by production code. Dropping it costs
//! nothing but the scan this step came to remove.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 40;

const CREATE_METER_RESPONSE_EVIDENCE_ATTEMPT_INDEX: &str = "\
CREATE INDEX idx_meter_response_evidence_attempt ON meter_response_evidence (attempt_id);";

/// The manual reversal, for the round-trip test below. Production code never
/// runs it: the framework is forward-only.
#[cfg(test)]
const DROP_METER_RESPONSE_EVIDENCE_ATTEMPT_INDEX: &str =
    "DROP INDEX idx_meter_response_evidence_attempt;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_METER_RESPONSE_EVIDENCE_ATTEMPT_INDEX)
        .map_err(|e| {
            Error::Store(format!(
                "cannot create the meter_response_evidence attempt index: {e}"
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
                "aub-migration-0040-test-{}-{}",
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

    /// A database migrated through the full registry, so the index lands on
    /// the real `meter_response_evidence` table rather than on a hand-built
    /// stand-in whose shape could drift from the schema.
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

    fn attempt_index_exists(conn: &rusqlite::Connection) -> bool {
        let names: Vec<String> = conn
            .prepare("SELECT name FROM pragma_index_list('meter_response_evidence')")
            .expect("index list must prepare")
            .query_map([], |row| row.get(0))
            .expect("index list must query")
            .collect::<Result<Vec<_>, _>>()
            .expect("index names must read");
        names
            .iter()
            .any(|name| name == "idx_meter_response_evidence_attempt")
    }

    /// The bead's migration round-trip: the index arrives with the step, the
    /// manual reversal removes it, and re-applying the step brings it back.
    /// The reversal is manual SQL under test, never a framework path.
    #[test]
    fn the_attempt_index_arrives_and_is_removed_by_the_manual_reversal() {
        let (_scratch, conn) = open_migrated();
        assert!(
            attempt_index_exists(&conn),
            "the step creates the attempt index on meter_response_evidence"
        );

        conn.execute_batch(DROP_METER_RESPONSE_EVIDENCE_ATTEMPT_INDEX)
            .expect("the manual reversal must run");
        assert!(
            !attempt_index_exists(&conn),
            "the manual reversal removes the index"
        );

        apply(&conn).expect("the step must re-apply after the reversal");
        assert!(
            attempt_index_exists(&conn),
            "re-applying the step brings the index back"
        );
    }
}
