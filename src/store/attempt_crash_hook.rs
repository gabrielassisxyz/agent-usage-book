//! The crash-injection surface for the two-stage meter attempt lifecycle
//! (`aub-sth.6`, PLAN.md section 34.7), test-only.
//!
//! `__attempt-crash-hook start` commits the attempt start through the real store
//! APIs and then aborts the process, so a test can prove exactly what survives a
//! kill between the two commits: the start with no result. `complete` is the
//! adjacent positive control, running start then result then a clean exit, and
//! `read-back` reports what the database actually holds.
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
use crate::domain::time::{Clock, MonotonicDuration};
use crate::error::Error;
use crate::store::account::observe_account;
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::meter_attempt::{
    DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult, count_attempts,
    record_meter_attempt_result, start_meter_attempt,
};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::sample_run::{Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};

/// The stages the `__attempt-crash-hook` command drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookStage {
    /// Commit the attempt start, then abort the process at the injection point.
    Start,
    /// Run both stages and exit cleanly: the adjacent positive control.
    Complete,
    /// Count what the database holds, without writing anything.
    ReadBack,
}

/// What one stage produced, for the CLI shim to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookOutcome {
    /// The complete stage wrote both facts.
    Completed { attempt_row_id: MeterAttemptRowId },
    /// The read-back stage counted the database.
    Counts { starts: u64, results: u64 },
}

/// Runs one crash-hook stage against the database at `db_path`.
///
/// The `Start` stage never returns: after the attempt start is durable it aborts
/// the process, which is the documented injection point. `ReadBack` opens the
/// database read-only and returns the counts; `Complete` writes both facts and
/// returns the attempt row.
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
        return Ok(CrashHookOutcome::Counts { starts, results });
    }

    run_migrations(&mut conn, &registry(), None, clock)?;

    // Fixture facts the attempt row references, written through the real insert
    // APIs so the lifecycle under test is the real one.
    let account = observe_account(&conn, "fixture-provider", "fixture-account", clock.now())?;
    let run = start_sample_run(&conn, Trigger::Manual, clock.now(), "__attempt-crash-hook")?;
    let snapshot_policy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: "fixture".to_string(),
        retry_backoff_policy: "fixture".to_string(),
        command_budget,
        policy_algorithm_version: "fixture-1".to_string(),
    };
    let snapshot = resolve_policy_snapshot(&conn, account, clock.now(), &snapshot_policy)?;

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
    record_meter_attempt_result(&conn, &result)?;
    Ok(CrashHookOutcome::Completed {
        attempt_row_id: row_id,
    })
}
