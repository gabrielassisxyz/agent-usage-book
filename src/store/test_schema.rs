//! The migrated schema, built once per test binary and copied per fixture.
//!
//! WHY this exists, measured 2026-09-08 (`aub-fx7e`): replaying the migration registry costs
//! **422 ms**, because `store::migrate::run_migrations` applies every migration in its own
//! `BEGIN EXCLUSIVE` committed separately, against a connection opened with
//! `synchronous=FULL`, so one real fsync per migration before a test's first statement. 301 of
//! the 1462 lib unit tests cost 0.3 s or more and hold 95.8% of the suite's 215 s, and at one
//! fixture each that is 127 s of the same schema being rebuilt; four proptest-driven tests
//! replay it 24 or 32 times for about 29 s more. Copying a prebuilt file is a few
//! milliseconds.
//!
//! WHY a file copy and not `:memory:`: many callers reopen the same path with a second
//! connection (`store::repository` opens one per call), and the `drill` and `restore` tests
//! copy a database into an archive and back. An in-memory database would break all of those,
//! so the template is copied to a real file and every caller keeps the file semantics it had.
//!
//! WHY the migration tests must not use this: `store::migrations::*::tests` and
//! `store::migrate`'s own tests exist to exercise the migration path. They keep calling
//! `run_migrations` directly.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;

/// The instant every template row carries in `schema_migration.applied_at`.
///
/// The fixtures this replaces each passed a different fake instant (9_000, 1_000, 0) with no
/// test depending on the value: the only reader of that table outside `store::migrate`'s own
/// tests is `store::retention`, which skips it by name. One value for every fixture is
/// therefore safe, and a test that ever does assert on it will fail visibly against this
/// constant rather than drift.
const TEMPLATE_CLOCK_NANOS: i64 = 1_000;

/// The pragma policy the template is built under. The busy timeout is irrelevant to a
/// single-connection build and is only here because `open` requires a policy; callers pass
/// their own when they open the copy.
fn template_policy() -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1_000),
    }
}

/// The prebuilt migrated database, built on first use in this process.
///
/// The directory is keyed by process id and deliberately outlives the process: a `OnceLock`
/// never drops, so there is nothing to run a cleanup from. It holds one small database file
/// under the test binary's temp directory, which is where every other scratch file in this
/// suite already lands.
fn template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("aub-test-schema-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("template directory must be creatable");
            let path = dir.join("template.db");
            // A previous process with the same id would leave a stale file that the migrations
            // would then find already applied, producing a template nobody built.
            let _ = std::fs::remove_file(&path);

            {
                let mut conn = open(&path, AccessMode::ReadWrite, &template_policy())
                    .expect("template database must open");
                run_migrations(
                    &mut conn,
                    &registry(),
                    None,
                    &FakeClock::new(UtcTimestamp::from_unix_nanos(TEMPLATE_CLOCK_NANOS)),
                )
                .expect("template migrations must apply");
            }
            // The database is in WAL mode, which is persistent in the file. SQLite checkpoints
            // and removes the -wal on the last connection close, which the scope above just
            // did. But that is a property of SQLite's shutdown path, not of this code, and a
            // copy taken while a -wal still held committed pages would be silently short of
            // rows. Assert it rather than trust it: this is the one failure that would produce
            // a template that opens fine and is missing data.
            let wal = path.with_extension("db-wal");
            assert!(
                !wal.exists(),
                "the template's write-ahead log survived the connection close at {}; \
                 copying the database alone would lose committed pages",
                wal.display()
            );
            path
        })
        .as_path()
}

/// Copies the migrated template to `dest`. The destination's parent must exist.
pub fn copy_migrated(dest: &Path) {
    std::fs::copy(template(), dest).unwrap_or_else(|e| {
        panic!(
            "the migrated template must be copyable to {}: {e}",
            dest.display()
        )
    });
}

/// A read-write connection to a fresh migrated database at `dest`, under `policy`.
///
/// This is the drop-in for the `open(...)` + `run_migrations(...)` pair every fixture used to
/// write out by hand.
pub fn open_migrated(dest: &Path, policy: &PragmaPolicy) -> rusqlite::Connection {
    copy_migrated(dest);
    open(dest, AccessMode::ReadWrite, policy)
        .unwrap_or_else(|e| panic!("the copied database must open at {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the schema of a database as SQLite itself describes it: the user version, and
    /// every object's own DDL text, ordered so two databases are comparable.
    fn schema_of(conn: &rusqlite::Connection) -> (i64, Vec<(String, String)>) {
        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version must be readable");
        let mut stmt = conn
            .prepare(
                "SELECT name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("sqlite_master must be queryable");
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("sqlite_master rows must map")
            .collect::<Result<Vec<(String, String)>, _>>()
            .expect("sqlite_master rows must read");
        (user_version, rows)
    }

    /// The property the whole cut rests on: a fixture that copies the template starts from the
    /// same schema as one that replayed the registry. Everything else in this suite is a
    /// regression check for it.
    #[test]
    fn a_template_copy_carries_the_same_schema_as_a_freshly_migrated_database() {
        let dir = std::env::temp_dir().join(format!(
            "aub-test-schema-equiv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock must be after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");

        let copied_path = dir.join("copied.db");
        let copied = open_migrated(&copied_path, &template_policy());

        let fresh_path = dir.join("fresh.db");
        let mut fresh = open(&fresh_path, AccessMode::ReadWrite, &template_policy())
            .expect("fresh database must open");
        run_migrations(
            &mut fresh,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(TEMPLATE_CLOCK_NANOS)),
        )
        .expect("fresh migrations must apply");

        assert_eq!(
            schema_of(&copied),
            schema_of(&fresh),
            "a copied template and a freshly migrated database must be indistinguishable by \
             user_version and by every object's DDL"
        );

        drop(copied);
        drop(fresh);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The copy is a fresh database each time, not a shared handle: writing to one must not
    /// reach another. A cut that accidentally handed every fixture the same file would pass
    /// the schema comparison above and corrupt every test that writes.
    #[test]
    fn two_copies_are_independent_databases() {
        let dir = std::env::temp_dir().join(format!(
            "aub-test-schema-indep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock must be after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir must be creatable");

        let a = open_migrated(&dir.join("a.db"), &template_policy());
        let b = open_migrated(&dir.join("b.db"), &template_policy());

        a.execute(
            "INSERT INTO account \
             (logical_name, provider_key, first_observed_at, last_observed_at) \
             VALUES ('only-in-a', 'p', 1, 1)",
            [],
        )
        .expect("the insert must apply to a");

        let in_b: i64 = b
            .query_row(
                "SELECT count(*) FROM account WHERE logical_name = 'only-in-a'",
                [],
                |row| row.get(0),
            )
            .expect("b must be queryable");
        assert_eq!(
            in_b, 0,
            "a row written to one copy must not appear in another"
        );

        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
