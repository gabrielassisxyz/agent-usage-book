//! Forward-only versioned schema migrations and the migration lock.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! Every migration is a numbered, forward-only step applied inside its own
//! exclusive transaction (PLAN.md section 11.4). The exclusive transaction is
//! the explicit migration lock: a concurrent migrator waits (bounded by the
//! connection's busy timeout) and then fails with the store class rather than
//! proceeding, and the version re-read inside the lock makes a double
//! application impossible no matter how two processes interleave.
//!
//! A migration that rewrites an irreplaceable table refuses to start unless a
//! verified backup exists from the current schema version: back up, verify,
//! migrate, integrity-check, and only then publish (PLAN.md section 11.4). The
//! backup verifier is an injected trait because the backup machinery itself is
//! a later bead (`aub-sth.12`); this module states the contract the verifier
//! must answer.
//!
//! Every applied migration records the `aub` version and source revision that
//! applied it (PLAN.md section 5: "Persist the aub version and source revision
//! used for every ... schema migration"), so "why does this table look like
//! that" is answerable from the database alone. A database at a newer schema
//! version than this binary is refused with a clear message rather than
//! touched: opening a newer database with an older binary is exactly how a
//! schema gets migrated backwards.
//!
//! The status path never runs a migration: it never opens SQLite at all
//! (PLAN.md section 16.2), and `bin/checks/boundary-rules/15-status-no-migration`
//! keeps the migration module out of the projection reader.

use crate::domain::time::Clock;
use crate::error::Error;

/// The schema version of a database no migration has ever applied to.
///
/// Version zero is a sentinel, never a migration: the first migration is
/// version 1 and each later one is exactly one higher, which is the ordering
/// rule [`run_migrations`] enforces on the registry.
pub const INITIAL_SCHEMA_VERSION: u32 = 0;

/// The framework's own bookkeeping table: one row per applied migration.
///
/// `version` is the primary key, so the current schema version is
/// `MAX(version)` and a version can never be applied twice. The audit columns
/// record who applied it: the `aub` version and source revision of the binary
/// that ran the migration, plus the wall-clock time it was applied.
const BOOTSTRAP_SQL: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    applied_at INTEGER NOT NULL,
    aub_version TEXT NOT NULL,
    source_revision TEXT NOT NULL
) STRICT";

/// One numbered, forward-only schema step.
///
/// `apply` runs inside the migration's exclusive transaction and must not
/// commit or roll back on its own: the framework commits the step together
/// with its version record, so a failure rolls back both and the recorded
/// version stays at the prior value.
///
/// Deliberately no `PartialEq`: the struct holds a function pointer, and
/// comparing function pointers is meaningless (their addresses are not
/// guaranteed unique). Equality between migrations is never needed; the
/// registry ordering rule compares versions.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// The schema version this migration produces. Consecutive from 1.
    pub version: u32,
    /// Whether this migration rewrites an irreplaceable table (as opposed to
    /// appending or adding columns, which are preferred; PLAN.md section 11.4).
    /// A rewrite refuses to start without a verified backup at the current
    /// schema version.
    pub rewrites_irreplaceable: bool,
    /// The step itself, run inside the migration's transaction.
    pub apply: fn(&rusqlite::Connection) -> Result<(), Error>,
}

/// Answers whether a verified backup exists from a given schema version.
///
/// A backup "exists" only once it has been verified (PLAN.md section 38: an
/// unverified archive is not yet a backup). The schema version asked about is
/// the version in force before the rewrite, so a backup taken from a different
/// schema generation never satisfies the guard.
pub trait VerifiedBackup {
    /// Whether a verified backup exists from `schema_version`.
    fn verified_backup_exists(&self, schema_version: u32) -> bool;
}

/// What one migration run did: the versions it applied and the resulting
/// schema version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationSummary {
    /// The versions applied by this run, in version order.
    pub applied: Vec<u32>,
    /// The schema version the database is at when the run finishes.
    pub version: u32,
}

/// Maps a rusqlite failure to the store class, naming the migration lock when
/// the failure is a busy database. Every database touch in this module goes
/// through this mapper so a contender for the lock fails with a message that
/// says what it was waiting for.
fn store_error(context: &str, err: rusqlite::Error) -> Error {
    let busy = matches!(
        &err,
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseBusy
    );
    if busy {
        Error::Store(format!("migration lock: database busy: {context}: {err}"))
    } else {
        Error::Store(format!("{context}: {err}"))
    }
}

