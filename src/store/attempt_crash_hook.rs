//! The crash-injection surface for the write-path crash matrix
//! (`aub-sth.14`, PLAN.md sections 13, 34.7), the test-only harness.
//!
//! Exposes named injection points across the five write-path stages:
//! 1. Before attempt-start commit (`BeforeStartCommit`)
//! 2. After attempt-start commit and before request returns (`AfterStartCommitBeforeRequest`)
//! 3. After network parse and before spool write (`AfterParseBeforeSpoolWrite`)
//! 4. After spool write and before SQLite commit (`AfterSpoolWriteBeforeSqliteCommit`)
//! 5. After SQLite commit and before pending deletion (`AfterSqliteCommitBeforePendingDeletion`)
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
//! `Complete` is the positive control running all stages cleanly without crash.
//! `ReadBack` counts database records and pending spool files without writing.
//! `Drain` runs the startup recovery pass over the pending spool directory.
//! `Freshness` evaluates the freshness state of the latest attempt past the command horizon.
//!
//! The harness lives in the store layer (`crate::store::attempt_crash_hook`)
//! because it runs migrations, writes fixture rows and counts rows, all of
//! which the boundary rules confine to `src/store/` (rules 15 and 16); the CLI
//! shim only parses the stage, resolves configuration and renders the outcome.
//!
//! Why the harness ships in the binary instead of hiding behind a cargo feature:
//! the injection points are abort call sites confined to this one module, and
//! this module is reachable only through the hidden `__attempt-crash-hook`
//! command, which drives a fixture database (`attempt-crash-hook.db`), never
//! the operator's ledger (with the exception of `sample`/`sample-crash` which
//! explicitly exercise the ledger database under contention). Gating the aborts
//! out of a default build would break the end-to-end case
//! (`tests/e2e/cases/009-attempt-crash.sh`) that drives the release binary
//! through this hook and asserts the abort by signal, and a feature-gated
//! matrix would leave plain `cargo test`. The property the bead asks to hold
//! is asserted by the matrix's own unit test: no injection point exists outside
//! this module, and no command on the shipping surface reaches one.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation
//! - configuration (the caller passes resolved values in)

use std::path::Path;

use crate::domain::attempt::{AttemptOutcome, AttemptResult, AttemptStarted};
use crate::domain::freshness::{
    Freshness, FreshnessInput, LatestAttempt, StaleReason, compute_freshness,
};
use crate::domain::ids::{
    AdapterVersion, CredentialContextId, MeterSemanticsId, ProviderContractId,
};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{
    Clock, ClockSkewEnvelope, FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::store::account::{AccountId, observe_account};
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::meter_attempt::{
    DueReason, MeterAttemptRowId, NewMeterAttempt, NewMeterAttemptResult, attempt_by_row_id,
    count_attempts, latest_attempt_row_id, result_by_attempt_id, start_meter_attempt,
};
use crate::store::meter_evidence::{
    NewMeterResponseEvidence, count_observations, measurement_basis_sql,
};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::repository::{
    NewMeterInterpretation, Repository, TerminalMeterBundle, commit_terminal_bundle_on_connection,
};
use crate::store::sample_run::{SampleRunId, Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
};
use crate::store::spool::{
    self, PendingTerminalBundle, PendingWindow, drain_pending, is_pending_record_name, pending_dir,
    pending_file_path, spool_pending,
};

/// The stages the `__attempt-crash-hook` command drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookStage {
    /// Injected before the attempt-start commit (Point 1).
    BeforeStartCommit,
    /// Injected after the attempt-start commit and before the request returns (Point 2).
    AfterStartCommitBeforeRequest,
    /// Injected after network parse and before the spool write (Point 3).
    AfterParseBeforeSpoolWrite,
    /// Injected after the spool write and before the SQLite commit (Point 4).
    AfterSpoolWriteBeforeSqliteCommit,
    /// Injected after the SQLite commit and before the pending deletion (Point 5).
    AfterSqliteCommitBeforePendingDeletion,
    /// Run all write-path stages and exit cleanly: the positive control.
    Complete,
    /// Count what the database and pending spool hold, without writing anything.
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
    /// Explicitly trigger pending-spool drain and recovery into the database.
    Drain,
    /// Evaluate the freshness of the latest attempt past the command horizon.
    Freshness,
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
    /// The complete stage wrote all facts.
    Completed { attempt_row_id: MeterAttemptRowId },
    /// The read-back stage counted the database and spool.
    Counts {
        starts: u64,
        results: u64,
        observations: u64,
        pending: u64,
    },
    /// A seeding stage wrote what it was asked to, named by `label` so the
    /// shim's one print per outcome stays honest about what it did.
    Seeded {
        label: &'static str,
        attempt_id: i64,
    },
    /// The drain stage completed a recovery pass.
    DrainReport {
        applied: usize,
        already_applied: usize,
        quarantined: usize,
    },
    /// The freshness check evaluated the latest attempt.
    FreshnessOutcome {
        kind: String,
        reason: Option<String>,
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

fn count_pending_records(state_dir: &Path) -> Result<u64, Error> {
    let dir = pending_dir(state_dir);
    if !dir.exists() {
        return Ok(0);
    }
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| Error::Store(format!("cannot read pending directory {dir:?}: {e}")))?;
    let mut count = 0;
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::Store(format!("cannot read pending directory entry: {e}")))?;
        if is_pending_record_name(&entry.path()) {
            count += 1;
        }
    }
    Ok(count)
}

