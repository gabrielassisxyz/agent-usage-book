//! Process-level integration tests for `aub calibrate fit`.
//!
//! Covers:
//! - Successful quantized fit via release binary: seeded coefficient within uncertainty interval
//! - Immutability: candidate written immutably to window_calibration_candidate
//! - No activation: fit never creates a calibration_lifecycle entry
//! - Reproducibility: rerunning on the same evidence yields the same inputs_hash and coefficient
//! - JSON format output validity
//! - Typed rejection: InsufficientEvidence produces exit status 6

use std::process::Command;

use agent_usage_book::calibration::settlement::{SettlementCriterion, SettlementPolicy};
use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::ids::{
    AdapterVersion, BillingSemanticsId, MeterSemanticsId, ProviderContractId,
};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::store::calibration::{
    CalibrationExperiment, CandidateId, ExperimentId, PlanTier,
};
use agent_usage_book::store::cost_model::{ProviderKey, ValidityInterval, seed_initial_cost_model};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, NewMeterAttemptResult};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow,
};
use agent_usage_book::store::usage_component::NewUsageComponent;
use agent_usage_book::store::usage_event::NewUsageEvent;
use agent_usage_book::store::{
    account as account_store, calibration as calibration_store, connection,
    meter_attempt as attempt_store, meter_evidence as evidence_store, migrate, migrations,
    sample_run as run_store, sampling_policy_snapshot as snapshot_store,
    usage_component as component_store, usage_event as event_store,
};
use rusqlite::Connection;
use test_support::StateDir;

const SECOND: i64 = 1_000_000_000;

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

fn open_test_ledger(state: &StateDir) -> Connection {
    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let policy = connection::PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let mut conn = connection::open(&path, connection::AccessMode::ReadWrite, &policy)
        .expect("the scratch ledger must open");
    migrate::run_migrations(
        &mut conn,
        &migrations::registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("the scratch ledger must migrate");
    conn
}

fn test_experiment(id: &str) -> CalibrationExperiment {
    let res = ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap();
    let criterion = SettlementCriterion::new(
        MonotonicDuration::from_seconds(300),
        3,
        MonotonicDuration::from_seconds(600),
        0,
        MonotonicDuration::from_seconds(3600),
        res,
    )
    .unwrap();
    let policy = SettlementPolicy::new(
        "test-policy-v1",
        criterion,
        criterion,
        Some("shared for test".into()),
    )
    .unwrap();

    CalibrationExperiment {
        id: ExperimentId::new(id),
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("test-tier"),
        window_semantic_key: WindowSemanticKey::new("seven_day"),
        meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
        settlement_policy: policy,
        validity: ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(1_000 * SECOND),
            UtcTimestamp::from_unix_nanos(100_000 * SECOND),
        )
        .unwrap(),
        knowledge_time: UtcTimestamp::from_unix_nanos(100_000 * SECOND),
    }
}