/// The current schema version of the database, bootstrapping the bookkeeping
/// table if it does not exist yet.
///
/// A fresh database (no `schema_migration` table, or an empty one) reads as
/// [`INITIAL_SCHEMA_VERSION`]. The bootstrap is idempotent, so calling this on
/// any database is safe.
pub fn current_schema_version(conn: &rusqlite::Connection) -> Result<u32, Error> {
    conn.execute_batch(BOOTSTRAP_SQL)
        .map_err(|e| store_error("cannot bootstrap the schema version table", e))?;
    recorded_schema_version(conn)
}

/// Reads the recorded version, assuming the bookkeeping table exists.
///
/// Backup verification uses this read-only form against the archived database.
/// It must not bootstrap a missing table because verification never repairs the
/// artifact it is judging.
pub fn recorded_schema_version(conn: &rusqlite::Connection) -> Result<u32, Error> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migration",
        [],
        |row| row.get(0),
    )
    .map_err(|e| store_error("cannot read the schema version", e))
}

/// Refuses a registry that violates the ordering rule before any migration
/// runs.
///
/// The rule: versions are consecutive from 1, each entry exactly one higher
/// than the previous. Version 0 is the fresh-database sentinel and can never
/// be a migration; a gap is refused because a missing version could be
/// appended later and would then never apply to a database already past it;
/// a duplicate or reordering is refused because forward-only means a version,
/// once applied, is never edited.
fn validate_registry(migrations: &[Migration]) -> Result<(), Error> {
    let mut previous = INITIAL_SCHEMA_VERSION;
    for migration in migrations {
        if migration.version == 0 {
            return Err(Error::Store(
                "migration registry: version 0 is the fresh-database sentinel and can never be a migration"
                    .into(),
            ));
        }
        if migration.version != previous + 1 {
            return Err(Error::Store(format!(
                "migration registry: versions must be consecutive and forward-only, expected {} after {previous}, found {}",
                previous + 1,
                migration.version
            )));
        }
        previous = migration.version;
    }
    Ok(())
}