fn make_fixture_bundles(
    attempt_id: MeterAttemptRowId,
    account_id: AccountId,
    clock: &impl Clock,
) -> (PendingTerminalBundle, TerminalMeterBundle) {
    let mono_start = clock.monotonic_now();
    let completed_at = clock.now();
    let elapsed = clock
        .monotonic_now()
        .duration_since(mono_start)
        .max(MonotonicDuration::from_nanos(1));

    let result = NewMeterAttemptResult {
        attempt_id,
        completed_at,
        elapsed,
        outcome: AttemptOutcome::Success,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
    };
    let evidence = NewMeterResponseEvidence {
        attempt_id,
        response_classification: "200".into(),
        received_at: completed_at,
        provider_observed_at_original: Some("2026-09-02T00:00:00Z".into()),
        evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
        capsule_schema_version: "capsule-v1".into(),
        sanitizer_version: "sanitizer-v1".into(),
        capture_truncated: false,
    };
    let interpretation = NewMeterInterpretation {
        account_id,
        provider: "fixture-provider".into(),
        provider_observed_at: Some(completed_at),
        received_at: completed_at,
        measurement_basis: MeasurementBasis::ProviderObserved,
        observed_plan: Some("max".into()),
        observed_tier: None,
        adapter_version: AdapterVersion::new("adapter-v1"),
        provider_contract_id: ProviderContractId::new("fixture-endpoint-schema"),
        meter_semantics_id: MeterSemanticsId::new("fixture-meter-semantics"),
        normalized_fingerprint: "fingerprint-v1".into(),
    };
    let windows = vec![MeterWindow::new(
        WindowSemanticKey::new("five_hour"),
        WindowScope::AccountWide,
        QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
        ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
        QuantizationSemantics::RoundedToNearest,
        completed_at,
        NominalWindowDuration::from_nanos(18_000_000_000_000),
    )];

    let pending_bundle = PendingTerminalBundle {
        attempt_id: attempt_id.value(),
        completed_at_nanos: completed_at.unix_nanos(),
        elapsed_nanos: elapsed.as_nanos() as i64,
        outcome: "success".into(),
        failure_class: None,
        retry_after_nanos: None,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
        response_classification: "200".into(),
        received_at_nanos: completed_at.unix_nanos(),
        provider_observed_at_original: Some("2026-09-02T00:00:00Z".into()),
        evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
        capsule_schema_version: "capsule-v1".into(),
        sanitizer_version: "sanitizer-v1".into(),
        capture_truncated: false,
        account_id: account_id.value(),
        provider: "fixture-provider".into(),
        provider_observed_at_nanos: Some(completed_at.unix_nanos()),
        measurement_basis: "provider_observed".into(),
        observed_plan: Some("max".into()),
        observed_tier: None,
        adapter_version: "adapter-v1".into(),
        provider_contract_id: "fixture-endpoint-schema".into(),
        meter_semantics_id: "fixture-meter-semantics".into(),
        normalized_fingerprint: "fingerprint-v1".into(),
        windows: vec![PendingWindow {
            semantic_key: "five_hour".into(),
            scope_kind: "account_wide".into(),
            scoped_model: None,
            quota_used_ppm: 250_000,
            reported_resolution_ppm: 10_000,
            quantization: "rounded_to_nearest".into(),
            resets_at_nanos: completed_at.unix_nanos(),
            nominal_duration_nanos: 18_000_000_000_000,
        }],
    };

    let terminal_bundle =
        TerminalMeterBundle::new(result, evidence, interpretation, windows).unwrap();
    (pending_bundle, terminal_bundle)
}

