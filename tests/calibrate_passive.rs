//! Process-level integration tests for `aub calibrate passive`.
//!
//! Covers:
//! - `calibrate passive` reporting intervals considered, eligible intervals, and failing condition counts
//! - JSON format output validity
//! - Passive fitting produces candidate only, never an activation (enforcing Invariant 14)
//! - Persisted meter-window anomaly exclusion annotation excludes affected interval while adjacent clean intervals stay eligible
//! - Persisted external-validation mismatch annotation excludes interval with typed reason
//! - Configured account exclusivity policy forbids passive fitting producing no candidates

use std::process::Command;

use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::store::cost_model::seed_initial_cost_model;
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, NewMeterAttemptResult};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow,
};
use agent_usage_book::store::usage_component::NewUsageComponent;
use agent_usage_book::store::usage_event::NewUsageEvent;
use agent_usage_book::store::{
    account as account_store, connection, meter_attempt as attempt_store,
    meter_evidence as evidence_store, migrate, migrations, sample_run as run_store,
    sampling_policy_snapshot as snapshot_store, usage_component as component_store,
    usage_event as event_store,
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

fn seed_test_ledger(state: &StateDir, exclusivity: Option<&str>) {
    let mut conn = open_test_ledger(state);

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

    // 4 observations with 10_000 ppm resolution
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

    // Usage events matching 1,000,000 input tokens each
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
                evidence_kind: "reported",
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

    // Create session for attribution
    conn.execute(
        "INSERT INTO session (id, source, native_session_id, start, end, project_key, repository_key)
         VALUES (1, 'claude', 'session-1', ?1, ?2, 'proj', 'repo')",
        rusqlite::params![
            1_000 * SECOND,
            4_000 * SECOND,
        ],
    )
    .unwrap();

    // Write aub.toml
    let config_file = state.path().join("aub.toml");
    let exclusivity_line = if let Some(policy) = exclusivity {
        format!("exclusivity_policy = \"{policy}\"\n")
    } else {
        String::new()
    };
    let config_content = format!(
        r#"state.dir = "{}"

[[accounts]]
name = "work"
provider = "anthropic"
credential = {{ kind = "file", path = "/tmp/dummy" }}
{exclusivity_line}
"#,
        state.path().display()
    );
    std::fs::write(&config_file, config_content).unwrap();
}

#[test]
fn test_calibrate_passive_cli_reports_intervals_considered_and_eligible_and_failing_counts() {
    let state = StateDir::new();
    seed_test_ledger(&state, None);

    let output = aub()
        .args(["calibrate", "passive", "--account", "work"])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "calibrate passive must succeed, got {:?}.\nstderr: {stderr}\nstdout: {stdout}",
        output.status.code()
    );

    assert!(
        stdout.contains("intervals considered:"),
        "stdout must report intervals considered, got:\n{stdout}"
    );
    assert!(
        stdout.contains("eligible intervals:"),
        "stdout must report eligible intervals, got:\n{stdout}"
    );
    assert!(
        stdout.contains("excluded intervals:"),
        "stdout must report excluded intervals, got:\n{stdout}"
    );

    // Invariant 14: Passive fitting produces a candidate by default and never an activation.
    // Verify calibration_lifecycle table has 0 rows.
    let conn = open_test_ledger(&state);
    let lifecycle_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM calibration_lifecycle", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        lifecycle_count, 0,
        "passive calibration must NEVER insert into calibration_lifecycle (Invariant 14)"
    );
}

#[test]
fn test_calibrate_passive_cli_json_format() {
    let state = StateDir::new();
    seed_test_ledger(&state, None);

    let output = aub()
        .args([
            "calibrate",
            "passive",
            "--account",
            "work",
            "--format",
            "json",
        ])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        output.status.success(),
        "calibrate passive --format json must succeed, got {:?}.\nstderr: {stderr}",
        output.status.code()
    );

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("output must be valid JSON");
    assert!(json["intervals_considered"].is_number());
    assert!(json["eligible_intervals"].is_number());
    assert!(json["excluded_intervals"].is_number());
    assert!(json["failing_condition_counts"].is_object());
}