fn seed_successful_experiment_ledger(state: &StateDir) {
    let mut conn = open_test_ledger(state);

    let experiment = test_experiment("exp-quantized-1");
    calibration_store::insert_experiment(&conn, &experiment).unwrap();

    // Seeds and activates anthropic-claude-messages-v1 at 500s
    seed_initial_cost_model(&mut conn, UtcTimestamp::from_unix_nanos(500 * SECOND)).unwrap();

    let account_id = account_store::observe_account(
        &conn,
        "anthropic",
        "work",
        UtcTimestamp::from_unix_nanos(100 * SECOND),
    )
    .unwrap();

    let run_id = run_store::start_sample_run(
        &conn,
        run_store::Trigger::Manual,
        UtcTimestamp::from_unix_nanos(100 * SECOND),
        "seed-run",
    )
    .unwrap();

    let snapshot_id = snapshot_store::resolve_policy_snapshot(
        &conn,
        account_id,
        UtcTimestamp::from_unix_nanos(100 * SECOND),
        &snapshot_store::ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(300),
            freshness_horizon: MonotonicDuration::from_seconds(900),
            reset_edge_policy: String::new(),
            retry_backoff_policy: String::new(),
            command_budget: MonotonicDuration::from_seconds(30),
            policy_algorithm_version: "v1".into(),
        },
    )
    .unwrap();

    // Seed 4 observations with 10_000 ppm resolution, rounded to nearest
    // Trajectory: 100_000 ppm, 130_000 ppm, 160_000 ppm, 190_000 ppm
    // Usage: 1,000,000 input tokens each between readings (3.0 credits per step)
    // Slope: (30_000 ppm) / (3.0 credits) = 10,000 ppm/credit -> 100 micros per point (100 credits capacity)
    let obs_data = [
        (1_000 * SECOND, 100_000, "ev-cap-1"),
        (2_000 * SECOND, 130_000, "ev-cap-2"),
        (3_000 * SECOND, 160_000, "ev-cap-3"),
        (4_000 * SECOND, 190_000, "ev-cap-4"),
    ];

    for (ts_nanos, used_ppm, ev_str) in obs_data {
        let ts = UtcTimestamp::from_unix_nanos(ts_nanos);
        let attempt_id = attempt_store::start_meter_attempt(
            &conn,
            &NewMeterAttempt {
                run_id,
                account_id,
                provider: "anthropic".into(),
                request_started_at: ts,
                credential_context_id: None,
                policy_snapshot_id: snapshot_id,
                due_at: ts,
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "contract-v1".into(),
                meter_semantics_id: "semantics-v1".into(),
            },
        )
        .unwrap();

        attempt_store::record_meter_attempt_result(
            &conn,
            &NewMeterAttemptResult {
                attempt_id,
                completed_at: ts,
                elapsed: MonotonicDuration::from_millis(50),
                outcome: AttemptOutcome::Success,
                sanitized_error_classification: None,
                retry_index: None,
                clock_anomaly: false,
            },
        )
        .unwrap();

        let evidence_row_id = evidence_store::insert_response_evidence(
            &conn,
            &NewMeterResponseEvidence {
                attempt_id,
                response_classification: "200".into(),
                received_at: ts,
                provider_observed_at_original: None,
                evidence_capsule: format!("{{\"hash\":\"{ev_str}\"}}"),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "san-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();

        let observation_row_id = evidence_store::insert_observation(
            &conn,
            &NewMeterObservation {
                attempt_id,
                evidence_id: evidence_row_id,
                account_id,
                provider: "anthropic".into(),
                provider_observed_at: Some(ts),
                received_at: ts,
                measurement_basis:
                    agent_usage_book::domain::time::MeasurementBasis::ProviderObserved,
                observed_plan: Some("test-tier".into()),
                observed_tier: Some("test-tier".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("contract-v1"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: format!("fp-{ev_str}"),
            },
        )
        .unwrap();

        evidence_store::insert_window(
            &conn,
            &NewMeterWindow {
                observation_id: observation_row_id,
                semantic_key: WindowSemanticKey::new("seven_day"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at: UtcTimestamp::from_unix_nanos(ts_nanos + 86_400 * SECOND).into(),
                nominal_duration: NominalWindowDuration::from_nanos(7 * 86_400 * 1_000_000_000),
            },
        )
        .unwrap();
    }

    // Seed usage events matching 3.0 credits between observations
    // In anthropic_claude_messages_v1, input rate is 3.0 credits per million tokens.
    // 1,000,000 input tokens = 3,000,000 micros = 3.0 credits
    let usage_points = [
        (1_500 * SECOND, "usage-1"),
        (2_500 * SECOND, "usage-2"),
        (3_500 * SECOND, "usage-3"),
    ];

    for (ts_nanos, event_id_str) in usage_points {
        let ts = UtcTimestamp::from_unix_nanos(ts_nanos);
        let ev_id = event_store::insert_event(
            &conn,
            &NewUsageEvent {
                canonical_event_id: event_id_str,
                session_id: Some("session-1"),
                event_timestamp: Some(ts),
                model_id: Some("claude-3-5-sonnet"),
                evidence_kind: "transcript",
                source_provenance: "test",
                parser_version: "v1",
                created_at: ts,
            },
        )
        .unwrap();

        component_store::insert_component(
            &conn,
            &NewUsageComponent {
                event_id: ev_id,
                token_class: "input",
                count: 1_000_000,
            },
        )
        .unwrap();
    }

    // Write empty aub.toml
    let config_file = state.path().join("aub.toml");
    std::fs::write(&config_file, "").unwrap();
}

#[test]
fn test_calibrate_fit_release_binary_quantized_success() {
    let state = StateDir::new();
    seed_successful_experiment_ledger(&state);

    let output = aub()
        .args(["calibrate", "fit"])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(
        output.status.code(),
        Some(0),
        "fit must succeed with exit status 0.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify stdout reports all required fields
    assert!(stdout.contains("Candidate ID:"), "must report Candidate ID");
    assert!(stdout.contains("Fitted:"), "must report Fitted coefficient");
    assert!(stdout.contains("Capacity:"), "must report Capacity");
    assert!(stdout.contains("Residual:"), "must report Residual");
    assert!(stdout.contains("Lag Handling:"), "must report Lag Handling");
    assert!(
        stdout.contains("Uncertainty:"),
        "must report Uncertainty interval"
    );
    assert!(
        stdout.contains("Usable Obs:"),
        "must report Usable Obs count"
    );
    assert!(stdout.contains("Method:"), "must report Method");
    assert!(stdout.contains("Parameters:"), "must report Parameters");

    // The seeded coefficient is 100 micros per point (slope = 10,000 ppm / credit)
    assert!(
        stdout.contains("100 micros/point"),
        "fitted coefficient must match expected slope: {stdout}"
    );

    // Verify candidate row is persisted in database
    let conn = open_test_ledger(&state);
    let candidate_row: (String, i64, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT candidate_id, fitted_micros_per_point, equivalent_full_window_capacity_micros,
                    uncertainty_low_micros, uncertainty_high_micros, sample_count
             FROM window_calibration_candidate LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("candidate row must exist in ledger");

    let (cand_id, fitted_micros, capacity_micros, unc_low, unc_high, sample_count) = candidate_row;
    assert_eq!(fitted_micros, 100);
    assert_eq!(capacity_micros, 100_000_000);
    assert_eq!(sample_count, 4);

    // Seeded coefficient is within the reported uncertainty interval
    assert!(
        unc_low <= fitted_micros && fitted_micros <= unc_high,
        "seeded coefficient {fitted_micros} must be within uncertainty interval [{unc_low}, {unc_high}]"
    );

    // Immutability: database triggers reject update and delete
    let update_res = calibration_store::try_update_candidate(&conn, &CandidateId::new(&cand_id));
    assert!(
        update_res.unwrap_err().to_string().contains("immutable"),
        "candidate must be immutable"
    );

    let delete_res = calibration_store::try_delete_candidate(&conn, &CandidateId::new(&cand_id));
    assert!(
        delete_res.unwrap_err().to_string().contains("immutable"),
        "candidate must be immutable"
    );

    // Never activated: calibration_lifecycle has 0 rows
    let lifecycle_count = calibration_store::count_calibration_lifecycles(&conn).unwrap();
    assert_eq!(
        lifecycle_count, 0,
        "candidate must never be activated by fit"
    );

    // Reproducibility: rerunning the fitter produces identical inputs_hash and identical coefficient
    let output2 = aub()
        .args(["calibrate", "fit", "--format", "json"])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("second aub binary run must succeed");

    assert_eq!(output2.status.code(), Some(0));
    let json2: serde_json::Value =
        serde_json::from_slice(&output2.stdout).expect("output must be valid json");

    assert_eq!(json2["candidate_id"], cand_id);
    assert_eq!(json2["fitted_micros_per_point"], 100);
    assert_eq!(json2["sample_count"], 4);
}

#[test]
fn test_calibrate_fit_release_binary_json_contract() {
    let state = StateDir::new();
    seed_successful_experiment_ledger(&state);

    let output = aub()
        .args(["calibrate", "fit", "--format", "json"])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output must be valid json");

    assert!(json["candidate_id"].is_string());
    assert!(json["experiment_id"].is_string());
    assert_eq!(json["provider"], "anthropic");
    assert_eq!(json["plan_tier"], "test-tier");
    assert_eq!(json["window_semantic_key"], "seven_day");
    assert_eq!(json["fitted_micros_per_point"], 100);
    assert_eq!(json["equivalent_full_window_capacity_micros"], 100_000_000);
    assert_eq!(json["residual_percentage_points"], 0.0);
    assert!(json["uncertainty_low_micros"].is_number());
    assert!(json["uncertainty_high_micros"].is_number());
    assert_eq!(json["lag_handling"], "settled-boundary-cancellation");
    assert_eq!(json["statistical_method"], "theil-sen-huber-interval");
    assert!(json["statistical_parameters"].is_string());
    assert_eq!(json["usable_observations"], 4);
    assert_eq!(json["sample_count"], 4);
    assert!(json["inputs_digest"].is_string());
    assert!(json["excluded_samples"].is_array());
    assert!(json["diagnostic_findings"].is_array());
}

#[test]
fn test_calibrate_fit_release_binary_rejected_insufficient_evidence() {
    let state = StateDir::new();
    // Ledger with migrations applied but no calibration experiment or observations
    let _conn = open_test_ledger(&state);
    let config_file = state.path().join("aub.toml");
    std::fs::write(&config_file, "").unwrap();

    let output = aub()
        .args(["calibrate", "fit"])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", &config_file)
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Brief requirement: "rejected fit with typed rejection reason (InsufficientEvidence, exit status 6)"
    assert_eq!(
        output.status.code(),
        Some(6),
        "insufficient evidence must exit with code 6, got {:?}.\nstderr: {stderr}",
        output.status.code()
    );

    assert!(
        stderr.contains("no calibration experiment")
            || stderr.contains("insufficient")
            || stderr.contains("fit rejected"),
        "stderr must name typed rejection reason, got: {stderr}"
    );
}
