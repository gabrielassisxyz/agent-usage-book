//! The migrated test schema, built once and cached in a file that outlives the
//! process that built it.
//!
//! WHY this exists, measured 2026-09-08 (`aub-fx7e`, `aub-ycyn`): replaying the
//! migration registry costs ~422 ms, because `store::migrate::run_migrations`
//! applies every migration in its own `BEGIN EXCLUSIVE` committed separately,
//! against a connection opened with `synchronous=FULL`, so one real fsync per
//! migration before a test's first statement. `aub-ycyn` paid that once per
//! process with a `OnceLock`; this crate pays it once per *commit*, shared by
//! the library's `#[cfg(test)]` code and by every `tests/*.rs` integration
//! binary, which each run as their own process and so each rebuilt the schema
//! from scratch.
//!
//! WHY a file copy and not `:memory:`: many callers reopen the same path with a
//! second connection, and some copy a database into an archive and back. An
//! in-memory database breaks all of those.
//!
//! WHY the migration tests must not use this: `tests/migration_matrix.rs`,
//! `store::migrations::*::tests` and `store::migrate`'s own tests exist to
//! exercise the migration path. They keep calling `run_migrations` directly.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use rusqlite::backup::{Backup, StepResult};

use agent_usage_book::build_info::source_revision;
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;

/// The instant every template row carries in `schema_migration.applied_at`.
///
/// The fixtures this replaces each passed a different fake instant with no test
/// depending on the value: the only reader of that table outside `store::migrate`'s
/// own tests is `store::retention`, which skips it by name. One value for every
/// fixture is therefore safe.
pub const TEMPLATE_CLOCK_NANOS: i64 = 1_000;

/// A per-writer disambiguator for the in-progress temp file name, so two threads
/// (or a thread and a retry) in one process never collide on a name. The process
/// id covers the cross-process case.
static WRITER_SEQ: AtomicU64 = AtomicU64::new(0);

/// The pragma policy the template is built under. The busy timeout is irrelevant
/// to a single-connection build and is only here because `open` requires a
/// policy; callers pass their own when they open the copy.
fn template_policy() -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1_000),
    }
}

/// The highest migration version this build knows, read statically from the
/// registry with no open database. It changes exactly when a migration is
/// appended, and this repository forbids editing an applied migration, so it is
/// a sound key for the case that rule covers. `source_revision()` in the key
/// below covers the case it does not: an edit to an existing migration's body.
fn schema_max_version() -> u32 {
    registry()
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// Where cached templates live. `$CARGO_TARGET_DIR` when set, which `bin/ci`
/// points per worktree on purpose: two worktrees on different migration counts
/// must not read each other's template. A machine-wide `temp_dir()` cache would
/// reintroduce exactly that hazard, so the fallback when the variable is unset
/// is the workspace `target/` beside this crate, not the temp directory.
fn cache_root() -> PathBuf {
    let base = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"),
    };
    base.join("aub-test-schema-cache")
}

/// The cached template path for a given key. Pure in its arguments so a test can
/// assert that changing either component changes the path.
fn template_path_for(max_version: u32, revision: &str) -> PathBuf {
    cache_root()
        .join(format!("migrated-schema-v{max_version}-{revision}"))
        .join("template.db")
}

/// The cached template path for this build.
pub fn cached_template_path() -> PathBuf {
    template_path_for(schema_max_version(), source_revision())
}

/// Appends a literal suffix to a path (SQLite's `-wal`/`-shm` sidecars append,
/// they do not replace an extension).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut raw: OsString = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

/// Removes a database file and every sidecar SQLite may have left beside it.
fn remove_db_family(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(with_suffix(path, suffix));
    }
}

fn fsync_file(path: &Path) {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .unwrap_or_else(|e| panic!("cannot fsync the built template {}: {e}", path.display()));
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) {
    fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .unwrap_or_else(|e| panic!("cannot fsync the cache directory {}: {e}", dir.display()));
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) {}

