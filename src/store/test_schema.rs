//! The migrated schema for tests, cached across processes in `test-support`.
//!
//! `aub-ycyn` built the migrated schema once per test binary with a `OnceLock`
//! and handed each fixture a file copy. `aub-yr9c` moved that cache into
//! `crates/test-support` and keyed it to a file that outlives the process, so
//! the ~422 ms migration replay is paid once per commit rather than once per
//! process: the 44 `tests/*.rs` integration binaries each ran as their own
//! process and so each rebuilt the schema from scratch.
//!
//! This module stays as the `store`-internal seam its `#[cfg(test)]` callers
//! already import; it now forwards to `test_support::migrated_schema`. The
//! equivalence and independence tests below are the regression check that the
//! delegation did not change behaviour.

use std::path::Path;

use crate::store::connection::{AccessMode, PragmaPolicy, open};

/// Copies the migrated template to `dest`. The destination's parent must exist.
///
/// Only `&Path` crosses into `test-support`: a `cargo test --lib` build links
/// two instances of this crate (the `cfg(test)` test target, and the plain
/// library that `test-support` depends on), so a library type passed across
/// that boundary would not type-check. The template is a file, so nothing has
/// to.
pub fn copy_migrated(dest: &Path) {
    test_support::migrated_schema::copy_migrated(dest);
}

/// A read-write connection to a fresh migrated database at `dest`, under `policy`.
///
/// The drop-in for the `open(...)` + `run_migrations(...)` pair every fixture
/// used to write out by hand.
pub fn open_migrated(dest: &Path, policy: &PragmaPolicy) -> rusqlite::Connection {
    copy_migrated(dest);
    open(dest, AccessMode::ReadWrite, policy)
        .unwrap_or_else(|e| panic!("the copied database must open at {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use test_support::migrated_schema::TEMPLATE_CLOCK_NANOS;

    fn template_policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        }
    }

    /// Reads the schema of a database as SQLite itself describes it: the user
    /// version, and every object's own DDL text, ordered so two databases are
    /// comparable.
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

    /// The property the whole cut rests on: a fixture that copies the template
    /// starts from the same schema as one that replayed the registry.
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

    /// The copy is a fresh database each time, not a shared handle: writing to
    /// one must not reach another. A cut that accidentally handed every fixture
    /// the same file would pass the schema comparison above and corrupt every
    /// test that writes.
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
