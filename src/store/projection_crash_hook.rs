//! The crash-injection surface for the publication ordering contract
//! (`aub-me5.5`, PLAN.md section 16.1), test-only.
//!
//! Like its meter-attempt sibling, the hook lives in the store layer because
//! it runs migrations, writes fixture rows and counts rows, all of which the
//! boundary rules confine to `src/store/` (rules 15 and 16); the projection
//! module stays free of migration references because the status path's source
//! must never run one.
//!
//! `__projection-crash-hook kill-before-publish` seeds fixture meter state,
//! runs one attempt and terminal bundle through the real repository path so a
//! projection exists on disk, then runs a second bundle whose publication step
//! is replaced by `abort`, and dies there. What the next process finds is the
//! ordering contract itself: the database holds both terminal facts and the
//! generation they advanced, while the projection on disk still describes the
//! earlier generation. A crash between the commit and the publication can
//! leave the projection older than SQLite and must never leave it ahead.
//!
//! `publish` is the adjacent positive control, committing and publishing and
//! exiting cleanly, and `read-back` reports what the database and the file
//! hold without writing anything.
//!
//! The hook lives beside the projection it exercises: the injection point is
//! the publication seam the repository exposes for exactly this proof, and
//! read-back parses the projection file format this module tree owns. It
//! drives store APIs only.
//!
//! May not depend on:
//! - HTTP or provider semantics
//! - presentation
//! - configuration (the caller passes the resolved values in)

use std::fs;
use std::path::Path;

use crate::domain::attempt::AttemptOutcome;
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{Clock, MeasurementBasis, MonotonicDuration};
use crate::domain::window::{
    MeterWindow, NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use crate::error::Error;
use crate::store::account::{AccountId, observe_account};
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::ledger_generation;
use crate::store::meter_attempt::{DueReason, MeterAttemptRowId, NewMeterAttempt};
use crate::store::meter_evidence::NewMeterResponseEvidence;
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::repository::{NewMeterInterpretation, Repository, TerminalMeterBundle};
use crate::store::sample_run::{SampleRunId, Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{
    ResolvedSamplingPolicy, SamplingPolicySnapshotId, resolve_policy_snapshot,
};

/// The stages the `__projection-crash-hook` command drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookStage {
    /// Seed fixture state and publish it through the production path, then
    /// exit cleanly: the adjacent positive control for the crash stage.
    Publish,
    /// Publish once, commit a second bundle, and abort the process at the
    /// injection point between that commit and the projection replacement.
    KillBeforePublish,
    /// Report what the database and the projection file hold, without writing.
    ReadBack,
}

/// What one stage produced, for the CLI shim to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashHookOutcome {
    /// The stage wrote and published its fixture state.
    Published,
    /// The read-back stage reported the two sides: how many terminal results
    /// the database holds, the database's generation, and the generation the
    /// projection file records, when it has one.
    Counts {
        results: u64,
        generation: u64,
        projection_generation: Option<u64>,
    },
}

/// The fixture state the stages share, built through the real store APIs.
struct Fixture {
    repository: Repository,
    account_id: AccountId,
    run_id: SampleRunId,
    policy_snapshot_id: SamplingPolicySnapshotId,
}

