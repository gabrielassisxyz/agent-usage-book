//! The `sample_run` table: one row per invocation or sampling batch (PLAN.md
//! 12.2). Useful for diagnosing the case where every account failed at once,
//! since the run is what those attempts had in common.
//!
//! Deliberately carries no account and no single policy snapshot: a run can
//! span many accounts, each sampled under its own account-specific policy
//! snapshot (`sampling_policy_snapshot`), so this table never fixes one.

use rusqlite::{OptionalExtension, params};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;

/// A `sample_run` row's identity: its SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SampleRunId(i64);

impl SampleRunId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// What started a sampling run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    Timer,
    Hook,
    Manual,
    Live,
}

impl Trigger {
    fn as_sql(self) -> &'static str {
        match self {
            Trigger::Timer => "timer",
            Trigger::Hook => "hook",
            Trigger::Manual => "manual",
            Trigger::Live => "live",
        }
    }

    fn from_sql(value: &str) -> Result<Self, Error> {
        match value {
            "timer" => Ok(Trigger::Timer),
            "hook" => Ok(Trigger::Hook),
            "manual" => Ok(Trigger::Manual),
            "live" => Ok(Trigger::Live),
            other => Err(Error::Store(format!(
                "unknown sample_run trigger stored in the database: {other:?}"
            ))),
        }
    }
}

/// One invocation or sampling batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleRun {
    id: SampleRunId,
    trigger: Trigger,
    started_at: UtcTimestamp,
    ended_at: Option<UtcTimestamp>,
    aub_version: String,
    configuration_fingerprint: String,
}

impl SampleRun {
    pub fn id(&self) -> SampleRunId {
        self.id
    }

    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    pub fn started_at(&self) -> UtcTimestamp {
        self.started_at
    }

    pub fn ended_at(&self) -> Option<UtcTimestamp> {
        self.ended_at
    }

    pub fn aub_version(&self) -> &str {
        &self.aub_version
    }

    pub fn configuration_fingerprint(&self) -> &str {
        &self.configuration_fingerprint
    }
}

fn row_to_sample_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<SampleRun> {
    let trigger_sql: String = row.get(1)?;
    let trigger = Trigger::from_sql(&trigger_sql).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let ended_at: Option<i64> = row.get(3)?;
    Ok(SampleRun {
        id: SampleRunId::new(row.get(0)?),
        trigger,
        started_at: UtcTimestamp::from_unix_nanos(row.get(2)?),
        ended_at: ended_at.map(UtcTimestamp::from_unix_nanos),
        aub_version: row.get(4)?,
        configuration_fingerprint: row.get(5)?,
    })
}

/// Starts a sample run: durable before any account in the batch is sampled.
/// `aub_version` is stamped from the running binary (`crate::build_info`),
/// matching how the migration framework stamps its own audit columns: a
/// caller cannot record a version other than the binary it is actually
/// running.
pub fn start_sample_run(
    conn: &rusqlite::Connection,
    trigger: Trigger,
    started_at: UtcTimestamp,
    configuration_fingerprint: &str,
) -> Result<SampleRunId, Error> {
    conn.query_row(
        "INSERT INTO sample_run (trigger, started_at, aub_version, configuration_fingerprint)
         VALUES (?1, ?2, ?3, ?4)
         RETURNING id",
        params![
            trigger.as_sql(),
            started_at.unix_nanos(),
            crate::build_info::crate_version(),
            configuration_fingerprint,
        ],
        |row| row.get(0),
    )
    .map(SampleRunId::new)
    .map_err(|e| Error::Store(format!("cannot start sample run: {e}")))
}

