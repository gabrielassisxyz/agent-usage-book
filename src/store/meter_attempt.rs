//! The `meter_attempt` and `meter_attempt_result` tables: the two-stage
//! append-only attempt lifecycle as durable schema (PLAN.md invariants 23 and
//! 24, sections 12.3, 13, 30).
//!
//! `start_meter_attempt` is durable before any network I/O; `record_meter_attempt_result`
//! is the separate, later terminal fact. A started attempt with no result past the
//! command's execution horizon reads as collector interruption, never as an endpoint
//! timeout and never as "no attempt occurred", because the start itself is evidence
//! the database holds onto. This module exposes insert and read only: the tables'
//! triggers reject every `UPDATE` and `DELETE`, and nothing here offers a path
//! around them.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation

use rusqlite::{OptionalExtension, params};

use crate::domain::attempt::{AttemptId, AttemptOutcome};
use crate::domain::failure::FailureClass;
use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::error::Error;

/// The retention class for the meter attempt tables (PLAN.md 11.5: forever).
pub const RETENTION_CLASS: &str = "forever";

/// Why this attempt was due, over the reasons the due decision names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DueReason {
    OrdinaryCadence,
    ResetEdge,
    PostResetConfirmation,
    ForcedOrManual,
}

impl DueReason {
    const fn as_sql(self) -> &'static str {
        match self {
            DueReason::OrdinaryCadence => "ordinary_cadence",
            DueReason::ResetEdge => "reset_edge",
            DueReason::PostResetConfirmation => "post_reset_confirmation",
            DueReason::ForcedOrManual => "forced_or_manual",
        }
    }

    fn from_sql(value: &str) -> Result<Self, Error> {
        match value {
            "ordinary_cadence" => Ok(DueReason::OrdinaryCadence),
            "reset_edge" => Ok(DueReason::ResetEdge),
            "post_reset_confirmation" => Ok(DueReason::PostResetConfirmation),
            "forced_or_manual" => Ok(DueReason::ForcedOrManual),
            other => Err(Error::Store(format!(
                "unknown meter_attempt due_reason stored in the database: {other:?}"
            ))),
        }
    }
}

/// A meter attempt row's identity in storage, which is the raw sequence value
/// the domain [`AttemptId`] wraps (attempt.rs: "the attempt row's identity in
/// storage").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MeterAttemptRowId(i64);

impl MeterAttemptRowId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }

    /// The domain identifier this storage row names.
    pub fn as_attempt_id(self) -> Result<AttemptId, Error> {
        u64::try_from(self.0)
            .map(AttemptId::new)
            .map_err(|_| Error::Store(format!("negative meter_attempt rowid: {}", self.0)))
    }
}

/// Which prior fact a due decision was based on: the prior attempt itself, or
/// its terminal result. Never both, and neither on the first attempt of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueBasis {
    Attempt { row_id: MeterAttemptRowId },
    Result { attempt_id: MeterAttemptRowId },
}

/// The durable start of one collection attempt, written before any network
/// I/O begins. There is no update form of this value on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeterAttempt {
    pub run_id: crate::store::sample_run::SampleRunId,
    pub account_id: crate::store::account::AccountId,
    pub provider: String,
    pub request_started_at: UtcTimestamp,
    pub credential_context_id: Option<String>,
    pub policy_snapshot_id: crate::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
    pub due_at: UtcTimestamp,
    pub due_reason: DueReason,
    pub due_basis: Option<DueBasis>,
    pub provider_contract_id: String,
    pub meter_semantics_id: String,
}

/// One stored start row. Fields read back exactly as they were written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeterAttempt {
    pub row_id: MeterAttemptRowId,
    pub run_id: crate::store::sample_run::SampleRunId,
    pub account_id: crate::store::account::AccountId,
    pub provider: String,
    pub request_started_at: UtcTimestamp,
    pub credential_context_id: Option<String>,
    pub policy_snapshot_id: crate::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
    pub due_at: UtcTimestamp,
    pub due_reason: DueReason,
    pub due_basis: Option<DueBasis>,
    pub provider_contract_id: String,
    pub meter_semantics_id: String,
}