/// Applies every pending migration forward, under the migration lock.
///
/// Each pending migration runs in its own `BEGIN EXCLUSIVE` transaction, which
/// is the explicit migration lock: a concurrent migrator waits up to the
/// connection's configured busy timeout and then fails with the store class
/// rather than proceeding. The current version is re-read inside the lock, so
/// a migration another process applied while this one waited is skipped rather
/// than applied twice. A rewrite migration additionally refuses to start
/// unless `backup` reports a verified backup at the current schema version.
///
/// The step and its version record commit atomically: a failed migration rolls
/// back entirely and the recorded version stays at the prior value, which is
/// the recovery-path contract (the database is never left partially migrated).
///
/// A database at a newer schema version than the highest version this binary
/// knows is refused with a message naming both versions, before anything is
/// touched.
pub fn run_migrations(
    conn: &mut rusqlite::Connection,
    migrations: &[Migration],
    backup: Option<&dyn VerifiedBackup>,
    clock: &dyn Clock,
) -> Result<MigrationSummary, Error> {
    validate_registry(migrations)?;

    let mut current = current_schema_version(conn)?;

    let binary_max = migrations
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(INITIAL_SCHEMA_VERSION);
    if current > binary_max {
        return Err(Error::Store(format!(
            "database schema version {current} is newer than this binary's {binary_max}; refusing to proceed"
        )));
    }

    let mut applied = Vec::new();
    for migration in migrations {
        if migration.version <= current {
            continue;
        }

        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
            .map_err(|e| store_error("cannot take the migration lock", e))?;

        let current_in_lock = recorded_schema_version(&tx)?;
        if current_in_lock >= migration.version {
            // Another process applied this version (or a later one) while this
            // process waited for the lock. Nothing to do; the transaction is
            // dropped uncommitted.
            drop(tx);
            continue;
        }

        if migration.rewrites_irreplaceable {
            let verified = backup
                .as_ref()
                .is_some_and(|verifier| verifier.verified_backup_exists(current_in_lock));
            if !verified {
                drop(tx);
                return Err(Error::Store(format!(
                    "migration to version {} rewrites irreplaceable tables and no verified backup exists at schema version {current_in_lock}; refusing to start",
                    migration.version
                )));
            }
        }

        (migration.apply)(&tx).map_err(|e| {
            Error::Store(format!(
                "migration to version {} failed: {e}",
                migration.version
            ))
        })?;

        tx.execute(
            "INSERT INTO schema_migration (version, applied_at, aub_version, source_revision) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                migration.version,
                clock.now().unix_nanos(),
                crate::build_info::crate_version(),
                crate::build_info::source_revision(),
            ],
        )
        .map_err(|e| store_error("cannot record the applied migration", e))?;

        tx.commit()
            .map_err(|e| store_error("cannot commit the migration", e))?;

        current = migration.version;
        applied.push(migration.version);
    }

    Ok(MigrationSummary {
        applied,
        version: current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::error::ExitClass;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-migrate-test-{}-{suffix}",
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

    fn policy(busy_millis: u64) -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_nanos(busy_millis * 1_000_000),
        }
    }

    fn clock_at(nanos: i64) -> FakeClock {
        FakeClock::new(crate::domain::time::UtcTimestamp::from_unix_nanos(nanos))
    }

    fn fixture_db(scratch: &ScratchDir) -> PathBuf {
        scratch.path().join("meter.db")
    }

    fn migration(
        version: u32,
        rewrites: bool,
        apply: fn(&rusqlite::Connection) -> Result<(), Error>,
    ) -> Migration {
        Migration {
            version,
            rewrites_irreplaceable: rewrites,
            apply,
        }
    }

    /// The test step for the "one migration applies forward" scenario: creates
    /// a table, leaving any pre-existing data untouched.
    fn migration_one(conn: &rusqlite::Connection) -> Result<(), Error> {
        conn.execute_batch("CREATE TABLE migrated (id INTEGER PRIMARY KEY, value TEXT) STRICT")
            .map_err(|e| Error::Store(format!("test migration failed: {e}")))
    }

    /// The test step for the rollback scenario: creates a table and then fails,
    /// so the side effect must be rolled back with the transaction.
    fn migration_fails_after_side_effect(conn: &rusqlite::Connection) -> Result<(), Error> {
        conn.execute_batch("CREATE TABLE should_not_survive (id INTEGER PRIMARY KEY)")
            .map_err(|e| Error::Store(format!("test migration failed: {e}")))?;
        Err(Error::Store("injected migration failure".into()))
    }

    /// The test step for the rewrite scenarios: a destructive rewrite in
    /// miniature, gated by the verified-backup guard.
    fn migration_rewrites(conn: &rusqlite::Connection) -> Result<(), Error> {
        conn.execute_batch("CREATE TABLE rewritten (id INTEGER PRIMARY KEY)")
            .map_err(|e| Error::Store(format!("test migration failed: {e}")))
    }

    /// A backup verifier that records every schema version it was asked about,
    /// so a test can assert the guard asks about the version actually in force.
    #[derive(Default)]
    struct RecordingBackup {
        present: bool,
        queried: RefCell<Vec<u32>>,
    }

    impl RecordingBackup {
        fn new(present: bool) -> Self {
            Self {
                present,
                queried: RefCell::new(Vec::new()),
            }
        }
    }

    impl VerifiedBackup for RecordingBackup {
        fn verified_backup_exists(&self, schema_version: u32) -> bool {
            self.queried.borrow_mut().push(schema_version);
            self.present
        }
    }

    fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        count == 1
    }

    // --- integration: one migration applied forward against a populated fixture ---

    /// A populated fixture database at version 0 migrates to version 1, the
    /// recorded version advances, and the fixture's data survives the step.
    #[test]
    fn one_migration_applies_forward_against_a_populated_fixture_and_the_data_survives() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        conn.execute_batch("CREATE TABLE fixture (id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        conn.execute("INSERT INTO fixture (value) VALUES (?1)", ["alpha"])
            .unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), 0);

        let summary = run_migrations(
            &mut conn,
            &[migration(1, false, migration_one)],
            None,
            &clock_at(5_000),
        )
        .unwrap();

        assert_eq!(summary.applied, vec![1]);
        assert_eq!(summary.version, 1);
        assert_eq!(current_schema_version(&conn).unwrap(), 1);
        assert!(table_exists(&conn, "migrated"));

        let value: String = conn
            .query_row("SELECT value FROM fixture WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            value, "alpha",
            "the fixture's data must survive the migration"
        );
    }

    /// An empty registry (the production state until the first schema bead
    /// lands) is a no-op that records nothing.
    #[test]
    fn an_empty_registry_is_a_no_op() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();

        let summary = run_migrations(&mut conn, &[], None, &clock_at(5_000)).unwrap();
        assert_eq!(summary.applied, Vec::<u32>::new());
        assert_eq!(summary.version, 0);
        assert_eq!(current_schema_version(&conn).unwrap(), 0);
    }

    // --- unit: the applied migration records the aub version and source revision ---

    /// The recorded row carries the injected clock's timestamp and the exact
    /// `aub` version and source revision of the binary that applied it, not a
    /// placeholder. The timestamp assertion pins that the record uses the
    /// injected clock: a mutation reading the real wall clock would record a
    /// value far from 9_000.
    #[test]
    fn applied_migration_records_the_aub_version_and_source_revision() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        run_migrations(
            &mut conn,
            &[migration(1, false, migration_one)],
            None,
            &clock_at(9_000),
        )
        .unwrap();

        let (version, applied_at, aub_version, source_revision): (u32, i64, String, String) = conn
            .query_row(
                "SELECT version, applied_at, aub_version, source_revision FROM schema_migration",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(applied_at, 9_000);
        assert_eq!(aub_version, crate::build_info::crate_version());
        assert_eq!(source_revision, crate::build_info::source_revision());
    }

    // --- unit: lock contention -----------------------------------------------------

    /// A migrator that cannot take the migration lock waits (bounded by the
    /// busy timeout) and then fails with the store class rather than
    /// proceeding: the contender applies nothing and the database is untouched.
    /// Two connections in one process contend through SQLite's file locks
    /// exactly as two processes would, so the test exercises the same lock.
    #[test]
    fn lock_contention_second_migrator_waits_then_fails_with_the_store_class() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut first = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        // Bootstrap the bookkeeping table before the race so the contender is
        // blocked at the migration lock itself, not at table creation.
        current_schema_version(&first).unwrap();

        let mut second = open(&db_path, AccessMode::ReadWrite, &policy(300)).unwrap();

        // The first migrator holds the exclusive transaction (the lock).
        let holder = first
            .transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
            .unwrap();

        let err = run_migrations(
            &mut second,
            &[migration(1, false, migration_one)],
            None,
            &clock_at(5_000),
        )
        .unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("migration lock"), "{message}");

        drop(holder);
        assert_eq!(
            current_schema_version(&second).unwrap(),
            0,
            "the contender must not apply anything while another migrator holds the lock"
        );
        assert!(
            !table_exists(&second, "migrated"),
            "the contender's migration must not run"
        );
    }

    // --- integration: rewrite migrations and the verified-backup guard -------------

    /// A rewrite migration refuses to start when no backup verifier is wired
    /// in at all: absence of evidence is refusal, not permission.
    #[test]
    fn a_rewrite_migration_refused_when_no_backup_verifier_is_available() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();

        let err = run_migrations(
            &mut conn,
            &[migration(1, true, migration_rewrites)],
            None,
            &clock_at(5_000),
        )
        .unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("rewrites irreplaceable"), "{message}");
        assert!(message.contains("no verified backup"), "{message}");
        assert_eq!(current_schema_version(&conn).unwrap(), 0);
        assert!(!table_exists(&conn, "rewritten"));
    }

    /// A rewrite migration refuses to start when the backup verifier reports no
    /// verified backup, and the guard asks about the schema version actually in
    /// force (the version before the rewrite), not a different one.
    #[test]
    fn a_rewrite_migration_refused_with_no_verified_backup_present() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        let backup = RecordingBackup::new(false);

        let err = run_migrations(
            &mut conn,
            &[migration(1, true, migration_rewrites)],
            Some(&backup),
            &clock_at(5_000),
        )
        .unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("no verified backup"), "{message}");
        assert!(message.contains("schema version 0"), "{message}");
        assert_eq!(
            backup.queried.borrow().as_slice(),
            &[0],
            "the guard must ask about the current schema version"
        );
        assert_eq!(current_schema_version(&conn).unwrap(), 0);
    }

    /// A rewrite migration proceeds when the backup verifier reports a verified
    /// backup at the current schema version.
    #[test]
    fn a_rewrite_migration_permitted_with_a_verified_backup() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        let backup = RecordingBackup::new(true);

        let summary = run_migrations(
            &mut conn,
            &[migration(1, true, migration_rewrites)],
            Some(&backup),
            &clock_at(5_000),
        )
        .unwrap();
        assert_eq!(summary.applied, vec![1]);
        assert_eq!(summary.version, 1);
        assert_eq!(
            backup.queried.borrow().as_slice(),
            &[0],
            "the guard must ask about the current schema version"
        );
        assert!(table_exists(&conn, "rewritten"));
    }

    // --- unit: newer schema refused -------------------------------------------------

    /// A database a newer binary migrated past this binary's highest version is
    /// refused with a message naming both versions, and is not touched.
    #[test]
    fn a_database_at_a_newer_schema_version_than_the_binary_is_refused() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        // The bookkeeping table with a record from a newer binary at version 2.
        current_schema_version(&conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migration (version, applied_at, aub_version, source_revision) \
             VALUES (2, 1, '0.9.0', 'aabbccddeeff00112233445566778899aabbccdd')",
            [],
        )
        .unwrap();

        let err = run_migrations(
            &mut conn,
            &[migration(1, false, migration_one)],
            None,
            &clock_at(5_000),
        )
        .unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("newer"), "{message}");
        assert!(message.contains('2'), "{message}");
        assert!(message.contains('1'), "{message}");
        assert_eq!(
            current_schema_version(&conn).unwrap(),
            2,
            "the newer database must not be touched"
        );
    }

    // --- unit: rollback of a failed migration ---------------------------------------

    /// A migration that fails mid-step rolls back entirely: its side effects
    /// vanish and the recorded version stays at the prior value, which is the
    /// recovery-path contract (never a partially migrated database).
    #[test]
    fn a_failed_migration_rolls_back_and_leaves_the_prior_version() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();

        let err = run_migrations(
            &mut conn,
            &[migration(1, false, migration_fails_after_side_effect)],
            None,
            &clock_at(5_000),
        )
        .unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("version 1"), "{message}");
        assert!(message.contains("injected migration failure"), "{message}");
        assert!(
            !table_exists(&conn, "should_not_survive"),
            "the failed migration's side effects must be rolled back"
        );
        assert_eq!(current_schema_version(&conn).unwrap(), 0);
    }

    // --- unit: registry ordering rule ------------------------------------------------

    /// A registry that violates the consecutive-from-1 ordering rule is refused
    /// before anything runs: duplicates, gaps and a version-0 entry are all
    /// defects, because each one would let a later schema bead's migration
    /// silently never apply.
    #[test]
    fn a_registry_with_duplicate_or_nonconsecutive_versions_is_refused() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();

        let duplicate = [
            migration(1, false, migration_one),
            migration(1, false, migration_one),
        ];
        let err = run_migrations(&mut conn, &duplicate, None, &clock_at(5_000)).unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        assert!(err.to_string().contains("consecutive"), "{}", err);

        let gap = [
            migration(1, false, migration_one),
            migration(3, false, migration_one),
        ];
        let err = run_migrations(&mut conn, &gap, None, &clock_at(5_000)).unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        assert!(err.to_string().contains('2'), "{}", err);

        let out_of_order = [
            migration(2, false, migration_one),
            migration(1, false, migration_one),
        ];
        let err = run_migrations(&mut conn, &out_of_order, None, &clock_at(5_000)).unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        assert!(err.to_string().contains("expected 1"), "{}", err);

        let zero = [migration(0, false, migration_one)];
        let err = run_migrations(&mut conn, &zero, None, &clock_at(5_000)).unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        assert!(err.to_string().contains("sentinel"), "{}", err);
    }

    // --- unit: version reading -------------------------------------------------------

    /// A fresh database reads as version zero before anything has applied.
    #[test]
    fn a_fresh_database_reads_schema_version_zero() {
        let scratch = ScratchDir::new();
        let db_path = fixture_db(&scratch);
        let conn = open(&db_path, AccessMode::ReadWrite, &policy(1000)).unwrap();
        assert_eq!(current_schema_version(&conn).unwrap(), 0);
    }
}