/// Builds the migrated schema into `final_path` using the write-temp, fsync,
/// atomic-rename, fsync-dir sequence of `src/store/spool.rs:436-523`: a reader
/// never observes a partially written database, and two processes racing to
/// build the template cannot corrupt each other, with the rename deciding which
/// one wins. Idempotent: a rename onto an existing complete template just
/// replaces it atomically with another complete one.
fn build_template_at(final_path: &Path) {
    let dir = final_path
        .parent()
        .expect("the template path must have a parent directory");
    fs::create_dir_all(dir).expect("the cache directory must be creatable");

    let token = WRITER_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp_path = dir.join(format!(
        ".template.db.building-{}-{token}",
        std::process::id()
    ));
    remove_db_family(&temp_path);

    {
        let mut conn = open(&temp_path, AccessMode::ReadWrite, &template_policy())
            .expect("the template database must open");
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(TEMPLATE_CLOCK_NANOS)),
        )
        .expect("the template migrations must apply");
        // Fold the write-ahead log back into the main file and delete it before
        // the file is published. `src/store/test_schema.rs` records the reason:
        // a copy taken while a `-wal` still holds committed pages opens cleanly
        // and is silently short of rows.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("the template's write-ahead log must checkpoint");
    }

    let wal = with_suffix(&temp_path, "-wal");
    assert!(
        !wal.exists(),
        "the template's write-ahead log survived the checkpoint and connection close at {}; \
         publishing the database file alone would lose committed pages",
        wal.display()
    );

    fsync_file(&temp_path);
    fs::rename(&temp_path, final_path).unwrap_or_else(|e| {
        let _ = fs::remove_file(&temp_path);
        panic!(
            "cannot publish the migrated template to {}: {e}",
            final_path.display()
        )
    });
    // Published read-only, and this is a correctness guard rather than tidiness. The cache key
    // is the commit, so every build of a dirty working tree shares one entry: a momentary bug
    // that writes into the template poisons an entry that outlives the fix, and every later
    // copy carries those rows. That happened on 2026-09-08 - a cached template ended a run with
    // account=2 and ingest_quarantine=5, and seven converted integration binaries went red on
    // rows they never inserted, with a failure count that moved between runs because it
    // depended on which test copied first. Read-only makes the write fail loudly at the moment
    // it happens instead of silently, and `template()` rebuilds any entry it finds writable.
    set_readonly(final_path);
    fsync_dir(dir);
}

/// Marks a published template read-only. Best effort: a filesystem that cannot represent it
/// leaves the guard in `template()` as the remaining defence.
fn set_readonly(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = fs::set_permissions(path, perms);
    }
}

/// Whether a cached template was published by this code. A writable file was either written by
/// something that had no business writing to it, or predates the read-only publish; either way
/// its contents are not trustworthy and it is cheaper to rebuild it than to reason about it.
fn is_published_readonly(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false)
}

/// Makes `path` a template this code published: builds it when absent, and REBUILDS it when it
/// is present but writable. A writable entry was either written by something with no business
/// writing to it or predates the read-only publish, and in both cases its rows are not
/// trustworthy. Factored out of `template()` so the rebuild decision has a test: a `OnceLock`
/// resolves once per process and cannot be exercised twice.
fn ensure_template_at(path: &Path) {
    if path.exists() && is_published_readonly(path) {
        return;
    }
    remove_db_family(path);
    build_template_at(path);
}

/// The cached template for this process, built on first use if the cross-process
/// cache is cold. A `OnceLock` never drops, so there is nothing to clean up from,
/// and it serializes concurrent first callers in one process onto a single build.
fn template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE
        .get_or_init(|| {
            let path = cached_template_path();
            ensure_template_at(&path);
            path
        })
        .as_path()
}

/// Copies the migrated template to `dest`. The destination's parent must exist.
pub fn copy_migrated(dest: &Path) {
    fs::copy(template(), dest).unwrap_or_else(|e| {
        panic!(
            "the migrated template must be copyable to {}: {e}",
            dest.display()
        )
    });
    // `fs::copy` carries the source's permission bits, and the template is published read-only,
    // so without this every fixture would receive a database it cannot write to.
    if let Ok(meta) = fs::metadata(dest) {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(dest, perms).unwrap_or_else(|e| {
            panic!("the copied database must be writable at {}: {e}", dest.display())
        });
    }
}

/// A read-write connection to a fresh migrated database at `dest`, under `policy`.
///
/// This is the drop-in for the `open(...)` + `run_migrations(...)` pair every
/// fixture used to write out by hand.
pub fn open_migrated(dest: &Path, policy: &PragmaPolicy) -> rusqlite::Connection {
    copy_migrated(dest);
    open(dest, AccessMode::ReadWrite, policy)
        .unwrap_or_else(|e| panic!("the copied database must open at {}: {e}", dest.display()))
}

/// The migrated template held in memory, one per process, for fixtures that
/// want an in-memory database rather than a file.
///
/// Populated from the file template through a scratch copy, which is removed as
/// soon as its pages are in memory: the template itself is published read-only
/// under the store's WAL policy, and this keeps every reader of that file on the
/// one path `copy_migrated` already exercises. A `Mutex` and not a bare
/// `OnceLock<Connection>` because `rusqlite::Connection` is `Send` but not
/// `Sync`, and the tests that clone from it run on many threads.
fn in_memory_template() -> &'static Mutex<rusqlite::Connection> {
    static TEMPLATE: OnceLock<Mutex<rusqlite::Connection>> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let scratch = std::env::temp_dir().join(format!(
            "aub-migrated-template-{}-{}.db",
            std::process::id(),
            WRITER_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        remove_db_family(&scratch);
        copy_migrated(&scratch);
        let mut conn =
            rusqlite::Connection::open_in_memory().expect("the in-memory template must open");
        conn.restore(
            rusqlite::DatabaseName::Main,
            &scratch,
            None::<fn(rusqlite::backup::Progress)>,
        )
        .unwrap_or_else(|e| {
            panic!(
                "the migrated template must restore into memory from {}: {e}",
                scratch.display()
            )
        });
        remove_db_family(&scratch);
        Mutex::new(conn)
    })
}