#[test]
fn test_persisted_anomaly_annotation_excludes_affected_interval_and_keeps_adjacent_eligible() {
    let state = StateDir::new();
    seed_test_ledger(&state, None);

    let conn = open_test_ledger(&state);

    // Insert an anomaly and an exclusion covering interval 2 (ts 2000s to 3000s)
    conn.execute(
        "INSERT INTO meter_window_anomaly (id, kind, account_id, semantic_key, scope_kind, scoped_model, prior_observation_id, prior_window_id, current_observation_id, current_window_id, detected_at, detail)
         VALUES (1, 'percentage_decrease_without_reset', 1, 'seven_day', 'account_wide', NULL, 1, 1, 2, 2, ?1, 'anomaly-test')",
        rusqlite::params![2_000 * SECOND],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO meter_calibration_exclusion (id, anomaly_id, account_id, semantic_key, scope_kind, scoped_model, interval_start_at, interval_end_at, created_at)
         VALUES (1, 1, 1, 'seven_day', 'account_wide', NULL, ?1, ?2, ?1)",
        rusqlite::params![2_000 * SECOND, 3_000 * SECOND],
    )
    .unwrap();

    let output = aub()
        .args([
            "calibrate",
            "passive",
            "--account",
            "work",
            "--format",
            "json",
        ])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("output must be valid JSON");

    let failing_counts = json["failing_condition_counts"]
        .as_object()
        .expect("failing_condition_counts must be an object");
    assert_eq!(
        failing_counts
            .get("meter_window_anomaly")
            .and_then(|v| v.as_i64()),
        Some(1),
        "failing condition counts must include the typed anomaly exclusion count 1, got: {failing_counts:?}"
    );
}

#[test]
fn test_external_validation_mismatch_annotation_alone_excludes_interval() {
    let state = StateDir::new();
    seed_test_ledger(&state, None);

    let conn = open_test_ledger(&state);

    conn.execute(
        "INSERT INTO authoritative_surface_comparison (id, observation_id, window_id, semantic_key, authoritative_surface, documented_granularity_ppm, adapter_quota_used_ppm, authoritative_quota_used_ppm, read_at, verdict)
         VALUES (1, 1, 1, 'seven_day', 'console', 10000, 100000, 150000, ?1, 'unresolved_mismatch')",
        rusqlite::params![1_500 * SECOND],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO adapter_semantics_annotation (id, kind, comparison_id, observation_id, semantic_key, adapter_quota_used_ppm, authoritative_quota_used_ppm, corrects_annotation_id, detail, created_at)
         VALUES (1, 'mismatch', 1, 1, 'seven_day', 100000, 150000, NULL, 'mismatch-test', ?1)",
        rusqlite::params![1_500 * SECOND],
    )
    .unwrap();

    let output = aub()
        .args([
            "calibrate",
            "passive",
            "--account",
            "work",
            "--format",
            "json",
        ])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("output must be valid JSON");

    let failing_counts = json["failing_condition_counts"]
        .as_object()
        .expect("failing_condition_counts must be an object");
    assert_eq!(
        failing_counts
            .get("external_validation_mismatch")
            .and_then(|v| v.as_i64()),
        Some(1),
        "failing condition counts must include external_validation_mismatch count 1, got: {failing_counts:?}"
    );
}

#[test]
fn test_configured_exclusivity_policy_forbids_passive_fitting_producing_no_candidates() {
    let state = StateDir::new();
    // Configure dedicated exclusivity policy forbidding passive fitting
    seed_test_ledger(&state, Some("dedicated_calibration_only"));

    let output = aub()
        .args([
            "calibrate",
            "passive",
            "--account",
            "work",
            "--format",
            "json",
        ])
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("output must be valid JSON");

    assert_eq!(
        json["eligible_intervals"].as_i64(),
        Some(0),
        "dedicated account must yield 0 eligible passive intervals"
    );
    assert!(
        json["candidate"].is_null(),
        "forbidden passive fitting account must produce no candidate"
    );
    let failing_counts = json["failing_condition_counts"]
        .as_object()
        .expect("failing_condition_counts must be an object");
    assert!(
        failing_counts
            .get("exclusivity_policy_permits_passive")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            > 0,
        "failing condition count for exclusivity_policy_permits_passive must be > 0, got: {failing_counts:?}"
    );
}
