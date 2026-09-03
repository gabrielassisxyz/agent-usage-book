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
use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{Clock, MeasurementBasis, MonotonicDuration, UtcTimestamp};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::store::account::observe_account;
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::meter_attempt::{
    DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult, count_attempts,
    record_meter_attempt_result, start_meter_attempt,
};
use crate::store::meter_evidence::{NewMeterResponseEvidence, measurement_basis_sql};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::repository::{
    NewMeterInterpretation, TerminalMeterBundle, commit_terminal_bundle_on_connection,
};
use crate::store::sample_run::{Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
use crate::store::spool::{PendingTerminalBundle, PendingWindow, spool_pending};

/// The stages the `__attempt-crash-hook` command drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookStage {
    /// Commit the attempt start, then abort the process at the injection point.
    Start,
    /// Run both stages and exit cleanly: the adjacent positive control.
    Complete,
    /// Count what the database holds, without writing anything.
    ReadBack,
    /// Commit one complete terminal bundle (start, result, evidence,
    /// observation, windows) for a fresh attempt through the real repository
    /// boundary: the committed observation a restore drill counts. Runs
    /// against the ledger database, not the crash-hook fixture database.
    CommitObservation,
    /// Start an attempt, then spool its terminal record without committing
    /// it: the pending evidence a recovery replay takes back. Runs against
    /// the ledger database.
    SpoolPending,
    /// Spool a terminal record for an attempt id that has no start row in
    /// this ledger: the record the ledger must refuse, which a recovery
    /// quarantines and reports rather than replays. Runs against the ledger
    /// database.
    SpoolOrphan { attempt_id: i64 },
}

/// What one stage produced, for the CLI shim to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookOutcome {
    /// The complete stage wrote both facts.
    Completed { attempt_row_id: MeterAttemptRowId },
    /// The read-back stage counted the database.
    Counts { starts: u64, results: u64 },
    /// A seeding stage wrote what it was asked to, named by `label` so the
    /// shim's one print per outcome stays honest about what it did.
    Seeded {
        label: &'static str,
        attempt_id: i64,
    },
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

    if stage == CrashHookStage::CommitObservation {
        // The result rides inside the terminal bundle: the repository boundary
        // records it in the same transaction as the evidence, the observation
        // and the windows, which is what makes the seeded row the same shape
        // a live sample produces rather than a hand-written half of one.
        let bundle = terminal_bundle_for(row_id, account)?;
        commit_terminal_bundle_on_connection(&mut conn, &bundle, || Ok(()))?;
        return Ok(CrashHookOutcome::Seeded {
            label: "committed",
            attempt_id: row_id.value(),
        });
    }

    if stage == CrashHookStage::SpoolPending {
        let state_dir = db_path.parent().ok_or_else(|| {
            Error::Store(format!("database path {db_path:?} has no parent directory"))
        })?;
        spool_pending(
            state_dir,
            &pending_bundle_for(row_id.value(), account.value()),
        )?;
        return Ok(CrashHookOutcome::Seeded {
            label: "spooled",
            attempt_id: row_id.value(),
        });
    }

    if let CrashHookStage::SpoolOrphan { attempt_id } = stage {
        // Fail loud when the id is not actually an orphan: a stage that
        // quietly seeded a replayable record would make a drill count the
        // wrong thing without ever looking wrong.
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM meter_attempt WHERE id = ?1)",
                rusqlite::params![attempt_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                Error::Store(format!("cannot check for attempt {attempt_id}: {error}"))
            })?;
        if exists {
            return Err(Error::Usage(format!(
                "attempt {attempt_id} already has a start row in this ledger; an orphan record \n                 must name an id no attempt row holds"
            )));
        }
        let state_dir = db_path.parent().ok_or_else(|| {
            Error::Store(format!("database path {db_path:?} has no parent directory"))
        })?;
        spool_pending(state_dir, &pending_bundle_for(attempt_id, account.value()))?;
        return Ok(CrashHookOutcome::Seeded {
            label: "spooled-orphan",
            attempt_id,
        });
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