/// Runs one crash-hook stage against the database and spool directory.
///
/// Runs one crash-hook stage against the database and spool directory.
///
/// Injected crash stages abort the process with `std::process::abort()`, exercising
/// real crash semantics rather than a clean shutdown path. `ReadBack` returns counts
/// of what is durable, `Drain` runs pending-spool recovery, and `Complete` executes
/// all write-path stages cleanly to completion. The sample stages drain first,
/// then run the PLAN.md section 13 cycle against `db_path` (the ledger database
/// the caller passes for them), and return one outcome per attempt.
pub fn run_stage(
    state_dir: &Path,
    db_path: &Path,
    stage: CrashHookStage,
    busy_timeout: MonotonicDuration,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<CrashHookOutcome, Error> {
    if stage == CrashHookStage::ReadBack {
        let pending = count_pending_records(state_dir)?;
        if !db_path.exists() {
            return Ok(CrashHookOutcome::Counts {
                starts: 0,
                results: 0,
                observations: 0,
                pending,
            });
        }
        let conn = open(
            db_path,
            AccessMode::ReadOnly,
            &PragmaPolicy { busy_timeout },
        )?;
        let (starts, results) = count_attempts(&conn)?;
        let observations = count_observations(&conn)?;
        return Ok(CrashHookOutcome::Counts {
            starts,
            results,
            observations,
            pending,
        });
    }

    if stage == CrashHookStage::Drain {
        let mut conn = open(
            db_path,
            AccessMode::ReadWrite,
            &PragmaPolicy { busy_timeout },
        )?;
        run_migrations(&mut conn, &registry(), None, clock)?;
        let report = drain_pending(&mut conn, state_dir)?;
        return Ok(CrashHookOutcome::DrainReport {
            applied: report.applied,
            already_applied: report.already_applied,
            quarantined: report.quarantined,
        });
    }

    if stage == CrashHookStage::Freshness {
        if !db_path.exists() {
            return Ok(CrashHookOutcome::FreshnessOutcome {
                kind: "stale".into(),
                reason: Some("no_successful_observation".into()),
            });
        }
        let conn = open(
            db_path,
            AccessMode::ReadOnly,
            &PragmaPolicy { busy_timeout },
        )?;
        let Some(row_id) = latest_attempt_row_id(&conn)? else {
            return Ok(CrashHookOutcome::FreshnessOutcome {
                kind: "stale".into(),
                reason: Some("no_successful_observation".into()),
            });
        };
        let Some(stored) = attempt_by_row_id(&conn, row_id)? else {
            return Ok(CrashHookOutcome::FreshnessOutcome {
                kind: "stale".into(),
                reason: Some("no_successful_observation".into()),
            });
        };
        let terminal = result_by_attempt_id(&conn, row_id)?;

        // The fixture horizons are the harness's own, not configuration: a
        // resultless attempt is evaluated 60 seconds after its start, well
        // past the 10-second command horizon, so an interrupted collector
        // reads as interruption and never as a fresh reading or an endpoint
        // timeout (PLAN.md section 34.7).
        let attempt_id = row_id.as_attempt_id()?;
        let started = AttemptStarted::new(attempt_id, stored.request_started_at);
        let attempt_result = terminal.map(|result| {
            AttemptResult::new(
                attempt_id,
                result.completed_at,
                result.elapsed,
                result.outcome,
            )
        });
        let credential_context = CredentialContextId::new(
            stored
                .credential_context_id
                .unwrap_or_else(|| "ctx-1".into()),
        );
        let latest = LatestAttempt::new(started, attempt_result, &credential_context);
        let input = FreshnessInput::<u64>::new(
            None,
            None,
            Some(latest),
            None,
            Some(&credential_context),
            MonotonicDuration::from_seconds(300),
            MonotonicDuration::from_seconds(10),
            ClockSkewEnvelope::new(MonotonicDuration::from_seconds(10)),
        );
        let eval_clock = FakeClock::new(UtcTimestamp::from_unix_nanos(
            stored.request_started_at.unix_nanos() + 60_000_000_000,
        ));
        let freshness = compute_freshness(&input, &eval_clock);
        let (kind, reason) = match freshness {
            Freshness::Fresh { .. } => ("fresh".to_string(), None),
            Freshness::AuthRequired { .. } => ("auth_required".to_string(), None),
            Freshness::Stale { reason, .. } => {
                let r = match reason {
                    StaleReason::AgeExceeded => "age_exceeded",
                    StaleReason::NoSuccessfulObservation => "no_successful_observation",
                    StaleReason::SourceUnreachable(_) => "source_unreachable",
                    StaleReason::MalformedProviderResponse => "malformed_provider_response",
                    StaleReason::RateLimited => "rate_limited",
                    StaleReason::SamplingGap => "sampling_gap",
                    StaleReason::ClockAnomaly => "clock_anomaly",
                    StaleReason::CollectorInterrupted => "collector_interrupted",
                    StaleReason::CredentialChangedUnverified => "credential_changed_unverified",
                };
                ("stale".to_string(), Some(r.to_string()))
            }
        };
        return Ok(CrashHookOutcome::FreshnessOutcome { kind, reason });
    }

    let mut conn = open(
        db_path,
        AccessMode::ReadWrite,
        &PragmaPolicy { busy_timeout },
    )?;

    run_migrations(&mut conn, &registry(), None, clock)?;

    match stage {
        CrashHookStage::Sample { attempts } => {
            // A mutating workflow drains the pending spool before its own work
            // (PLAN.md section 13), so evidence left by a crash or a busy
            // timeout enters the ledger before new attempts are made.
            let drain = crate::store::spool::drain_pending(&mut conn, state_dir)?;
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
                &crate::projection::projection_path_in(state_dir),
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
            spool::spool_pending(state_dir, &PendingTerminalBundle::from_bundle(&bundle))?;
            std::process::abort();
        }
        CrashHookStage::BeforeStartCommit
        | CrashHookStage::AfterStartCommitBeforeRequest
        | CrashHookStage::AfterParseBeforeSpoolWrite
        | CrashHookStage::AfterSpoolWriteBeforeSqliteCommit
        | CrashHookStage::AfterSqliteCommitBeforePendingDeletion
        | CrashHookStage::Complete
        | CrashHookStage::CommitObservation
        | CrashHookStage::SpoolPending
        | CrashHookStage::SpoolOrphan { .. } => {
            start_then_maybe_abort(&mut conn, stage, state_dir, command_budget, clock)
        }
        CrashHookStage::ReadBack | CrashHookStage::Drain | CrashHookStage::Freshness => {
            unreachable!("handled above")
        }
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
    state_dir: &Path,
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

    // Point 1: Before the attempt-start commit
    if stage == CrashHookStage::BeforeStartCommit {
        std::process::abort();
    }

    let row_id = start_meter_attempt(conn, &attempt)?;

    if stage == CrashHookStage::CommitObservation {
        // The result rides inside the terminal bundle: the repository boundary
        // records it in the same transaction as the evidence, the observation
        // and the windows, which is what makes the seeded row the same shape
        // a live sample produces rather than a hand-written half of one.
        let bundle = terminal_bundle_for(row_id, account, started_at)?;
        commit_terminal_bundle_on_connection(conn, &bundle, || Ok(()))?;
        return Ok(CrashHookOutcome::Seeded {
            label: "committed",
            attempt_id: row_id.value(),
        });
    }

    if stage == CrashHookStage::SpoolPending {
        spool_pending(
            state_dir,
            &pending_bundle_for(row_id.value(), account.value(), started_at),
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
        spool_pending(
            state_dir,
            &pending_bundle_for(attempt_id, account.value(), started_at),
        )?;
        return Ok(CrashHookOutcome::Seeded {
            label: "spooled-orphan",
            attempt_id,
        });
    }

    // Point 2: After the attempt-start commit and before the request returns
    if stage == CrashHookStage::AfterStartCommitBeforeRequest {
        std::process::abort();
    }

    let (pending_bundle, terminal_bundle) = make_fixture_bundles(row_id, account, clock);

    // Point 3: After network parse and before the spool write
    if stage == CrashHookStage::AfterParseBeforeSpoolWrite {
        std::process::abort();
    }

    spool_pending(state_dir, &pending_bundle)?;

    // Point 4: After the spool write and before the SQLite commit
    if stage == CrashHookStage::AfterSpoolWriteBeforeSqliteCommit {
        std::process::abort();
    }

    commit_terminal_bundle_on_connection(conn, &terminal_bundle, || Ok(()))?;

    // Point 5: After the SQLite commit and before the pending deletion
    if stage == CrashHookStage::AfterSqliteCommitBeforePendingDeletion {
        std::process::abort();
    }

    // The deletion the matrix's fifth injection point sits in front of: the
    // pending record's work is durable in SQLite, so the record's only
    // remaining purpose is to be absent on the next drain. The path comes
    // from the spool module itself, never from a re-derived filename format.
    let pending_file = pending_file_path(state_dir, row_id.value());
    let _ = std::fs::remove_file(&pending_file);

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
    started_at: UtcTimestamp,
) -> Result<TerminalMeterBundle, Error> {
    let received_at = UtcTimestamp::from_unix_nanos(started_at.unix_nanos().saturating_add(500));
    let result = NewMeterAttemptResult {
        attempt_id: row_id,
        completed_at: UtcTimestamp::from_unix_nanos(started_at.unix_nanos().saturating_add(1_000)),
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
        provider_observed_at: Some(started_at),
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
        UtcTimestamp::from_unix_nanos(started_at.unix_nanos().saturating_add(5_000)),
        NominalWindowDuration::from_nanos(18_000_000_000_000),
    );
    TerminalMeterBundle::new(result, evidence, interpretation, vec![window])
}

/// The same fixture bundle as [`terminal_bundle_for`], flattened into the
/// spool's durable on-disk shape, for the two spooling stages.
fn pending_bundle_for(
    attempt_id: i64,
    account_id: i64,
    started_at: UtcTimestamp,
) -> PendingTerminalBundle {
    PendingTerminalBundle {
        attempt_id,
        completed_at_nanos: started_at.unix_nanos().saturating_add(1_000),
        elapsed_nanos: 1_000,
        outcome: "success".into(),
        failure_class: None,
        retry_after_nanos: None,
        sanitized_error_classification: None,
        retry_index: None,
        clock_anomaly: false,
        response_classification: "success".into(),
        received_at_nanos: started_at.unix_nanos().saturating_add(500),
        provider_observed_at_original: Some("2026-09-02T00:00:00Z".into()),
        evidence_capsule: "{\"sanitized\":true}".into(),
        capsule_schema_version: "v1".into(),
        sanitizer_version: "v1".into(),
        capture_truncated: false,
        account_id,
        provider: "fixture-provider".into(),
        provider_observed_at_nanos: Some(started_at.unix_nanos()),
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
            resets_at_nanos: started_at.unix_nanos().saturating_add(5_000),
            nominal_duration_nanos: 18_000_000_000_000,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::FakeClock;
    use test_support::StateDir;

    #[test]
    fn complete_stage_writes_all_facts_and_read_back_reports_exact_counts() {
        let scratch = StateDir::new();
        let db_path = scratch.path().join("crash-test.db");
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(100_000_000));
        let timeout = MonotonicDuration::from_seconds(5);

        let outcome = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::Complete,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();

        let CrashHookOutcome::Completed { attempt_row_id } = outcome else {
            panic!("expected Completed outcome");
        };
        assert_eq!(attempt_row_id.value(), 1);

        let counts = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::ReadBack,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();

        assert_eq!(
            counts,
            CrashHookOutcome::Counts {
                starts: 1,
                results: 1,
                observations: 1,
                pending: 0,
            }
        );
    }

    #[test]
    fn drain_recovers_pending_file_into_exact_observation_count() {
        let scratch = StateDir::new();
        let db_path = scratch.path().join("drain-test.db");
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(100_000_000));
        let timeout = MonotonicDuration::from_seconds(5);

        // Seed attempt start and pending spool file directly
        let mut conn = open(
            &db_path,
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: timeout,
            },
        )
        .unwrap();
        run_migrations(&mut conn, &registry(), None, &clock).unwrap();
        let account =
            observe_account(&conn, "fixture-provider", "fixture-account", clock.now()).unwrap();
        let run = start_sample_run(&conn, Trigger::Manual, clock.now(), "test").unwrap();
        let snapshot_policy = ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_millis(300_000),
            freshness_horizon: MonotonicDuration::from_millis(900_000),
            reset_edge_policy: "fixture".to_string(),
            retry_backoff_policy: "fixture".to_string(),
            command_budget: timeout,
            policy_algorithm_version: "fixture-1".to_string(),
        };
        let snapshot =
            resolve_policy_snapshot(&conn, account, clock.now(), &snapshot_policy).unwrap();
        let attempt = NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "fixture-provider".to_string(),
            request_started_at: clock.now(),
            credential_context_id: Some("fixture-credential-context".to_string()),
            policy_snapshot_id: snapshot,
            due_at: clock.now(),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "fixture-endpoint-schema".to_string(),
            meter_semantics_id: "fixture-meter-semantics".to_string(),
        };
        let row_id = start_meter_attempt(&conn, &attempt).unwrap();
        let (pending_bundle, _) = make_fixture_bundles(row_id, account, &clock);
        spool_pending(scratch.path(), &pending_bundle).unwrap();
        drop(conn);

        // Before drain: starts=1, results=0, observations=0, pending=1
        let pre_counts = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::ReadBack,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        assert_eq!(
            pre_counts,
            CrashHookOutcome::Counts {
                starts: 1,
                results: 0,
                observations: 0,
                pending: 1,
            }
        );

        // Drain
        let drain_res = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::Drain,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        assert_eq!(
            drain_res,
            CrashHookOutcome::DrainReport {
                applied: 1,
                already_applied: 0,
                quarantined: 0,
            }
        );

        // After drain: starts=1, results=1, observations=1, pending=0
        let post_counts = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::ReadBack,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        assert_eq!(
            post_counts,
            CrashHookOutcome::Counts {
                starts: 1,
                results: 1,
                observations: 1,
                pending: 0,
            }
        );
    }

    #[test]
    fn commit_observation_and_spool_pending_seed_the_shapes_a_restore_drill_counts() {
        let scratch = StateDir::new();
        let db_path = scratch.path().join("seed-test.db");
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(100_000_000));
        let timeout = MonotonicDuration::from_seconds(5);

        let committed = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::CommitObservation,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        let CrashHookOutcome::Seeded { label, attempt_id } = committed else {
            panic!("expected Seeded outcome");
        };
        assert_eq!(label, "committed");
        assert_eq!(attempt_id, 1);

        let spooled = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::SpoolPending,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        let CrashHookOutcome::Seeded { label, attempt_id } = spooled else {
            panic!("expected Seeded outcome");
        };
        assert_eq!(label, "spooled");
        assert_eq!(attempt_id, 2);

        // Orphan spool needs an id no attempt row holds; the two seeds above
        // used 1 and 2. Every stage still seeds its own attempt start before
        // branching, so this one leaves a third, incidental start row behind
        // even though the orphan record itself is spooled under id 999.
        let orphaned = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::SpoolOrphan { attempt_id: 999 },
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        let CrashHookOutcome::Seeded { label, attempt_id } = orphaned else {
            panic!("expected Seeded outcome");
        };
        assert_eq!(label, "spooled-orphan");
        assert_eq!(attempt_id, 999);

        let counts = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::ReadBack,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        assert_eq!(
            counts,
            CrashHookOutcome::Counts {
                starts: 3,
                results: 1,
                observations: 1,
                pending: 2,
            }
        );
    }

    #[test]
    fn spool_orphan_refuses_an_id_that_already_has_a_start_row() {
        let scratch = StateDir::new();
        let db_path = scratch.path().join("orphan-refuse-test.db");
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(100_000_000));
        let timeout = MonotonicDuration::from_seconds(5);

        let seeded = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::SpoolPending,
            timeout,
            timeout,
            &clock,
        )
        .unwrap();
        let CrashHookOutcome::Seeded { attempt_id, .. } = seeded else {
            panic!("expected Seeded outcome");
        };

        let result = run_stage(
            scratch.path(),
            &db_path,
            CrashHookStage::SpoolOrphan { attempt_id },
            timeout,
            timeout,
            &clock,
        );
        assert!(result.is_err(), "expected refusal for a non-orphan id");
    }
}
