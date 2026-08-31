//! The ledger generation counter (`aub-sth.9`).
//!
//! A projection has to be comparable against the database it claims to
//! describe, and every obvious substitute for that comparison is wrong: a
//! rowid says nothing about content, a timestamp compares two clocks, a file
//! mtime describes the filesystem, a WAL position describes storage
//! internals. A generation counter advanced inside the writing transaction is
//! a statement about content instead: if the projection says generation 412
//! and the database is at 415, three projection-relevant commits have
//! happened since it was published.
//!
//! [`advance`] is meant to be called by a caller that is already inside its
//! own write transaction over projection-relevant durable meter state, so the
//! generation bump commits atomically with the state change it accompanies.
//! This module owns only the counter itself; which writes must call it is a
//! contract enforced by each of those call sites, not by this one.
//!
//! SQLite serializes writers to one at a time even under WAL, so two
//! transactions both calling `advance` cannot observe or produce the same
//! post-increment value: the second writer blocks for the write lock (bounded
//! by the connection's busy timeout) until the first commits, then reads the
//! value the first one left behind.

use crate::error::Error;

/// A single ledger generation value.
///
/// Deliberately not `Ord`-derived beyond what a plain integer comparison
/// gives: the direction of the inequality is a statement about a specific
/// projection and a specific database, not a general ordering this type
/// should encourage comparing on its own.
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
/// (`0003_ledger_generation.rs`). Never called again: a later migration that
/// touched this table would violate "never resets, including across
/// migrations."
pub(crate) fn create_table(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TABLE ledger_generation (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            generation INTEGER NOT NULL
        ) STRICT;
        INSERT INTO ledger_generation (id, generation) VALUES (1, 0);",
    )
    .map_err(|e| Error::Store(format!("cannot create the ledger_generation table: {e}")))
}

/// Advances the counter by one and returns the new value.
///
/// Callers hold this within the same transaction as the projection-relevant
/// state change the bump accompanies, so a rollback of that change rolls the
/// generation back too and a commit advances both together.
pub fn advance(conn: &rusqlite::Connection) -> Result<Generation, Error> {
    conn.query_row(
        "UPDATE ledger_generation SET generation = generation + 1 WHERE id = 1 \
         RETURNING generation",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| Generation::new(value as u64))
    .map_err(|e| Error::Store(format!("cannot advance the ledger generation: {e}")))
}

/// Reads the current value without advancing it.
pub fn current(conn: &rusqlite::Connection) -> Result<Generation, Error> {
    conn.query_row(
        "SELECT generation FROM ledger_generation WHERE id = 1",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| Generation::new(value as u64))
    .map_err(|e| Error::Store(format!("cannot read the ledger generation: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::{Migration, run_migrations};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-ledger-generation-test-{}-{suffix}",
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

    fn fixture_conn(busy_millis: u64) -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(busy_millis),
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

    // --- unit: seeded at zero, advances by one --------------------------------------

    #[test]
    fn a_fresh_database_reads_generation_zero_and_advance_returns_one() {
        let (_scratch, conn) = fixture_conn(1000);
        assert_eq!(current(&conn).unwrap(), Generation::new(0));
        assert_eq!(advance(&conn).unwrap(), Generation::new(1));
        assert_eq!(current(&conn).unwrap(), Generation::new(1));
    }

    // --- integration: two concurrent writers cannot produce the same generation -----

    /// Two write connections both call `advance` at roughly the same time. SQLite's
    /// single-writer serialization means the second blocks for the write lock until
    /// the first commits, so the two calls must observe two distinct, consecutive
    /// values: {1, 2}, never a repeat.
    #[test]
    fn two_concurrent_writers_cannot_produce_the_same_generation() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(5000),
        };
        let mut bootstrap = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        run_migrations(
            &mut bootstrap,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        drop(bootstrap);

        let first = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        let second = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();

        let results: Vec<Generation> = std::thread::scope(|scope| {
            let a = scope.spawn(move || advance(&first).unwrap());
            let b = scope.spawn(move || advance(&second).unwrap());
            vec![a.join().unwrap(), b.join().unwrap()]
        });

        assert_ne!(
            results[0], results[1],
            "two concurrent writers must not produce the same generation: {results:?}"
        );
        let mut values: Vec<u64> = results.iter().map(|g| g.value()).collect();
        values.sort_unstable();
        assert_eq!(values, vec![1, 2]);
    }

    // --- unit: monotonic across a later migration ------------------------------------

    /// A populated fixture (generation already advanced past zero) migrates forward
    /// through a later, unrelated schema step. The generation the fixture already
    /// held survives: nothing about running a later migration resets it.
    #[test]
    fn the_counter_survives_a_later_migration_unchanged() {
        let (_scratch, mut conn) = fixture_conn(1000);
        advance(&conn).unwrap();
        advance(&conn).unwrap();
        advance(&conn).unwrap();
        assert_eq!(current(&conn).unwrap(), Generation::new(3));

        fn harmless_step(conn: &rusqlite::Connection) -> Result<(), Error> {
            conn.execute_batch(
                "CREATE TABLE ledger_generation_test_marker (id INTEGER PRIMARY KEY) STRICT",
            )
            .map_err(|e| Error::Store(format!("test migration failed: {e}")))
        }

        let mut extended = crate::store::migrations::registry();
        let next_version = extended.iter().map(|m| m.version).max().unwrap_or(0) + 1;
        extended.push(Migration {
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
            Generation::new(3),
            "a later migration that never touches ledger_generation must not reset it"
        );
    }

    // --- integration: no rowid, timestamp, mtime or WAL substitute ------------------

    /// A grep over this module's own source: the generation must never be derived
    /// from `last_insert_rowid`, a time function, or a WAL/data-version pragma. Any
    /// of those would silently reintroduce exactly the substitute this design
    /// rejects (see the module doc), so this guards the source directly rather than
    /// trusting a reviewer to notice.
    #[test]
    fn the_module_source_never_substitutes_rowid_timestamp_mtime_or_wal_for_the_generation() {
        // Scanning only the production half of the file (before `#[cfg(test)]`)
        // is deliberate: the banned list below necessarily quotes every one of
        // these patterns as a string literal, so scanning the whole file
        // (this test included) would always fail against itself.
        let source = include_str!("ledger_generation.rs");
        let production_source = source
            .split_once("#[cfg(test)]")
            .expect("this module must have a #[cfg(test)] boundary")
            .0;
        let banned = [
            "last_insert_rowid",
            "ROWID",
            "strftime",
            "unixepoch",
            "julianday",
            "CURRENT_TIMESTAMP",
            "data_version",
            "wal_checkpoint",
            ".metadata(",
            "SystemTime",
        ];
        for pattern in banned {
            assert!(
                !production_source.contains(pattern),
                "ledger_generation.rs must never derive the generation from {pattern}"
            );
        }
    }
}
