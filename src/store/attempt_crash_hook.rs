//! The crash-injection surface for the two-stage meter attempt lifecycle
//! (`aub-sth.6`, PLAN.md section 34.7), test-only.
//!
//! `__attempt-crash-hook start` commits the attempt start through the real store
//! APIs and then aborts the process, so a test can prove exactly what survives a
//! kill between the two commits: the start with no result. `complete` is the
//! adjacent positive control, running start then result then a clean exit, and
//! `read-back` reports what the database actually holds.
//!
//! The `sample` stages (`aub-lqe.18`) run the meter evidence cycle PLAN.md
//! section 13 specifies, against the LEDGER database itself so they contend
//! with a concurrent ingest the way the two real workloads do: fixture attempt
//! start, durable spool of the terminal bundle, commit through the repository
//! boundary, pending-file deletion on success. `sample` runs the cycle N times
//! and reports committed-or-spooled per attempt; `sample-crash` is the
//! documented injection point between the spool and the commit, aborting the
//! process with the record already durable, which is the state a kill mid-cycle
//! leaves. Both drain the spool first, because they are mutating workflows.
//!
//! The hook lives in the store layer rather than in the CLI because it is
//! store-lifecycle test surface: it runs migrations, writes fixture rows and
//! counts rows, all of which the boundary rules confine to `src/store/` (rules
//! 15 and 16). The CLI shim only parses the stage, resolves configuration and
//! renders the outcome.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation
//! - configuration (the caller passes the resolved values in)

use std::path::Path;

use crate::domain::attempt::AttemptOutcome;
use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{Clock, MonotonicDuration, MeasurementBasis, UtcTimestamp};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::store::account::{AccountId, observe_account};
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::meter_attempt::{
    DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult, count_attempts,
    record_meter_attempt_result, start_meter_attempt,
};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::repository::{NewMeterInterpretation, Repository, TerminalMeterBundle};
use crate::store::sample_run::{SampleRunId, Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
};
use crate::store::spool::{self, PendingTerminalBundle};

/// The stages the `__attempt-crash-hook` command drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookStage {
    /// Commit the attempt start, then abort the process at the injection point.
    Start,
    /// Run both stages and exit cleanly: the adjacent positive control.
    Complete,
    /// Count what the database holds, without writing anything.
    ReadBack,
    /// The meter evidence cycle of PLAN.md section 13, run `attempts` times
    /// against the ledger database: spool the terminal bundle, commit it, delete
    /// the pending record on success. An attempt whose commit cannot take the
    /// writer slot leaves its record durably spooled; the next run drains first.
    Sample { attempts: u32 },
    /// The documented injection point between the spool and the commit: one
    /// fixture attempt is spooled durably, then the process aborts before any
    /// commit. The state a kill mid-cycle leaves is exactly this state.
    SampleCrash,
}

/// What one sample attempt did, for the CLI to render and correlate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleAttemptOutcome {
    pub attempt_id: MeterAttemptRowId,
    /// True when the bundle committed and the pending record was deleted;
    /// false when the record remains durably spooled awaiting a drain.
    pub committed: bool,
    /// How long the commit call waited before it ran or gave up: under
    /// contention this is dominated by the wait for the writer slot.
    pub commit_wait: MonotonicDuration,
}

/// What one stage produced, for the CLI shim to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashHookOutcome {
    /// The complete stage wrote both facts.
    Completed { attempt_row_id: MeterAttemptRowId },
    /// The read-back stage counted the database.
    Counts {
        starts: u64,
        results: u64,
        evidence: u64,
        observations: u64,
    },
    /// The sample stage: the drain that ran first, then one outcome per
    /// attempt in landing order.
    Sampled {
        drain_applied: usize,
        drain_already_applied: usize,
        drain_quarantined: usize,
        attempts: Vec<SampleAttemptOutcome>,
    },
}

