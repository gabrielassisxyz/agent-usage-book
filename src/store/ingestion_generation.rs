//! The ingestion generation counter (`aub-xus.7`).
//!
//! The export header carries the ledger generation and the ingestion
//! generation it was produced from, so a consumer can tell which data state an
//! export describes. The ledger generation is advanced by every
//! projection-relevant write; the ingestion generation is the transcript side
//! of the same fact: how many completed ingestion passes the exported usage
//! reflects. It mirrors the ledger generation counter: one row, seeded at
//! zero, never reset. The ingest path advances it when it lands (`aub-lqe.11`);
//! until then the counter reads zero, which is the truth: no ingestion pass
//! has ever completed.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use crate::error::Error;

/// A single ingestion generation value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Creates the single-row counter this migration owns, seeded at zero.
///
/// Called once, from the migration that introduces this table
/// (`0018_ingestion_generation.rs`). Never called again: a later migration that
/// touched this table would violate "never resets, including across
/// migrations."
pub(crate) fn create_table(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TABLE ingestion_generation (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            generation INTEGER NOT NULL
        ) STRICT;
        INSERT INTO ingestion_generation (id, generation) VALUES (1, 0);",
    )
    .map_err(|e| Error::Store(format!("cannot create the ingestion_generation table: {e}")))
}

/// Reads the current value without advancing it.
pub fn current(conn: &rusqlite::Connection) -> Result<Generation, Error> {
    conn.query_row(
        "SELECT generation FROM ingestion_generation WHERE id = 1",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| Generation::new(value as u64))
    .map_err(|e| Error::Store(format!("cannot read the ingestion generation: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-ingestion-generation-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_conn() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("ingestion.db");
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    /// A fresh database reads generation zero: no ingestion pass has ever
    /// completed, and the export header must say exactly that.
    #[test]
    fn a_fresh_database_reads_generation_zero() {
        let (_scratch, conn) = fixture_conn();
        assert_eq!(current(&conn).unwrap(), Generation::new(0));
    }

    /// The counter survives a later migration unchanged, matching the ledger
    /// generation counter's contract: nothing about running a migration resets
    /// it.
    #[test]
    fn the_counter_survives_a_later_migration_unchanged() {
        let (_scratch, mut conn) = fixture_conn();
        assert_eq!(current(&conn).unwrap(), Generation::new(0));

        fn harmless_step(conn: &rusqlite::Connection) -> Result<(), Error> {
            conn.execute_batch(
                "CREATE TABLE ingestion_generation_test_marker (id INTEGER PRIMARY KEY) STRICT",
            )
            .map_err(|e| Error::Store(format!("test migration failed: {e}")))
        }

        let mut extended = crate::store::migrations::registry();
        let next_version = extended.iter().map(|m| m.version).max().unwrap_or(0) + 1;
        extended.push(crate::store::migrate::Migration {
            version: next_version,
            rewrites_irreplaceable: false,
            apply: harmless_step,
        });

        run_migrations(
            &mut conn,
            &extended,
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();

        assert_eq!(
            current(&conn).unwrap(),
            Generation::new(0),
            "a later migration that never touches ingestion_generation must not reset it"
        );
    }
}