/// The terminal fact about one attempt, insertable at most once per attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMeterAttemptResult {
    pub attempt_id: MeterAttemptRowId,
    pub completed_at: UtcTimestamp,
    pub elapsed: MonotonicDuration,
    pub outcome: AttemptOutcome,
    pub sanitized_error_classification: Option<String>,
    pub retry_index: Option<u32>,
    /// Set when a demonstrably backwards clock produced a `completed_at`
    /// before the attempt started: the database's explicit anomaly marker,
    /// which is what lets the ordering constraint stand for every other row
    /// instead of being relaxed for all of them.
    pub clock_anomaly: bool,
}

/// One stored terminal result. The failure class is carried as the database
/// stores it: present exactly when the outcome is unreachable, the retry
/// delay only for a rate-limit class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeterAttemptResult {
    pub attempt_id: MeterAttemptRowId,
    pub completed_at: UtcTimestamp,
    pub elapsed: MonotonicDuration,
    pub outcome: AttemptOutcome,
    pub failure_class: Option<FailureClass>,
    pub sanitized_error_classification: Option<String>,
    pub retry_index: Option<u32>,
    pub clock_anomaly: bool,
}

/// Maps a [`FailureClass`] to this bead's single database spelling, and back.
/// One definition here and nowhere else: a hand copy of these strings between
/// modules is exactly the defect the constants rule exists to prevent.
pub mod failure_class_sql {
    use crate::domain::failure::{FailureClass, HttpStatusClass};
    use crate::error::Error;

    pub fn as_sql(class: &FailureClass) -> &'static str {
        match class {
            FailureClass::DnsFailure | FailureClass::ConnectTimeout | FailureClass::ReadTimeout => {
                "transport_timeout"
            }
            FailureClass::TotalBudgetExpired => "total_budget_expired",
            FailureClass::HttpStatus(status) => match status {
                HttpStatusClass::ClientError => "http_status_client_error",
                HttpStatusClass::ServerError => "http_status_server_error",
            },
            FailureClass::RateLimited { .. } => "rate_limited",
            FailureClass::MalformedBody => "malformed_body",
            FailureClass::MissingRequiredField => "missing_required_field",
        }
    }

    pub fn from_sql(code: &str) -> Result<FailureClass, Error> {
        match code {
            "transport_timeout" => Ok(FailureClass::ReadTimeout),
            "total_budget_expired" => Ok(FailureClass::TotalBudgetExpired),
            "http_status_client_error" => {
                Ok(FailureClass::HttpStatus(HttpStatusClass::ClientError))
            }
            "http_status_server_error" => {
                Ok(FailureClass::HttpStatus(HttpStatusClass::ServerError))
            }
            "rate_limited" => Ok(FailureClass::RateLimited { retry_after: None }),
            "malformed_body" => Ok(FailureClass::MalformedBody),
            "missing_required_field" => Ok(FailureClass::MissingRequiredField),
            other => Err(Error::Store(format!(
                "unknown failure class stored in the database: {other:?}"
            ))),
        }
    }
}

const INSERT_ATTEMPT: &str = "
INSERT INTO meter_attempt (
    run_id, account_id, provider, request_started_at, credential_context_id,
    policy_snapshot_id, due_at, due_reason, due_basis_attempt_id,
    due_basis_result_id, provider_contract_id, meter_semantics_id
) VALUES (
    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
) RETURNING id";

