//! SQLite connection policy: every connection is opened through one function,
//! applies the required pragmas, and verifies them by readback.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! This is the only module that may open a SQLite connection. The boundary rule
//! `bin/checks/boundary-rules/11-store-connections-through-one-function` greps the
//! source tree for `Connection::open` outside this file, so the pragma policy cannot
//! be bypassed by a caller that opens its own connection.
//!
//! Verification rather than assumption is the point (PLAN.md section 11.3): journal
//! mode is persistent in the database file while `synchronous` and `foreign_keys`
//! are per connection, so a process that assumes an earlier process configured
//! things behaves differently depending on who opened the file first. Every
//! connection therefore sets what it can and reads all of it back, refusing with a
//! store-failure class when a required value is not in effect.

use std::path::{Path, PathBuf};

use crate::domain::time::MonotonicDuration;
use crate::error::Error;

use super::startup::{create_file_mode_0600, force_file_mode_0600};

/// The file name of the one production database (PLAN.md section 11.1: one
/// database, no separate per-purpose stores). Every store-touching command
/// resolves its database under the configured state directory through this
/// constant, so the file is named in exactly one place. The test hooks open
/// their own databases and deliberately do not share it.
pub const LEDGER_DATABASE_FILE: &str = "ledger.db";

/// The journal mode every database must be in (PLAN.md section 11.2). WAL lets a
/// long analytical read coexist with a writer holding the write slot.
const REQUIRED_JOURNAL_MODE: &str = "wal";

/// The durability level for irreplaceable meter writes (PLAN.md section 11.3).
/// SQLite reports `synchronous` as an integer: 0 = OFF, 1 = NORMAL, 2 = FULL.
const REQUIRED_SYNCHRONOUS: &str = "2";

/// Foreign keys are per connection and silently off by default, which makes them
/// exactly the kind of setting that appears to work until the first orphan.
const REQUIRED_FOREIGN_KEYS: &str = "1";

/// The upper bound on a configured busy timeout, in nanoseconds. A value above
/// this is a misconfiguration, not a policy: the point of the bound is that a
/// misconfigured value cannot turn a lock wait into an unbounded hang. 30 seconds
/// is far beyond any write transaction this workload performs (a meter sample
/// commits in milliseconds) while still being a finite wait; `aub-sth.10` owns
/// preserving a successful network result when the bound expires.
const MAX_BUSY_TIMEOUT_NANOS: u64 = 30 * 1_000_000_000;

/// Which access a connection is opened with. Read paths open read-only
/// connections and short snapshots; the write path is the only one that opens for
/// write (PLAN.md section 11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// A read-only connection for a short analytical snapshot. Verifies the
    /// pragma set; cannot set `journal_mode`, which requires write access.
    ReadOnly,
    /// A read-write connection. The only path that may set `journal_mode`,
    /// transitioning a DELETE-journal database to WAL.
    ReadWrite,
    /// A read-only connection to a database this project reads but does not own,
    /// such as an issue tracker's own SQLite file. No pragma policy is applied or
    /// verified: the required journal mode, durability level and foreign-key
    /// setting encode assumptions about a schema this project controls, and
    /// enforcing them against a foreign schema would refuse a healthy foreign
    /// database for disagreeing with settings it was never asked to hold.
    ForeignReadOnly,
}

/// The pragma policy every connection must establish and verify.
///
/// The journal mode, durability level and foreign-key setting are fixed by the
/// design (PLAN.md section 11.3); the one configurable element is the busy
/// timeout, which is bounded by [`MAX_BUSY_TIMEOUT_NANOS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PragmaPolicy {
    /// How long a writer waits for the write slot before SQLite reports busy.
    pub busy_timeout: MonotonicDuration,
}

/// The minimal SQLite surface the pragma policy needs, so tests can inject set and
/// readback failures without a real database.
pub trait PragmaConnection {
    /// Sets a pragma. The value is the pragma's own textual form.
    fn pragma_set(&self, name: &str, value: &str) -> Result<(), Error>;