/// Runs one stage against `state_dir`. The `KillBeforePublish` stage never
/// returns: after the second bundle is durable it aborts the process, which is
/// the documented injection point.
pub fn run_stage(
    state_dir: &Path,
    stage: CrashHookStage,
    busy_timeout: MonotonicDuration,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<CrashHookOutcome, Error> {
    let database_path = state_dir.join("projection-crash-hook.db");
    let projection_path = crate::projection::projection_path_in(state_dir);
    if stage == CrashHookStage::ReadBack {
        return read_back(&database_path, &projection_path, busy_timeout);
    }

    let fixture = seed(&database_path, busy_timeout, command_budget, clock)?;
    let first = fixture.start_attempt(clock)?;
    let first_bundle = fixture.success_bundle(first, clock);
    fixture.repository.commit_terminal_bundle(&first_bundle)?;

    if stage == CrashHookStage::KillBeforePublish {
        let second = fixture.start_attempt(clock)?;
        let bundle = fixture.success_bundle(second, clock);
        // The injection point: the bundle above is durable, and the process
        // ends here by signal, before this second bundle's publication.
        // The call never returns: the closure aborts the process, which is
        // the documented injection point.
        let _ = fixture
            .repository
            .commit_terminal_bundle_publishing_with(&bundle, |_, _| std::process::abort());
    }

    Ok(CrashHookOutcome::Published)
}

fn read_back(
    database_path: &Path,
    projection_path: &Path,
    busy_timeout: MonotonicDuration,
) -> Result<CrashHookOutcome, Error> {
    let conn = open(
        database_path,
        AccessMode::ReadOnly,
        &PragmaPolicy { busy_timeout },
    )?;
    // The count comes through the store's own read, since SQL is confined to
    // src/store/ by the boundary rules.
    let (_starts, results) = crate::store::meter_attempt::count_attempts(&conn)?;
    let generation = ledger_generation::current(&conn)?.value();
    let projection_generation = fs::read_to_string(projection_path)
        .ok()
        .and_then(|text| crate::projection::recorded_generation(&text));
    Ok(CrashHookOutcome::Counts {
        results,
        generation,
        projection_generation,
    })
}

impl Fixture {
    fn start_attempt(&self, clock: &impl Clock) -> Result<MeterAttemptRowId, Error> {
        let started_at = clock.now();
        let attempt = NewMeterAttempt {
            run_id: self.run_id,
            account_id: self.account_id,
            provider: "fixture-provider".to_string(),
            request_started_at: started_at,
            credential_context_id: Some("fixture-credential-context".to_string()),
            policy_snapshot_id: self.policy_snapshot_id,
            due_at: started_at,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "fixture-endpoint-schema".to_string(),
            meter_semantics_id: "fixture-meter-semantics".to_string(),
        };
        let started = self.repository.start_meter_attempt(&attempt)?;
        i64::try_from(started.attempt_id().value())
            .map(MeterAttemptRowId::new)
            .map_err(|_| Error::Store("attempt identity exceeds SQLite INTEGER".into()))
    }

    /// One success bundle whose terminal timestamps come from the clock, so
    /// the schema's ordering trigger holds for a real-clock run.
    fn success_bundle(
        &self,
        attempt_id: MeterAttemptRowId,
        clock: &impl Clock,
    ) -> TerminalMeterBundle {
        let completed_at = clock.now();
        let window = MeterWindow::new(
            WindowSemanticKey::new("five_hour"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::RoundedToNearest,
            completed_at,
            NominalWindowDuration::from_nanos(18_000_000_000_000),
        );
        let evidence = NewMeterResponseEvidence {
            attempt_id,
            response_classification: "success".into(),
            received_at: completed_at,
            provider_observed_at_original: Some("1970-01-01T00:00:04Z".into()),
            evidence_capsule: "{\"five_hour\":\"25.0\"}".into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        };
        let interpretation = NewMeterInterpretation {
            account_id: self.account_id,
            provider: "fixture-provider".into(),
            provider_observed_at: Some(completed_at),
            received_at: completed_at,
            measurement_basis: MeasurementBasis::ProviderObserved,
            observed_plan: Some("fixture-plan".into()),
            observed_tier: None,
            adapter_version: crate::domain::ids::AdapterVersion::new("adapter-v1"),
            provider_contract_id: crate::domain::ids::ProviderContractId::new(
                "fixture-endpoint-schema",
            ),
            meter_semantics_id: crate::domain::ids::MeterSemanticsId::new(
                "fixture-meter-semantics",
            ),
            normalized_fingerprint: "fixture-fingerprint".into(),
        };
        TerminalMeterBundle::new(
            crate::store::meter_attempt::NewMeterAttemptResult {
                attempt_id,
                completed_at,
                elapsed: MonotonicDuration::from_nanos(1),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
            evidence,
            interpretation,
            vec![window],
        )
        .expect("fixture bundle is internally consistent")
    }
}

fn seed(
    database_path: &Path,
    busy_timeout: MonotonicDuration,
    command_budget: MonotonicDuration,
    clock: &impl Clock,
) -> Result<Fixture, Error> {
    let policy = PragmaPolicy { busy_timeout };
    let mut conn = open(database_path, AccessMode::ReadWrite, &policy)?;
    run_migrations(&mut conn, &registry(), None, clock)?;
    let account_id = observe_account(&conn, "fixture-provider", "fixture-account", clock.now())?;
    let run_id = start_sample_run(
        &conn,
        Trigger::Manual,
        clock.now(),
        "__projection-crash-hook",
    )?;
    let policy_snapshot_id = resolve_policy_snapshot(
        &conn,
        account_id,
        clock.now(),
        &ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_millis(300_000),
            freshness_horizon: MonotonicDuration::from_millis(900_000),
            reset_edge_policy: "fixture".to_string(),
            retry_backoff_policy: "fixture".to_string(),
            command_budget,
            policy_algorithm_version: "fixture-1".to_string(),
        },
    )?;
    drop(conn);
    Ok(Fixture {
        repository: Repository::new(database_path, policy),
        account_id,
        run_id,
        policy_snapshot_id,
    })
}