/// Writes the durable attempt start. The write commits here, before any
/// network I/O the caller performs: the caller's ordering is the invariant,
/// and this function returns only once the row is durable.
pub fn start_meter_attempt(
    conn: &rusqlite::Connection,
    attempt: &NewMeterAttempt,
) -> Result<MeterAttemptRowId, Error> {
    let (basis_attempt, basis_result) = match attempt.due_basis {
        None => (None, None),
        Some(DueBasis::Attempt { row_id }) => (Some(row_id.value()), None),
        Some(DueBasis::Result { attempt_id }) => (None, Some(attempt_id.value())),
    };
    conn.query_row(
        INSERT_ATTEMPT,
        params![
            attempt.run_id.value(),
            attempt.account_id.value(),
            attempt.provider,
            attempt.request_started_at.unix_nanos(),
            attempt.credential_context_id,
            attempt.policy_snapshot_id.value(),
            attempt.due_at.unix_nanos(),
            attempt.due_reason.as_sql(),
            basis_attempt,
            basis_result,
            attempt.provider_contract_id,
            attempt.meter_semantics_id,
        ],
        |row| row.get(0),
    )
    .map(MeterAttemptRowId::new)
    .map_err(|e| Error::Store(format!("cannot record the meter attempt start: {e}")))
}

/// Writes the terminal result for one attempt. A second result for the same
/// attempt fails at the database (the table's primary key), which is the
/// authority this bead's criterion names: a duplicate result can never be
/// written, however confident the caller was.
pub fn record_meter_attempt_result(
    conn: &rusqlite::Connection,
    result: &NewMeterAttemptResult,
) -> Result<(), Error> {
    let outcome_sql = match &result.outcome {
        AttemptOutcome::Success => "success",
        AttemptOutcome::AuthRequired => "auth_required",
        AttemptOutcome::Unreachable(_) => "unreachable",
    };
    let (failure_class_sql, retry_after) = match &result.outcome {
        AttemptOutcome::Unreachable(class) => {
            let retry_after = match class {
                FailureClass::RateLimited { retry_after } => {
                    retry_after.map(|d| d.as_nanos() as i64)
                }
                FailureClass::DnsFailure
                | FailureClass::ConnectTimeout
                | FailureClass::ReadTimeout
                | FailureClass::TotalBudgetExpired
                | FailureClass::HttpStatus(_)
                | FailureClass::MalformedBody
                | FailureClass::MissingRequiredField => None,
            };
            (Some(failure_class_sql::as_sql(class)), retry_after)
        }
        AttemptOutcome::Success | AttemptOutcome::AuthRequired => (None, None),
    };
    conn.execute(
        "INSERT INTO meter_attempt_result (
            attempt_id, completed_at, elapsed_nanos, outcome, failure_class,
            retry_after_nanos, sanitized_error_classification, retry_index, clock_anomaly
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            result.attempt_id.value(),
            result.completed_at.unix_nanos(),
            result.elapsed.as_nanos() as i64,
            outcome_sql,
            failure_class_sql,
            retry_after,
            result.sanitized_error_classification,
            result.retry_index,
            result.clock_anomaly as i64,
        ],
    )
    .map_err(|e| {
        let text = e.to_string();
        if text.contains("UNIQUE constraint failed: meter_attempt_result.attempt_id") {
            Error::Store(format!(
                "attempt {} already carries a terminal result; the database refused it: {e}",
                result.attempt_id.value()
            ))
        } else {
            Error::Store(format!("cannot record the meter attempt result: {e}"))
        }
    })?;
    Ok(())
}

fn due_basis_from_sql(
    basis_attempt: Option<i64>,
    basis_result: Option<i64>,
) -> Result<Option<DueBasis>, Error> {
    match (basis_attempt, basis_result) {
        (None, None) => Ok(None),
        (Some(row_id), None) => Ok(Some(DueBasis::Attempt {
            row_id: MeterAttemptRowId::new(row_id),
        })),
        (None, Some(attempt_id)) => Ok(Some(DueBasis::Result {
            attempt_id: MeterAttemptRowId::new(attempt_id),
        })),
        (Some(_), Some(_)) => Err(Error::Store(
            "a meter_attempt row carries both a due-basis attempt and a due-basis result; \
             the table's constraint makes this unrecoverable"
                .into(),
        )),
    }
}

const SELECT_ATTEMPT_COLUMNS: &str = "
    id, run_id, account_id, provider, request_started_at, credential_context_id,
    policy_snapshot_id, due_at, due_reason, due_basis_attempt_id, due_basis_result_id,
    provider_contract_id, meter_semantics_id";