    /// Reads a pragma's current value back, as the text SQLite reports it.
    fn pragma_read(&self, name: &str) -> Result<String, Error>;
}

impl PragmaConnection for rusqlite::Connection {
    fn pragma_set(&self, name: &str, value: &str) -> Result<(), Error> {
        self.pragma_update(None, name, value)
            .map_err(|e| Error::Store(format!("cannot set PRAGMA {name} = {value}: {e}")))
    }

    fn pragma_read(&self, name: &str) -> Result<String, Error> {
        let value: rusqlite::types::Value =
            self.pragma_query_value(None, name, |row| row.get(0))
                .map_err(|e| Error::Store(format!("cannot read PRAGMA {name}: {e}")))?;
        Ok(match value {
            rusqlite::types::Value::Integer(i) => i.to_string(),
            rusqlite::types::Value::Text(t) => t,
            rusqlite::types::Value::Real(r) => r.to_string(),
            rusqlite::types::Value::Null => "NULL".to_string(),
            rusqlite::types::Value::Blob(_) => "<blob>".to_string(),
        })
    }
}

/// Reads `name` back and refuses when it is not `required`, naming both values so
/// the failure says what was wanted and what was actually in effect.
fn verify_pragma(conn: &dyn PragmaConnection, name: &str, required: &str) -> Result<(), Error> {
    let observed = conn.pragma_read(name)?;
    if observed != required {
        return Err(Error::Store(format!(
            "PRAGMA {name}: required {required}, observed {observed}"
        )));
    }
    Ok(())
}

/// Applies and verifies the pragma policy on an already-open connection.
///
/// `journal_mode` is persistent in the database file and can only be set with
/// write access, so the read-only path verifies it by readback while the
/// read-write path sets it (transitioning a DELETE-journal database to WAL) and
/// then verifies it. The other three pragmas are per connection and are set and
/// verified on every connection. Any failure is a store-failure class refusal
/// before repository work begins.
pub fn apply_policy(
    conn: &dyn PragmaConnection,
    mode: AccessMode,
    policy: &PragmaPolicy,
) -> Result<(), Error> {
    if policy.busy_timeout.as_nanos() > MAX_BUSY_TIMEOUT_NANOS {
        return Err(Error::Store(format!(
            "busy_timeout {}ms exceeds the bound of {}ms",
            policy.busy_timeout.as_nanos() / 1_000_000,
            MAX_BUSY_TIMEOUT_NANOS / 1_000_000,
        )));
    }

    let busy_ms = (policy.busy_timeout.as_nanos() / 1_000_000).to_string();
    conn.pragma_set("busy_timeout", &busy_ms)?;
    verify_pragma(conn, "busy_timeout", &busy_ms)?;

    if mode == AccessMode::ReadWrite {
        conn.pragma_set("journal_mode", "WAL")?;
    }
    verify_pragma(conn, "journal_mode", REQUIRED_JOURNAL_MODE)?;

    conn.pragma_set("synchronous", "FULL")?;
    verify_pragma(conn, "synchronous", REQUIRED_SYNCHRONOUS)?;

    conn.pragma_set("foreign_keys", "ON")?;
    verify_pragma(conn, "foreign_keys", REQUIRED_FOREIGN_KEYS)?;

    Ok(())
}

/// The path SQLite gives its write-ahead log sidecar next to the main database file.
fn wal_sidecar_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}

/// The path SQLite gives its shared-memory index sidecar next to the main database
/// file.
fn shm_sidecar_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push("-shm");
    PathBuf::from(os)
}