/// A fresh, writable in-memory database carrying the migrated schema.
///
/// The drop-in for the `open_in_memory()` + `run_migrations(...)` pair, for a
/// fixture that never reopens its database by path. Each call clones the whole
/// template through SQLite's backup API into its own `:memory:` connection, so
/// two callers share no page and no lock once this returns.
///
/// WHY this exists next to `open_migrated`, measured 2026-09-09 (`aub-iatc`):
/// `tests/reconciliation.rs` replayed the registry ~530 times per run, once per
/// test and once per proptest case, for 62 s of a 293 s `30-test`. An on-disk
/// copy per case would have moved that cost into `synchronous=FULL` writes
/// instead of removing it; a page copy between two in-memory databases costs
/// neither the replay nor the fsync.
pub fn open_migrated_in_memory() -> rusqlite::Connection {
    let source = in_memory_template()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut dest =
        rusqlite::Connection::open_in_memory().expect("the in-memory database must open");
    let backup = Backup::new(&source, &mut dest).expect("the template backup must start");
    // A negative page count copies everything in one step: the template is a
    // few hundred pages, and holding the source lock for that long is cheaper
    // than the bookkeeping of stepping through it.
    let outcome = backup.step(-1).expect("the template pages must copy");
    assert_eq!(
        outcome,
        StepResult::Done,
        "one step over the whole template must finish the copy"
    );
    drop(backup);
    drop(source);
    dest
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_usage_book::store::migrate::recorded_schema_version;
    use std::sync::{Arc, Barrier};

    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must be after the epoch")
            .as_nanos();
        let seq = WRITER_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aub-migrated-schema-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        dir
    }

    /// The cache key covers both components: two builds that differ only in the
    /// migration count, or only in the source revision, must not read one
    /// template for the other. Mutation: drop `revision` from `template_path_for`
    /// and the second assertion goes red.
    #[test]
    fn the_cache_path_changes_with_either_key_component() {
        let base = template_path_for(41, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let newer_schema = template_path_for(42, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let other_revision = template_path_for(41, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        assert_ne!(
            base, newer_schema,
            "a new migration must produce a different cache path"
        );
        assert_ne!(
            base, other_revision,
            "a different source revision must produce a different cache path"
        );
    }

    /// The published template carries the whole schema and no `-wal` sibling: a
    /// copy of it opens at the highest migration version with every row a fresh
    /// replay would have.
    #[test]
    fn the_template_is_complete_and_has_no_write_ahead_log() {
        let dir = scratch_dir("complete");
        let template_path = dir.join("template.db");
        build_template_at(&template_path);

        assert!(
            !with_suffix(&template_path, "-wal").exists(),
            "the published template must not have a -wal sibling"
        );

        let copy_path = dir.join("copy.db");
        let conn = open_migrated_from(&template_path, &copy_path);
        assert_eq!(
            recorded_schema_version(&conn).expect("the copy must report a schema version"),
            schema_max_version(),
            "a copy of the template must be at the highest migration version"
        );
        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The guard that would have caught the failure this rule exists for. On 2026-09-08 a
    /// cached template finished a run holding account=2 and ingest_quarantine=5, and seven
    /// converted integration binaries went red on rows they had never inserted, because the
    /// cache key is the commit and a dirty working tree reuses one entry across builds. A
    /// poisoned entry must be rebuilt, not read.
    #[test]
    fn a_writable_cache_entry_is_rebuilt_rather_than_trusted() {
        let dir = scratch_dir("poisoned");
        let path = dir.join("template.db");

        ensure_template_at(&path);
        assert!(
            is_published_readonly(&path),
            "a freshly published template must be read-only"
        );

        // Poison it exactly as a stray write would: make it writable and insert a row.
        let mut perms = fs::metadata(&path).expect("stat").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(&path, perms).expect("the entry must become writable");
        {
            let conn = open(&path, AccessMode::ReadWrite, &template_policy())
                .expect("the poisoned entry must open");
            conn.execute(
                "INSERT INTO account \
                 (logical_name, provider_key, first_observed_at, last_observed_at) \
                 VALUES ('poison', 'p', 1, 1)",
                [],
            )
            .expect("the poison row must insert");
        }

        ensure_template_at(&path);

        assert!(
            is_published_readonly(&path),
            "the rebuilt template must be read-only again"
        );
        let dest = dir.join("copy.db");
        let conn = open_migrated_from(&path, &dest);
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM account", [], |r| r.get(0))
            .expect("the rebuilt copy must be queryable");
        assert_eq!(
            rows, 0,
            "a rebuilt template must carry no rows from the entry it replaced"
        );

        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Opens a fresh copy of an explicit template path (the production
    /// `open_migrated` goes through the process cache).
    fn open_migrated_from(template_path: &Path, dest: &Path) -> rusqlite::Connection {
        fs::copy(template_path, dest).expect("the template must be copyable");
        // The template is published read-only and `fs::copy` carries its mode, so the copy has
        // to be made writable exactly as `copy_migrated` does it.
        let mut perms = fs::metadata(dest)
            .expect("the copy must stat")
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(dest, perms).expect("the copy must become writable");
        open(dest, AccessMode::ReadWrite, &template_policy()).expect("the copy must open")
    }

    /// Two threads build the same cold template concurrently, while a third
    /// watches the destination: every time the file exists it must already be a
    /// complete database. The atomic rename is what makes that hold; a build
    /// that wrote the final path in place would let the watcher see a truncated
    /// or half-migrated file. Both builders must finish with a complete
    /// template.
    #[test]
    fn two_threads_on_a_cold_cache_never_expose_a_partial_file() {
        let dir = scratch_dir("race");
        let template_path = dir.join("template.db");

        let start = Arc::new(Barrier::new(3));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let watcher = {
            let template_path = template_path.clone();
            let probe_path = dir.join("watch-copy.db");
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                start.wait();
                let mut observed_complete = false;
                while !done.load(Ordering::Relaxed) {
                    remove_db_family(&probe_path);
                    // A same-filesystem rename is atomic, so this copy sees
                    // either no file or a fully published one, never a prefix.
                    if fs::copy(&template_path, &probe_path).is_err() {
                        continue;
                    }
                    let len = fs::metadata(&probe_path).map(|m| m.len()).unwrap_or(0);
                    if len == 0 {
                        continue;
                    }
                    // Read-only: a published template is read-only on disk, and the watcher
                    // only reads its schema version.
                    let conn = open(&probe_path, AccessMode::ReadOnly, &template_policy())
                        .expect("a published, non-empty template must open as a database");
                    assert_eq!(
                        recorded_schema_version(&conn).ok(),
                        Some(schema_max_version()),
                        "the watcher observed a non-empty template that is not fully migrated: \
                         an atomic rename of a completed build makes this impossible"
                    );
                    observed_complete = true;
                }
                observed_complete
            })
        };

        let builders: Vec<_> = (0..2)
            .map(|_| {
                let template_path = template_path.clone();
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    build_template_at(&template_path);
                })
            })
            .collect();

        for builder in builders {
            builder.join().expect("a builder thread must not panic");
        }
        done.store(true, Ordering::Relaxed);
        watcher.join().expect("the watcher thread must not panic");

        let final_copy = dir.join("final-copy.db");
        let conn = open_migrated_from(&template_path, &final_copy);
        assert_eq!(
            recorded_schema_version(&conn).expect("the final template must report a version"),
            schema_max_version(),
            "after the race the template must be a complete migrated database"
        );
        drop(conn);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Each in-memory clone is a complete, writable database of its own: at the
    /// highest migration version, and sharing no rows with its siblings. Mutation:
    /// return the template's own connection instead of a backup and the second
    /// assertion goes red.
    #[test]
    fn an_in_memory_clone_is_complete_and_independent() {
        let first = open_migrated_in_memory();
        let second = open_migrated_in_memory();
        assert_eq!(
            recorded_schema_version(&first).expect("the clone must report a schema version"),
            schema_max_version(),
            "an in-memory clone must be at the highest migration version"
        );
        first
            .execute(
                "INSERT INTO account \
                 (logical_name, provider_key, first_observed_at, last_observed_at) \
                 VALUES ('only-in-first', 'p', 1, 1)",
                [],
            )
            .expect("a clone must be writable");
        let rows: i64 = second
            .query_row("SELECT count(*) FROM account", [], |r| r.get(0))
            .expect("the sibling clone must be queryable");
        assert_eq!(
            rows, 0,
            "a row written to one clone must not appear in another"
        );
        let third = open_migrated_in_memory();
        let rows: i64 = third
            .query_row("SELECT count(*) FROM account", [], |r| r.get(0))
            .expect("a later clone must be queryable");
        assert_eq!(
            rows, 0,
            "a row written to one clone must not leak into the template"
        );
    }
}
