//! Test ledger for the calibrate CLI unit tests (`aub-c0b.2`).
//!
//! Lives under `src/store` because the boundary rules forbid SQL and the
//! migration framework outside the store: `src/cli.rs` test code may not seed
//! fixtures with raw SQL nor import `crate::store::migrate`. The helpers here
//! build the same meter chain through the store's own insert functions, and the
//! CLI tests call them.
//!
//! Only compiled for tests: the release binary never carries fixture builders.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use crate::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
use crate::domain::time::{FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp};
use crate::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowResetState,
    WindowScope, WindowSemanticKey,
};
use crate::store::account::observe_account;
use crate::store::connection::{AccessMode, PragmaPolicy, open};
use crate::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
use crate::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
    insert_response_evidence, insert_window,
};
use crate::store::migrate::run_migrations;
use crate::store::migrations::registry;
use crate::store::sample_run::{Trigger, start_sample_run};
use crate::store::sampling_policy_snapshot::{ResolvedSamplingPolicy, resolve_policy_snapshot};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Owns the temp directory holding the scratch ledger for one CLI calibrate
/// test. Removing the directory on drop keeps the fixture hermetic.
pub struct CalibrateCliTestLedgerDir(PathBuf);

impl CalibrateCliTestLedgerDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-cli-calibrate-test-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CalibrateCliTestLedgerDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens a migrated scratch ledger the way a fresh process would: a new
/// connection over a temp file, running the idempotent migrations.
pub fn open_calibrate_cli_test_ledger() -> (CalibrateCliTestLedgerDir, Connection) {
    let scratch = CalibrateCliTestLedgerDir::new();
    let mut conn = open(
        &scratch.path().join("calibrate.db"),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("scratch database must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .expect("migrations must run");
    (scratch, conn)
}

const CALIBRATE_CLI_POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
    ordinary_cadence: MonotonicDuration::from_nanos(3_600_000_000_000),
    freshness_horizon: MonotonicDuration::from_nanos(300_000_000_000),
    reset_edge_policy: String::new(),
    retry_backoff_policy: String::new(),
    command_budget: MonotonicDuration::from_nanos(10_000_000_000),
    policy_algorithm_version: String::new(),
};

/// Inserts the thinnest meter chain a calibrate baseline needs: account, run,
/// policy snapshot, attempt, evidence, observation and one window for
/// `semantic_key` at `received_at` with `quota_ppm`. Built only through the
/// store's own insert functions, never raw SQL.
pub fn insert_calibrate_cli_meter_chain(
    conn: &Connection,
    account: &str,
    semantic_key: &str,
    received_at: UtcTimestamp,
    quota_ppm: i64,
) {
    let quota_ppm_i32 = i32::try_from(quota_ppm).expect("calibrate fixture quota must fit in i32");
    let account_id =
        observe_account(conn, "anthropic", account, received_at).expect("account insert must work");
    let run_id = start_sample_run(conn, Trigger::Manual, received_at, "fp")
        .expect("sample run insert must work");
    // The policy values match the historical fixture: hourly cadence, five
    // minute horizon, lead-60s reset edge, no backoff, ten second budget, v1.
    // `String::new` cannot carry those in a const, so the owned strings are
    // built here per call.
    let policy = ResolvedSamplingPolicy {
        ordinary_cadence: CALIBRATE_CLI_POLICY.ordinary_cadence,
        freshness_horizon: CALIBRATE_CLI_POLICY.freshness_horizon,
        reset_edge_policy: "lead-60s".to_string(),
        retry_backoff_policy: "none".to_string(),
        command_budget: CALIBRATE_CLI_POLICY.command_budget,
        policy_algorithm_version: "v1".to_string(),
    };
    let snapshot_id = resolve_policy_snapshot(conn, account_id, received_at, &policy)
        .expect("policy snapshot insert must work");
    let attempt_id = start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id,
            account_id,
            provider: "anthropic".to_string(),
            request_started_at: received_at,
            credential_context_id: None,
            policy_snapshot_id: snapshot_id,
            due_at: received_at,
            due_reason: DueReason::ForcedOrManual,
            due_basis: None,
            provider_contract_id: "contract-v1".to_string(),
            meter_semantics_id: "meter-v1".to_string(),
        },
    )
    .expect("attempt insert must work");
    let evidence_id = insert_response_evidence(
        conn,
        &NewMeterResponseEvidence {
            attempt_id,
            response_classification: "success".to_string(),
            received_at,
            provider_observed_at_original: None,
            evidence_capsule: "capsule".to_string(),
            capsule_schema_version: "v1".to_string(),
            sanitizer_version: "v1".to_string(),
            capture_truncated: false,
        },
    )
    .expect("evidence insert must work");
    let observation_id = insert_observation(
        conn,
        &NewMeterObservation {
            attempt_id,
            evidence_id,
            account_id,
            provider: "anthropic".to_string(),
            provider_observed_at: None,
            received_at,
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: None,
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("contract-v1"),
            meter_semantics_id: MeterSemanticsId::new("meter-v1"),
            normalized_fingerprint: "fingerprint".to_string(),
        },
    )
    .expect("observation insert must work");
    let quota_used =
        QuotaUsed::new(QuotaFractionPpm::new(quota_ppm_i32).expect("valid test quota"));
    let reported_resolution =
        ReportedResolution::new(QuotaFractionPpm::new(10_000).expect("valid test resolution"))
            .expect("non-zero test resolution");
    insert_window(
        conn,
        &NewMeterWindow {
            observation_id,
            semantic_key: WindowSemanticKey::new(semantic_key),
            scope: WindowScope::AccountWide,
            quota_used,
            reported_resolution,
            quantization: QuantizationSemantics::Exact,
            resets_at: WindowResetState::Known(UtcTimestamp::from_unix_nanos(
                received_at.unix_nanos() + 18_000_000_000_000,
            )),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        },
    )
    .expect("window insert must work");
}