/// Reads one sample run by id, or `None` if no such run exists.
pub fn sample_run_by_id(
    conn: &rusqlite::Connection,
    id: SampleRunId,
) -> Result<Option<SampleRun>, Error> {
    conn.query_row(
        "SELECT id, trigger, started_at, ended_at, aub_version, configuration_fingerprint
         FROM sample_run WHERE id = ?1",
        params![id.value()],
        row_to_sample_run,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read sample run {}: {e}", id.value())))
}

/// The started instants of every timer-triggered sample run in `[start, end)`,
/// oldest first. The coverage engine reads these to report the most recent
/// timer run the scheduler actually made (aub-me5.9).
pub fn timer_run_times_between(
    conn: &rusqlite::Connection,
    start: UtcTimestamp,
    end: UtcTimestamp,
) -> Result<Vec<UtcTimestamp>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT started_at FROM sample_run
             WHERE trigger = 'timer' AND started_at >= ?1 AND started_at < ?2
             ORDER BY started_at",
        )
        .map_err(|e| Error::Store(format!("cannot read timer runs: {e}")))?;
    let rows = statement
        .query_map(params![start.unix_nanos(), end.unix_nanos()], |row| {
            row.get::<_, i64>(0).map(UtcTimestamp::from_unix_nanos)
        })
        .map_err(|e| Error::Store(format!("cannot read timer runs: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read timer runs: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-sample-run-test-{}-{suffix}",
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
        let db_path = scratch.path().join("meter.db");
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    /// Every trigger kind round-trips through the store without loss: what is
    /// written is exactly what is read back.
    #[test]
    fn every_trigger_kind_round_trips_through_the_store() {
        let (_scratch, conn) = fixture_conn();
        for trigger in [
            Trigger::Timer,
            Trigger::Hook,
            Trigger::Manual,
            Trigger::Live,
        ] {
            let id =
                start_sample_run(&conn, trigger, UtcTimestamp::from_unix_nanos(0), "fp").unwrap();
            let run = sample_run_by_id(&conn, id).unwrap().unwrap();
            assert_eq!(run.trigger(), trigger);
        }
    }

    #[test]
    fn a_started_run_carries_the_running_binarys_aub_version_and_no_end_time() {
        let (_scratch, conn) = fixture_conn();
        let id = start_sample_run(
            &conn,
            Trigger::Timer,
            UtcTimestamp::from_unix_nanos(1_000),
            "fp-abc",
        )
        .unwrap();
        let run = sample_run_by_id(&conn, id).unwrap().unwrap();
        assert_eq!(run.aub_version(), crate::build_info::crate_version());
        assert_eq!(run.configuration_fingerprint(), "fp-abc");
        assert_eq!(run.ended_at(), None);
    }
}

#[cfg(test)]
mod coverage_query_tests {
    use super::*;
    use crate::domain::time::{MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-sample-run-coverage-test-{}-{suffix}",
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

    /// Timer runs are the scheduler's own trace: the coverage command reads
    /// exactly the timer-triggered runs inside the interval, in order, and a
    /// manual or hook run never reads as one.
    #[test]
    fn timer_runs_read_only_timer_triggers_inside_the_interval() {
        let scratch = ScratchDir::new();
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("meter.db"),
            AccessMode::ReadWrite,
            &policy,
        )
        .expect("fixture connection must open");
        let clock_at =
            |nanos: i64| crate::domain::time::FakeClock::new(UtcTimestamp::from_unix_nanos(nanos));
        run_migrations(&mut conn, &registry(), None, &clock_at(0))
            .expect("fixture migrations must apply");

        for (trigger, at) in [
            (Trigger::Timer, 1_000),
            (Trigger::Manual, 2_000),
            (Trigger::Timer, 3_000),
            (Trigger::Hook, 4_000),
            (Trigger::Timer, 9_000),
        ] {
            start_sample_run(&conn, trigger, UtcTimestamp::from_unix_nanos(at), "test")
                .expect("the sample run must insert");
        }

        let runs = timer_run_times_between(
            &conn,
            UtcTimestamp::from_unix_nanos(0),
            UtcTimestamp::from_unix_nanos(5_000),
        )
        .expect("the timer read must succeed");
        assert_eq!(
            runs,
            vec![
                UtcTimestamp::from_unix_nanos(1_000),
                UtcTimestamp::from_unix_nanos(3_000),
            ],
            "only timer-triggered runs inside the half-open interval, in order"
        );

        let empty = timer_run_times_between(
            &conn,
            UtcTimestamp::from_unix_nanos(5_000),
            UtcTimestamp::from_unix_nanos(6_000),
        )
        .expect("the empty-window read must succeed");
        assert!(
            empty.is_empty(),
            "an interval with no timer runs reads empty"
        );
    }
}