/// Runs one crash-hook stage against the database at `db_path`.
///
/// The `Start` and `SampleCrash` stages never return: each aborts the process at
/// its documented injection point. `ReadBack` opens the database read-only and
/// returns the counts; `Complete` writes both facts and returns the attempt row;
/// the sample stages drain first, then run the PLAN.md section 13 cycle against
/// `db_path` (the ledger database the caller passes for them), and return one
/// outcome per attempt.
pub fn run_stage(
    db_path: &Path,
    stage: CrashHookStage,
    busy_timeout: MonotonicDuration,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<CrashHookOutcome, Error> {
    let read_only = stage == CrashHookStage::ReadBack;
    let mut conn = open(
        db_path,
        if read_only {
            AccessMode::ReadOnly
        } else {
            AccessMode::ReadWrite
        },
        &PragmaPolicy { busy_timeout },
    )?;
    if read_only {
        let (starts, results) = count_attempts(&conn)?;
        let evidence: u64 = conn
            .query_row(
                "SELECT count(*) FROM meter_response_evidence",
                [],
                |row| row.get(0),
            )
            .map_err(|e| Error::Store(format!("cannot count response evidence: {e}")))?;
        let observations: u64 = conn
            .query_row("SELECT count(*) FROM meter_observation", [], |row| row.get(0))
            .map_err(|e| Error::Store(format!("cannot count observations: {e}")))?;
        return Ok(CrashHookOutcome::Counts {
            starts,
            results,
            evidence,
            observations,
        });
    }

    run_migrations(&mut conn, &registry(), None, clock)?;
    let state_dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    match stage {
        CrashHookStage::Sample { attempts } => {
            // A mutating workflow drains the pending spool before its own work
            // (PLAN.md section 13), so evidence left by a crash or a busy
            // timeout enters the ledger before new attempts are made.
            let drain = crate::store::spool::drain_pending(&mut conn, &state_dir)?;
            let repository = Repository::new(db_path, PragmaPolicy { busy_timeout });
            let mut landed = Vec::new();
            for _ in 0..attempts {
                landed.push(sample_once(&conn, &repository, command_budget, clock)?);
            }
            // The sample's fixture writes and commits change projection-
            // relevant state; one publication at the end leaves the file
            // describing the state the database actually holds.
            let _ = crate::projection::publish(
                &conn,
                &crate::projection::projection_path_in(&state_dir),
            );
            Ok(CrashHookOutcome::Sampled {
                drain_applied: drain.applied,
                drain_already_applied: drain.already_applied,
                drain_quarantined: drain.quarantined,
                attempts: landed,
            })
        }
        CrashHookStage::SampleCrash => {
            let (account, run, snapshot) = seed_fixture(&conn, command_budget, clock)?;
            let started_at = clock.now();
            let attempt = NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "fixture-provider".to_string(),
                request_started_at: started_at,
                credential_context_id: Some("fixture-credential-context".to_string()),
                policy_snapshot_id: snapshot,
                due_at: started_at,
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "fixture-endpoint-schema".to_string(),
                meter_semantics_id: "fixture-meter-semantics".to_string(),
            };
            let row_id = start_meter_attempt(&conn, &attempt)?;
            let bundle = sample_terminal_bundle(row_id, account, clock.now());
            // The injection point: the record is durably spooled (PLAN.md
            // section 13 step 5 complete), the commit never happens, and the
            // process ends here by signal.
            spool::spool_pending(
                &state_dir,
                &PendingTerminalBundle::from_bundle(&bundle),
            )?;
            std::process::abort();
        }
        CrashHookStage::Start | CrashHookStage::Complete => {
            start_then_maybe_abort(&mut conn, stage, command_budget, clock)
        }
        CrashHookStage::ReadBack => unreachable!("read-only handled above"),
    }
}

/// The fixture facts every attempt row references, written through the real
/// insert APIs so the lifecycle under test is the real one.
fn seed_fixture(
    conn: &rusqlite::Connection,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<(AccountId, SampleRunId, SamplingPolicySnapshotId), Error> {
    let account = observe_account(conn, "fixture-provider", "fixture-account", clock.now())?;
    let run = start_sample_run(conn, Trigger::Manual, clock.now(), "__attempt-crash-hook")?;
    let snapshot_policy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: "fixture".to_string(),
        retry_backoff_policy: "fixture".to_string(),
        command_budget,
        policy_algorithm_version: "fixture-1".to_string(),
    };
    let snapshot = resolve_policy_snapshot(conn, account, clock.now(), &snapshot_policy)?;
    Ok((account, run, snapshot))
}