/// Opens a database connection through the one setup path every caller uses.
///
/// The write path is the only one that may create the database file; read paths
/// open read-only connections for short analytical snapshots (PLAN.md section
/// 11.2). The pragma policy is applied and verified before the connection is
/// returned, so a connection that cannot establish the required settings is
/// refused rather than handed out.
///
/// On the write path the main database file is created (or repaired, if found
/// wider) at mode 0600 before SQLite ever opens it, so it cannot exist even
/// momentarily at the process's default create mode; after the pragma policy has
/// transitioned the file into WAL, its `-wal` and `-shm` sidecars are repaired to
/// 0600 the same way (PLAN.md line 4775, `aub-c2bw`).
/// Test-only: take the database's single writer slot and hold it until the
/// returned guard is dropped, which is how a test produces the busy database a
/// lock timeout sees in production.
///
/// It lives here rather than at each call site because a caller would otherwise
/// have to name a SQLite transaction type, and two layers are forbidden from
/// doing that at all: `bin/checks/boundary-rules/03-meter-no-sqlite` fails the
/// build when anything under `src/meter/` so much as mentions `rusqlite::`.
/// Returning the guard by inference keeps the knowledge of what a writer slot is
/// in the layer that owns connections.
#[cfg(test)]
pub(crate) fn hold_writer_slot(conn: &mut rusqlite::Connection) -> rusqlite::Transaction<'_> {
    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("a connection with the writer slot free must be able to take it")
}