/// One complete terminal bundle for a seeded attempt: sanitized evidence, one
/// interpretation, one observation, one window. The values are fixtures, but
/// the write path is the production one, which is the whole point of seeding
/// through this hook rather than through raw SQL in a script.
fn terminal_bundle_for(
    row_id: MeterAttemptRowId,
    account: crate::store::account::AccountId,
) -> Result<TerminalMeterBundle, Error> {
    let received_at = UtcTimestamp::from_unix_nanos(1_000);
    let result = NewMeterAttemptResult {
        attempt_id: row_id,
        completed_at: UtcTimestamp::from_unix_nanos(2_000),
        elapsed: MonotonicDuration::from_nanos(1_000),
        outcome: AttemptOutcome::Success,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
    };
    let evidence = NewMeterResponseEvidence {
        attempt_id: row_id,
        response_classification: "success".to_string(),
        received_at,
        provider_observed_at_original: Some("2026-09-02T00:00:00Z".to_string()),
        evidence_capsule: "{\"sanitized\":true}".to_string(),
        capsule_schema_version: "v1".to_string(),
        sanitizer_version: "v1".to_string(),
        capture_truncated: false,
    };
    let interpretation = NewMeterInterpretation {
        account_id: account,
        provider: "fixture-provider".to_string(),
        provider_observed_at: Some(UtcTimestamp::from_unix_nanos(900)),
        received_at,
        measurement_basis: MeasurementBasis::ProviderObserved,
        observed_plan: Some("fixture-plan".to_string()),
        observed_tier: None,
        adapter_version: AdapterVersion::new("fixture-adapter".to_string()),
        provider_contract_id: ProviderContractId::new("fixture-contract".to_string()),
        meter_semantics_id: MeterSemanticsId::new("fixture-semantics".to_string()),
        normalized_fingerprint: format!("fixture-fp-{}", row_id.value()),
    };
    let window = MeterWindow::new(
        WindowSemanticKey::new("five_hour".to_string()),
        WindowScope::AccountWide,
        QuotaUsed::new(QuotaFractionPpm::new(250_000).ok_or_else(|| {
            Error::Internal("fixture window quota_used_ppm is out of range".into())
        })?),
        ReportedResolution::new(QuotaFractionPpm::new(10_000).ok_or_else(|| {
            Error::Internal("fixture window reported_resolution_ppm is out of range".into())
        })?)
        .ok_or_else(|| {
            Error::Internal("fixture window reported_resolution_ppm must be non-zero".into())
        })?,
        QuantizationSemantics::Exact,
        UtcTimestamp::from_unix_nanos(5_000),
        NominalWindowDuration::from_nanos(18_000_000_000_000),
    );
    TerminalMeterBundle::new(result, evidence, interpretation, vec![window])
}

/// The same fixture bundle as [`terminal_bundle_for`], flattened into the
/// spool's durable on-disk shape, for the two spooling stages.
fn pending_bundle_for(attempt_id: i64, account_id: i64) -> PendingTerminalBundle {
    PendingTerminalBundle {
        attempt_id,
        completed_at_nanos: 2_000,
        elapsed_nanos: 1_000,
        outcome: "success".into(),
        failure_class: None,
        retry_after_nanos: None,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
        response_classification: "success".into(),
        received_at_nanos: 1_000,
        provider_observed_at_original: Some("2026-09-02T00:00:00Z".into()),
        evidence_capsule: "{\"sanitized\":true}".into(),
        capsule_schema_version: "v1".into(),
        sanitizer_version: "v1".into(),
        capture_truncated: false,
        account_id,
        provider: "fixture-provider".into(),
        provider_observed_at_nanos: Some(900),
        measurement_basis: measurement_basis_sql::as_sql(MeasurementBasis::ProviderObserved)
            .to_owned(),
        observed_plan: Some("fixture-plan".into()),
        observed_tier: None,
        adapter_version: "fixture-adapter".into(),
        provider_contract_id: "fixture-contract".into(),
        meter_semantics_id: "fixture-semantics".into(),
        normalized_fingerprint: format!("fixture-fp-{attempt_id}"),
        windows: vec![PendingWindow {
            semantic_key: "five_hour".into(),
            scope_kind: "account_wide".into(),
            scoped_model: None,
            quota_used_ppm: 250_000,
            reported_resolution_ppm: 10_000,
            quantization: "exact".into(),
            resets_at_nanos: 5_000,
            nominal_duration_nanos: 18_000_000_000_000,
        }],
    }
}