/// One full meter evidence cycle (PLAN.md section 13, steps 2 and 5 to 7):
/// fixture attempt start, terminal bundle, spool, commit through the
/// repository boundary, pending-file deletion on success. Called from a stage
/// that has already drained, so no prior pending record is superseded here.
fn sample_once(
    conn: &rusqlite::Connection,
    repository: &Repository,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<SampleAttemptOutcome, Error> {
    let (account, run, snapshot) = seed_fixture(conn, command_budget, clock)?;
    let started_at = clock.now();
    let attempt = NewMeterAttempt {
        run_id: run,
        account_id: account,
        provider: "fixture-provider".to_string(),
        request_started_at: started_at,
        credential_context_id: Some("fixture-credential-context".to_string()),
        policy_snapshot_id: snapshot,
        due_at: started_at,
        due_reason: DueReason::OrdinaryCadence,
        due_basis: None,
        provider_contract_id: "fixture-endpoint-schema".to_string(),
        meter_semantics_id: "fixture-meter-semantics".to_string(),
    };
    let row_id = start_meter_attempt(conn, &attempt)?;
    let bundle = sample_terminal_bundle(row_id, account, clock.now());
    match spool::spool_then_commit(repository, &bundle, clock)? {
        spool::SpoolCycleOutcome::Committed { commit_wait, .. } => Ok(SampleAttemptOutcome {
            attempt_id: row_id,
            committed: true,
            commit_wait,
        }),
        spool::SpoolCycleOutcome::LeftPending { commit_wait, .. } => Ok(SampleAttemptOutcome {
            attempt_id: row_id,
            committed: false,
            commit_wait,
        }),
    }
}

/// One complete fixture terminal bundle: the same shape the projection and
/// backup proofs commit, with a single account-wide window.
fn sample_terminal_bundle(
    attempt_id: MeterAttemptRowId,
    account_id: AccountId,
    completed_at: UtcTimestamp,
) -> TerminalMeterBundle {
    let window = MeterWindow::new(
        WindowSemanticKey::new("five_hour"),
        WindowScope::AccountWide,
        QuotaUsed::new(QuotaFractionPpm::new(250_000).expect("fixture ppm in range")),
        ReportedResolution::new(QuotaFractionPpm::new(10_000).expect("fixture ppm in range"))
            .expect("fixture resolution non-zero"),
        QuantizationSemantics::RoundedToNearest,
        completed_at,
        NominalWindowDuration::from_nanos(18_000_000_000_000),
    );
    TerminalMeterBundle::new(
        NewMeterAttemptResult {
            attempt_id,
            completed_at,
            elapsed: MonotonicDuration::from_nanos(1),
            outcome: AttemptOutcome::Success,
            sanitized_error_classification: None,
            retry_index: None,
            clock_anomaly: false,
        },
        crate::store::meter_evidence::NewMeterResponseEvidence {
            attempt_id,
            response_classification: "success".to_string(),
            received_at: completed_at,
            provider_observed_at_original: Some("1970-01-01T00:00:00Z".to_string()),
            evidence_capsule: "{\"five_hour\":\"25.0\"}".to_string(),
            capsule_schema_version: "capsule-v1".to_string(),
            sanitizer_version: "sanitizer-v1".to_string(),
            capture_truncated: false,
        },
        NewMeterInterpretation {
            account_id,
            provider: "fixture-provider".to_string(),
            provider_observed_at: Some(completed_at),
            received_at: completed_at,
            measurement_basis: MeasurementBasis::ProviderObserved,
            observed_plan: Some("fixture-plan".to_string()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("fixture-adapter-v1"),
            provider_contract_id: ProviderContractId::new("fixture-endpoint-schema"),
            meter_semantics_id: MeterSemanticsId::new("fixture-meter-semantics"),
            normalized_fingerprint: "fixture-fingerprint-v1".to_string(),
        },
        vec![window],
    )
    .expect("fixture bundle fields agree")
}

/// The two-stage lifecycle's original stages: start, then (for the control)
/// result. The `Start` stage aborts at its injection point and never returns.
fn start_then_maybe_abort(
    conn: &mut rusqlite::Connection,
    stage: CrashHookStage,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<CrashHookOutcome, Error> {
    let (account, run, snapshot) = seed_fixture(conn, command_budget, clock)?;

    let started_at = clock.now();
    let attempt = NewMeterAttempt {
        run_id: run,
        account_id: account,
        provider: "fixture-provider".to_string(),
        request_started_at: started_at,
        credential_context_id: Some("fixture-credential-context".to_string()),
        policy_snapshot_id: snapshot,
        due_at: started_at,
        due_reason: DueReason::OrdinaryCadence,
        due_basis: None,
        provider_contract_id: "fixture-endpoint-schema".to_string(),
        meter_semantics_id: "fixture-meter-semantics".to_string(),
    };
    let row_id = start_meter_attempt(conn, &attempt)?;

    if stage == CrashHookStage::Start {
        // The crash-injection point: the start is durable, the result does not
        // exist, and the process ends here by signal.
        std::process::abort();
    }

    let mono_start = clock.monotonic_now();
    let result = NewMeterAttemptResult {
        attempt_id: row_id,
        completed_at: clock.now(),
        elapsed: clock
            .monotonic_now()
            .duration_since(mono_start)
            .max(MonotonicDuration::from_nanos(1)),
        outcome: AttemptOutcome::Success,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
    };
    record_meter_attempt_result(conn, &result)?;
    Ok(CrashHookOutcome::Completed {
        attempt_row_id: row_id,
    })
}
