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
//! `Complete` is the positive control running all stages cleanly without crash.
//! `ReadBack` counts database records and pending spool files without writing.
//! `Drain` runs the startup recovery pass over the pending spool directory.
//! `Freshness` evaluates the freshness state of the latest attempt past the command horizon.
//!
//! Why the harness ships in the binary instead of hiding behind a cargo feature:
//! the injection points are abort call sites confined to this one module, and
//! this module is reachable only through the hidden `__attempt-crash-hook`
//! command, which drives a fixture database (`attempt-crash-hook.db`), never
//! the operator's ledger. Gating the aborts out of a default build would break
//! the end-to-end case (`tests/e2e/cases/009-attempt-crash.sh`) that drives the
//! release binary through this hook and asserts the abort by signal, and a
//! feature-gated matrix would leave plain `cargo test`. The property the bead
//! asks to hold is asserted by the matrix's own unit test: no injection point
//! exists outside this module, and no command on the shipping surface reaches
//! one. That test is the source-level check, the layer that can actually fail:
//! `[profile.release] strip = true` and dead-code elimination make a
//! binary-contents check unprovable (bin/checks/
//! 65-synthetic-adapter-absent-from-release documents the same reasoning).
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
use crate::store::meter_evidence::{NewMeterResponseEvidence, count_observations};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::repository::{
    NewMeterInterpretation, TerminalMeterBundle, commit_terminal_bundle_on_connection,
};
use crate::store::sample_run::{Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};
use crate::store::spool::{
    PendingTerminalBundle, PendingWindow, drain_pending, is_pending_record_name, pending_dir,
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
    /// Explicitly trigger pending-spool drain and recovery into the database.
    Drain,
    /// Evaluate the freshness of the latest attempt past the command horizon.
    Freshness,
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
/// Injected crash stages abort the process with `std::process::abort()`, exercising
/// real crash semantics rather than a clean shutdown path. `ReadBack` returns counts
/// of what is durable, `Drain` runs pending-spool recovery, and `Complete` executes
/// all write-path stages cleanly to completion.
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

    // Point 1: Before the attempt-start commit
    if stage == CrashHookStage::BeforeStartCommit {
        std::process::abort();
    }

    let row_id = start_meter_attempt(&conn, &attempt)?;

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

    commit_terminal_bundle_on_connection(&mut conn, &terminal_bundle, || Ok(()))?;

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
}