fn row_to_attempt(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMeterAttempt> {
    let due_reason_string: String = row.get("due_reason")?;
    let due_reason = DueReason::from_sql(&due_reason_string).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            8, // due_reason, by position in SELECT_ATTEMPT_COLUMNS
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    let basis = due_basis_from_sql(
        row.get("due_basis_attempt_id")?,
        row.get("due_basis_result_id")?,
    )
    .map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            9, // due_basis_attempt_id, by position in SELECT_ATTEMPT_COLUMNS
            rusqlite::types::Type::Text,
            Box::new(e),
        )
    })?;
    Ok(StoredMeterAttempt {
        row_id: MeterAttemptRowId::new(row.get("id")?),
        run_id: crate::store::sample_run::SampleRunId::new(row.get("run_id")?),
        account_id: crate::store::account::AccountId::new(row.get("account_id")?),
        provider: row.get("provider")?,
        request_started_at: UtcTimestamp::from_unix_nanos(row.get("request_started_at")?),
        credential_context_id: row.get("credential_context_id")?,
        policy_snapshot_id: crate::store::sampling_policy_snapshot::SamplingPolicySnapshotId::new(
            row.get("policy_snapshot_id")?,
        ),
        due_at: UtcTimestamp::from_unix_nanos(row.get("due_at")?),
        due_reason,
        due_basis: basis,
        provider_contract_id: row.get("provider_contract_id")?,
        meter_semantics_id: row.get("meter_semantics_id")?,
    })
}

/// Reads one attempt start by its rowid, or `None` when there is no such row.
pub fn attempt_by_row_id(
    conn: &rusqlite::Connection,
    row_id: MeterAttemptRowId,
) -> Result<Option<StoredMeterAttempt>, Error> {
    conn.query_row(
        &format!("{SELECT_ATTEMPT_COLUMNS} FROM meter_attempt WHERE id = ?1"),
        params![row_id.value()],
        row_to_attempt,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read meter_attempt {}: {e}", row_id.value())))
}

fn failure_class_pair(
    outcome_sql: &str,
    failure_class_sql: Option<String>,
    retry_after_nanos: Option<i64>,
) -> Result<(AttemptOutcome, Option<FailureClass>), Error> {
    match outcome_sql {
        "success" => Ok((AttemptOutcome::Success, None)),
        "auth_required" => Ok((AttemptOutcome::AuthRequired, None)),
        "unreachable" => {
            let Some(code) = failure_class_sql.as_deref() else {
                return Err(Error::Store(
                    "a meter_attempt_result row is unreachable without a failure class".to_string(),
                ));
            };
            let class = match (failure_class_sql::from_sql(code)?, retry_after_nanos) {
                (FailureClass::RateLimited { .. }, Some(nanos)) => FailureClass::RateLimited {
                    retry_after: Some(MonotonicDuration::from_nanos(nanos as u64)),
                },
                (class, _) => class,
            };
            Ok((AttemptOutcome::Unreachable(class), Some(class)))
        }
        other => Err(Error::Store(format!(
            "unknown meter_attempt_result outcome stored in the database: {other:?}"
        ))),
    }
}

const SELECT_RESULT_COLUMNS: &str = "
    attempt_id, completed_at, elapsed_nanos, outcome, failure_class,
    retry_after_nanos, sanitized_error_classification, retry_index, clock_anomaly";

fn row_to_result(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMeterAttemptResult> {
    let outcome_sql: String = row.get("outcome")?;
    let failure_class_sql: Option<String> = row.get("failure_class")?;
    let retry_after_nanos: Option<i64> = row.get("retry_after_nanos")?;
    let (outcome, failure_class) =
        failure_class_pair(&outcome_sql, failure_class_sql, retry_after_nanos).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                3, // outcome, by position in SELECT_RESULT_COLUMNS
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
    Ok(StoredMeterAttemptResult {
        attempt_id: MeterAttemptRowId::new(row.get("attempt_id")?),
        completed_at: UtcTimestamp::from_unix_nanos(row.get("completed_at")?),
        elapsed: MonotonicDuration::from_nanos(row.get::<_, i64>("elapsed_nanos")? as u64),
        outcome,
        failure_class,
        sanitized_error_classification: row.get("sanitized_error_classification")?,
        retry_index: row
            .get::<_, Option<i64>>("retry_index")?
            .map(|v| u32::try_from(v).expect("the table CHECK keeps retry_index non-negative")),
        clock_anomaly: row.get::<_, i64>("clock_anomaly")? == 1,
    })
}

/// Reads one attempt's terminal result, or `None` while the attempt is still
/// open, which is the started-with-no-result state this schema exists to keep
/// representable.
pub fn result_by_attempt_id(
    conn: &rusqlite::Connection,
    attempt_id: MeterAttemptRowId,
) -> Result<Option<StoredMeterAttemptResult>, Error> {
    conn.query_row(
        &format!("SELECT {SELECT_RESULT_COLUMNS} FROM meter_attempt_result WHERE attempt_id = ?1"),
        params![attempt_id.value()],
        row_to_result,
    )
    .optional()
    .map_err(|e| {
        Error::Store(format!(
            "cannot read the result of meter attempt {}: {e}",
            attempt_id.value()
        ))
    })
}

/// Every started attempt that still carries no terminal result: the
/// collector-interruption candidates, oldest first.
pub fn open_attempt_row_ids(conn: &rusqlite::Connection) -> Result<Vec<MeterAttemptRowId>, Error> {
    let mut statement = conn
        .prepare(
            "SELECT ma.id FROM meter_attempt ma WHERE NOT EXISTS (
                SELECT 1 FROM meter_attempt_result mar WHERE mar.attempt_id = ma.id
            ) ORDER BY ma.request_started_at",
        )
        .map_err(|e| Error::Store(format!("cannot list open meter attempts: {e}")))?;
    let rows = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| Error::Store(format!("cannot list open meter attempts: {e}")))?;
    rows.map(|entry| entry.map(MeterAttemptRowId::new))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read open meter attempts: {e}")))
}