pub fn open(
    path: &Path,
    mode: AccessMode,
    policy: &PragmaPolicy,
) -> Result<rusqlite::Connection, Error> {
    let flags = match mode {
        AccessMode::ReadOnly | AccessMode::ForeignReadOnly => {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        }
        AccessMode::ReadWrite => {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
        }
    };
    if mode == AccessMode::ReadWrite {
        create_file_mode_0600(path)?;
    }
    let conn = rusqlite::Connection::open_with_flags(path, flags)
        .map_err(|e| Error::Store(format!("cannot open database {path:?}: {e}")))?;
    if mode != AccessMode::ForeignReadOnly {
        apply_policy(&conn, mode, policy)?;
    }
    if mode == AccessMode::ReadWrite {
        force_file_mode_0600(&wal_sidecar_path(path))?;
        force_file_mode_0600(&shm_sidecar_path(path))?;
    }
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{Clock, RealClock};
    use crate::error::ExitClass;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-conn-test-{}-{suffix}",
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

    fn policy() -> PragmaPolicy {
        PragmaPolicy {
            busy_timeout: MonotonicDuration::from_seconds(1),
        }
    }

    /// The value SQLite reports back after a set, modelling the normalization
    /// the real connection performs: journal mode is lower-cased, and
    /// `synchronous` and `foreign_keys` report their integer form.
    fn normalized_pragma_value(name: &str, value: &str) -> String {
        match name {
            "journal_mode" => value.to_lowercase(),
            "synchronous" => match value {
                "FULL" => "2".to_string(),
                "NORMAL" => "1".to_string(),
                "OFF" => "0".to_string(),
                _ => value.to_string(),
            },
            "foreign_keys" => match value {
                "ON" => "1".to_string(),
                "OFF" => "0".to_string(),
                _ => value.to_string(),
            },
            _ => value.to_string(),
        }
    }

    /// A scripted pragma connection: tests inject set and readback failures,
    /// sets that silently do not take effect, and mismatched readbacks without a
    /// real database, and can assert which pragmas were set or read.
    #[derive(Default)]
    struct FakePragmaConnection {
        values: RefCell<BTreeMap<String, String>>,
        fail_set: BTreeMap<String, ()>,
        fail_read: BTreeMap<String, ()>,
        stuck: BTreeMap<String, ()>,
        set_calls: RefCell<Vec<String>>,
        read_calls: RefCell<Vec<String>>,
    }

    impl FakePragmaConnection {
        fn new() -> Self {
            let mut fake = Self::default();
            // The values a correctly configured connection would read back.
            fake.values
                .get_mut()
                .insert("busy_timeout".into(), "1000".into());
            fake.values
                .get_mut()
                .insert("journal_mode".into(), "wal".into());
            fake.values
                .get_mut()
                .insert("synchronous".into(), "2".into());
            fake.values
                .get_mut()
                .insert("foreign_keys".into(), "1".into());
            fake
        }

        fn fail_on_set(mut self, name: &str) -> Self {
            self.fail_set.insert(name.into(), ());
            self
        }

        fn fail_on_read(mut self, name: &str) -> Self {
            self.fail_read.insert(name.into(), ());
            self
        }

        fn stuck_on(mut self, name: &str) -> Self {
            self.stuck.insert(name.into(), ());
            self
        }

        fn with_value(mut self, name: &str, value: &str) -> Self {
            self.values.get_mut().insert(name.into(), value.into());
            self
        }

        fn was_set(&self, name: &str) -> bool {
            self.set_calls.borrow().iter().any(|n| n == name)
        }

        fn was_read(&self, name: &str) -> bool {
            self.read_calls.borrow().iter().any(|n| n == name)
        }
    }

    impl PragmaConnection for FakePragmaConnection {
        fn pragma_set(&self, name: &str, value: &str) -> Result<(), Error> {
            self.set_calls.borrow_mut().push(name.to_string());
            if self.fail_set.contains_key(name) {
                return Err(Error::Store(format!(
                    "injected failure setting PRAGMA {name} = {value}"
                )));
            }
            if self.stuck.contains_key(name) {
                // The set silently does not take effect: the readback observes
                // the value already in effect.
                return Ok(());
            }
            self.values
                .borrow_mut()
                .insert(name.to_string(), normalized_pragma_value(name, value));
            Ok(())
        }

        fn pragma_read(&self, name: &str) -> Result<String, Error> {
            self.read_calls.borrow_mut().push(name.to_string());
            if self.fail_read.contains_key(name) {
                return Err(Error::Store(format!(
                    "injected failure reading PRAGMA {name}"
                )));
            }
            Ok(self.values.borrow().get(name).cloned().unwrap_or_default())
        }
    }

    // --- unit: DELETE-to-WAL transition -------------------------------------------

    /// A database opened in DELETE journal mode is transitioned to WAL by the
    /// read-write path, and the transition is verified by readback.
    #[test]
    fn opening_a_delete_journal_database_transitions_it_to_wal() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");

        // Force the starting state explicitly rather than assuming SQLite's
        // default for a fresh file.
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        raw.pragma_update(None, "journal_mode", "DELETE").unwrap();
        let mode: String = raw
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        drop(raw);

        let conn = open(&db_path, AccessMode::ReadWrite, &policy()).unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    // --- unit: file mode (aub-c2bw) --------------------------------------------------

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// After the first open of a fresh state directory through the production open
    /// path, the database file and every sidecar SQLite created next to it are at
    /// mode 0600, not whatever the process's default create mode would have given
    /// them.
    #[cfg(unix)]
    #[test]
    fn a_fresh_database_and_its_wal_sidecars_are_created_at_mode_0600() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");

        let _conn = open(&db_path, AccessMode::ReadWrite, &policy()).unwrap();

        assert_eq!(mode_of(&db_path), 0o600);
        let wal_path = wal_sidecar_path(&db_path);
        if wal_path.exists() {
            assert_eq!(mode_of(&wal_path), 0o600);
        }
        let shm_path = shm_sidecar_path(&db_path);
        if shm_path.exists() {
            assert_eq!(mode_of(&shm_path), 0o600);
        }
    }

    /// Planted negative: a database file pre-created at a wider mode (0644) is
    /// repaired to 0600 by the production open path rather than left as found.
    #[cfg(unix)]
    #[test]
    fn a_preexisting_database_at_a_wider_mode_is_repaired_to_0600() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        std::fs::write(&db_path, b"").unwrap();
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode_of(&db_path), 0o644);

        let _conn = open(&db_path, AccessMode::ReadWrite, &policy()).unwrap();
        assert_eq!(mode_of(&db_path), 0o600);
    }

    // --- unit: injected set and readback failures ----------------------------------

    /// Every pragma's set failure is a typed store refusal naming the pragma.
    #[test]
    fn injected_set_failures_produce_typed_refusals() {
        for name in [
            "busy_timeout",
            "journal_mode",
            "synchronous",
            "foreign_keys",
        ] {
            let fake = FakePragmaConnection::new().fail_on_set(name);
            let err = apply_policy(&fake, AccessMode::ReadWrite, &policy()).unwrap_err();
            assert_eq!(err.exit_class(), ExitClass::Store, "{name}");
            assert!(err.to_string().contains(name), "{}", err);
        }
    }

    /// Every pragma's readback failure is a typed store refusal naming the pragma.
    #[test]
    fn injected_readback_failures_produce_typed_refusals() {
        for name in [
            "busy_timeout",
            "journal_mode",
            "synchronous",
            "foreign_keys",
        ] {
            let fake = FakePragmaConnection::new().fail_on_read(name);
            let err = apply_policy(&fake, AccessMode::ReadWrite, &policy()).unwrap_err();
            assert_eq!(err.exit_class(), ExitClass::Store, "{name}");
            assert!(err.to_string().contains(name), "{}", err);
        }
    }

    /// A readback that disagrees with the required value names both the required
    /// and the observed value, so the failure says what was wanted and what was
    /// actually in effect.
    #[test]
    fn a_readback_mismatch_names_required_and_observed_values() {
        let fake = FakePragmaConnection::new()
            .with_value("journal_mode", "delete")
            .stuck_on("journal_mode");
        let err = apply_policy(&fake, AccessMode::ReadWrite, &policy()).unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("journal_mode"), "{message}");
        assert!(message.contains("wal"), "{message}");
        assert!(message.contains("delete"), "{message}");
    }

    /// A configured busy timeout above the bound is refused before any pragma is
    /// touched, naming the bound.
    #[test]
    fn a_busy_timeout_above_the_bound_is_refused() {
        let fake = FakePragmaConnection::new();
        let over_bound = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_nanos(MAX_BUSY_TIMEOUT_NANOS + 1),
        };
        let err = apply_policy(&fake, AccessMode::ReadWrite, &over_bound).unwrap_err();
        assert_eq!(err.exit_class(), ExitClass::Store);
        let message = err.to_string();
        assert!(message.contains("bound"), "{message}");
        assert!(message.contains("30000"), "{message}");
    }

    /// A busy timeout exactly at the bound is accepted: the bound is inclusive.
    #[test]
    fn a_busy_timeout_at_the_bound_is_accepted() {
        let fake = FakePragmaConnection::new();
        let at_bound = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_nanos(MAX_BUSY_TIMEOUT_NANOS),
        };
        assert!(apply_policy(&fake, AccessMode::ReadWrite, &at_bound).is_ok());
    }

    // --- unit: read-only path -------------------------------------------------------

    /// A read-only connection verifies WAL by readback and never sets it (setting
    /// journal mode requires write access), while still setting and verifying the
    /// per-connection pragmas.
    #[test]
    fn read_only_connections_verify_wal_without_setting_it() {
        let fake = FakePragmaConnection::new();
        apply_policy(&fake, AccessMode::ReadOnly, &policy()).unwrap();
        assert!(
            !fake.was_set("journal_mode"),
            "a read-only connection must not set journal_mode"
        );
        assert!(
            fake.was_read("journal_mode"),
            "a read-only connection must verify journal_mode by readback"
        );
        for name in ["busy_timeout", "synchronous", "foreign_keys"] {
            assert!(
                fake.was_set(name),
                "{name} must be set on a read-only connection"
            );
            assert!(
                fake.was_read(name),
                "{name} must be verified on a read-only connection"
            );
        }
    }

    /// A read-only connection opened against a WAL database verifies the policy
    /// and is genuinely read-only: a write is refused by SQLite.
    #[test]
    fn a_read_only_connection_verifies_the_policy_and_refuses_writes() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        open(&db_path, AccessMode::ReadWrite, &policy()).unwrap();

        let read = open(&db_path, AccessMode::ReadOnly, &policy()).unwrap();
        let err = read.execute("CREATE TABLE t (x)", []).unwrap_err();
        assert!(
            err.to_string().contains("readonly"),
            "a read-only connection must refuse writes: {err}"
        );
    }

    // --- integration: reader and writer concurrently under WAL ----------------------

    /// A reader is not blocked behind a writer holding the write slot: WAL readers
    /// read the last committed snapshot without waiting for the writer.
    #[test]
    fn a_reader_is_not_blocked_behind_a_writer_under_wal() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        let mut writer = open(&db_path, AccessMode::ReadWrite, &policy()).unwrap();
        writer
            .execute_batch("CREATE TABLE samples (id INTEGER PRIMARY KEY, value INTEGER)")
            .unwrap();

        std::thread::scope(|scope| {
            let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let writer_started = started.clone();
            let db_path = &db_path;
            let policy = &policy();

            let writer_thread = scope.spawn(move || {
                let tx = writer
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                tx.execute("INSERT INTO samples (value) VALUES (1)", [])
                    .unwrap();
                writer_started.store(true, std::sync::atomic::Ordering::Release);
                std::thread::sleep(std::time::Duration::from_millis(200));
                tx.commit().unwrap();
            });

            while !started.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::yield_now();
            }

            let reader = open(db_path, AccessMode::ReadOnly, policy).unwrap();
            let clock = RealClock::new();
            let before = clock.monotonic_now();
            let count: i64 = reader
                .query_row("SELECT COUNT(*) FROM samples", [], |row| row.get(0))
                .unwrap();
            let elapsed = clock.monotonic_now().duration_since(before);

            // The read completes while the writer holds the write slot; a reader
            // blocked behind the writer would take at least the writer's 200 ms
            // hold. 100 ms is a generous bound for an uncontended local read.
            assert!(
                elapsed.as_nanos() < 100_000_000,
                "reader blocked behind writer: {}ms",
                elapsed.as_nanos() / 1_000_000
            );
            // The writer's insert is uncommitted while the reader reads, so the
            // snapshot shows the pre-insert state; the loose bound keeps the test
            // robust to a writer that commits before the reader's query.
            assert!(count == 0 || count == 1, "count = {count}");

            writer_thread.join().unwrap();
        });
    }

    // --- performance: write transaction under synchronous = FULL --------------------

    /// A write transaction under `synchronous = FULL` completes within the
    /// configured busy timeout plus 250 ms harness overhead, and the run logs the
    /// configured timeout, the measured duration and the lock state.
    #[test]
    fn a_write_transaction_under_synchronous_full_completes_within_the_busy_bound() {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy()).unwrap();
        conn.execute_batch("CREATE TABLE samples (id INTEGER PRIMARY KEY, value INTEGER)")
            .unwrap();

        let clock = RealClock::new();
        let before = clock.monotonic_now();
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO samples (value) VALUES (1)", [])
            .unwrap();
        tx.commit().unwrap();
        let elapsed = clock.monotonic_now().duration_since(before);

        let bound = policy().busy_timeout.as_nanos() + 250_000_000;
        assert!(
            elapsed.as_nanos() <= bound,
            "write transaction took {}ms, bound is {}ms",
            elapsed.as_nanos() / 1_000_000,
            bound / 1_000_000,
        );
        eprintln!(
            "store perf: busy_timeout={}ms measured={}ms lock_state=uncontended",
            policy().busy_timeout.as_nanos() / 1_000_000,
            elapsed.as_nanos() / 1_000_000,
        );
    }
}