/// The number of started attempts and the number of terminal results in the
/// database. The read-back surface for the crash-injection hook (`aub-sth.6`):
/// the two counts together state exactly what survived a kill between the two
/// commits.
pub fn count_attempts(conn: &rusqlite::Connection) -> Result<(u64, u64), Error> {
    let starts: u64 = conn
        .query_row("SELECT count(*) FROM meter_attempt", [], |row| row.get(0))
        .map_err(|e| Error::Store(format!("cannot count meter attempts: {e}")))?;
    let results: u64 = conn
        .query_row("SELECT count(*) FROM meter_attempt_result", [], |row| {
            row.get(0)
        })
        .map_err(|e| Error::Store(format!("cannot count meter attempt results: {e}")))?;
    Ok((starts, results))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::account::{AccountId, observe_account};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use crate::store::sample_run::{SampleRunId, Trigger, start_sample_run};
    use crate::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
    };
    use proptest::prelude::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-meter-attempt-test-{}-{suffix}",
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

    const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: String::new(),
        retry_backoff_policy: String::new(),
        command_budget: MonotonicDuration::from_millis(60_000),
        policy_algorithm_version: String::new(),
    };

    /// A connection migrated through the full registry, holding one account,
    /// one sample run and one policy snapshot the attempts can reference.
    fn fixture() -> (
        ScratchDir,
        rusqlite::Connection,
        SampleRunId,
        AccountId,
        SamplingPolicySnapshotId,
    ) {
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
        run_migrations(&mut conn, &registry(), None, &clock_at(9_000))
            .expect("fixture migrations must apply");
        let account = observe_account(
            &conn,
            "test-provider",
            "test-account",
            UtcTimestamp::from_unix_nanos(10_000),
        )
        .expect("fixture account must insert");
        let run = start_sample_run(
            &conn,
            Trigger::Manual,
            UtcTimestamp::from_unix_nanos(10_000),
            "test",
        )
        .expect("fixture sample run must insert");
        let snapshot = resolve_policy_snapshot(
            &conn,
            account,
            UtcTimestamp::from_unix_nanos(10_000),
            &POLICY,
        )
        .expect("fixture policy snapshot must insert");
        (scratch, conn, run, account, snapshot)
    }

    fn attempt_start(
        run: SampleRunId,
        account: AccountId,
        snapshot: SamplingPolicySnapshotId,
    ) -> NewMeterAttempt {
        NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "test-provider".into(),
            request_started_at: UtcTimestamp::from_unix_nanos(20_000),
            credential_context_id: Some("ctx-1".into()),
            policy_snapshot_id: snapshot,
            due_at: UtcTimestamp::from_unix_nanos(19_000),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-schema-v3".into(),
            meter_semantics_id: "account-5h-v2".into(),
        }
    }

    /// The planted negative for the single-terminal-result invariant: the
    /// duplicate is refused by the database, not by this module's own code.
    #[test]
    fn a_second_result_for_one_attempt_fails_at_the_database() {
        let (_scratch, conn, run, account, snapshot) = fixture();
        let row_id = start_meter_attempt(&conn, &attempt_start(run, account, snapshot))
            .expect("first attempt must insert");
        let first = NewMeterAttemptResult {
            attempt_id: row_id,
            completed_at: UtcTimestamp::from_unix_nanos(30_000),
            elapsed: MonotonicDuration::from_millis(10_000),
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        };
        record_meter_attempt_result(&conn, &first).expect("the first result must insert");
        let second = NewMeterAttemptResult {
            completed_at: UtcTimestamp::from_unix_nanos(40_000),
            ..first
        };
        let err = record_meter_attempt_result(&conn, &second)
            .expect_err("the database must refuse a second result");
        assert!(
            err.to_string()
                .contains("already carries a terminal result"),
            "the refusal must name the invariant: {err}"
        );
    }

    #[test]
    fn triggers_refuse_every_update_and_delete_on_both_tables() {
        let (_scratch, conn, run, account, snapshot) = fixture();
        let row_id = start_meter_attempt(&conn, &attempt_start(run, account, snapshot))
            .expect("the attempt must insert");
        record_meter_attempt_result(
            &conn,
            &NewMeterAttemptResult {
                attempt_id: row_id,
                completed_at: UtcTimestamp::from_unix_nanos(30_000),
                elapsed: MonotonicDuration::from_millis(10_000),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
        )
        .expect("the result must insert before the trigger assertions");
        for sql in [
            "UPDATE meter_attempt SET provider = 'rewritten' WHERE id = 1",
            "DELETE FROM meter_attempt WHERE id = 1",
            "UPDATE meter_attempt_result SET completed_at = 1 WHERE attempt_id = 1",
            "DELETE FROM meter_attempt_result WHERE attempt_id = 1",
        ] {
            let err = conn
                .execute(sql, [])
                .err()
                .unwrap_or_else(|| panic!("direct statement must be refused: {sql}"));
            assert!(
                err.to_string().contains("irreplaceable evidence"),
                "the trigger must name the reason: {err}"
            );
        }
    }

    #[test]
    fn the_clock_anomaly_marker_is_the_only_way_a_result_precedes_its_start() {
        let (_scratch, conn, run, account, snapshot) = fixture();
        let row_id = start_meter_attempt(&conn, &attempt_start(run, account, snapshot))
            .expect("the attempt must insert");
        let anomaly_result = NewMeterAttemptResult {
            attempt_id: row_id,
            completed_at: UtcTimestamp::from_unix_nanos(15_000),
            elapsed: MonotonicDuration::from_millis(5),
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: true,
        };
        record_meter_attempt_result(&conn, &anomaly_result)
            .expect("an explicitly marked anomaly is exactly the allowed case");
        let stored = result_by_attempt_id(&conn, row_id)
            .expect("the result must read back")
            .expect("the result must exist");
        assert!(stored.clock_anomaly);

        let (scratch2, conn2, run2, account2, snapshot2) = fixture();
        let _ = scratch2;
        let row_id2 = start_meter_attempt(&conn2, &attempt_start(run2, account2, snapshot2))
            .expect("the attempt must insert");
        let before_start = NewMeterAttemptResult {
            attempt_id: row_id2,
            completed_at: UtcTimestamp::from_unix_nanos(15_000),
            elapsed: MonotonicDuration::from_millis(5_000),
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        };
        let err = record_meter_attempt_result(&conn2, &before_start)
            .expect_err("an unmarked result before its start must violate the constraint");
        assert!(
            err.to_string()
                .contains("cannot record the meter attempt result"),
            "{err}"
        );
    }

    #[test]
    fn reads_report_started_attempts_with_no_result_as_open() {
        let (_scratch, conn, run, account, snapshot) = fixture();
        let first = start_meter_attempt(&conn, &attempt_start(run, account, snapshot))
            .expect("the first attempt must insert");
        let _second = start_meter_attempt(
            &conn,
            &NewMeterAttempt {
                due_reason: DueReason::ResetEdge,
                due_basis: Some(DueBasis::Attempt { row_id: first }),
                ..attempt_start(run, account, snapshot)
            },
        )
        .expect("the second attempt must insert");
        assert_eq!(
            open_attempt_row_ids(&conn).expect("open attempts must read"),
            [first, _second]
        );
        record_meter_attempt_result(
            &conn,
            &NewMeterAttemptResult {
                attempt_id: first,
                completed_at: UtcTimestamp::from_unix_nanos(30_000),
                elapsed: MonotonicDuration::from_millis(10_000),
                outcome: AttemptOutcome::Unreachable(FailureClass::RateLimited {
                    retry_after: Some(MonotonicDuration::from_millis(60_000)),
                }),
                sanitized_error_classification: Some(
                    "request failed: [REDACTED] status=429".into(),
                ),
                retry_index: Some(1),
                clock_anomaly: false,
            },
        )
        .expect("the rate-limited result must insert");
        assert_eq!(
            open_attempt_row_ids(&conn).expect("open attempts must read"),
            [_second]
        );
        let stored = result_by_attempt_id(&conn, first).unwrap().unwrap();
        assert_eq!(
            stored.outcome,
            AttemptOutcome::Unreachable(FailureClass::RateLimited {
                retry_after: Some(MonotonicDuration::from_millis(60_000)),
            })
        );
        assert_eq!(stored.retry_index, Some(1));
    }

    // Over generated attempt sequences, whatever the interleaving, the number
    // of terminal results never exceeds the number of started attempts: a
    // result can only land on an already-started attempt, and a second result
    // for a started attempt is refused by the database.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn result_rows_never_exceed_start_rows(
            events in proptest::collection::vec(proptest::bool::ANY, 1..18),
        ) {
            let (_scratch, conn, run, account, snapshot) = fixture();
            let mut started: u64 = 0;
            let mut ended: u64 = 0;
            for is_result in events {
                if !is_result {
                    let row_id = start_meter_attempt(&conn, &attempt_start(run, account, snapshot))
                        .expect("each generated start must insert");
                    started += 1;
                    let _ = row_id;
                } else {
                    let open = open_attempt_row_ids(&conn).expect("open attempts must read");
                    let Some(target) = open.first().copied() else {
                        continue;
                    };
                    let result = NewMeterAttemptResult {
                        attempt_id: target,
                        completed_at: UtcTimestamp::from_unix_nanos(60_000),
                        elapsed: MonotonicDuration::from_millis(5_000),
                        outcome: AttemptOutcome::Success,
                        sanitized_error_classification: None,
                        retry_index: None,
                        clock_anomaly: false,
                    };
                    if result_by_attempt_id(&conn, target)
                        .expect("result reads must not fail")
                        .is_none()
                    {
                        record_meter_attempt_result(&conn, &result)
                            .expect("one result per open attempt must insert");
                        ended += 1;
                    }
                }
                let total_results: u64 = conn
                    .query_row("SELECT count(*) FROM meter_attempt_result", [], |row| {
                        row.get(0)
                    })
                    .expect("the count must read");
                prop_assert!(
                    total_results <= started,
                    "results ({}) exceeded starts ({})", total_results, started
                );
                prop_assert_eq!(total_results, ended);
                let _ = ended;
            }
        }
    }
}
