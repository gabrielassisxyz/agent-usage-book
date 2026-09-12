//! Process-level integration tests for `aub calibrate promote`.
//!
//! Covers the path the calibration design was missing: a candidate fitted by
//! `aub calibrate fit` becomes a `window_calibration_result` carrying validation
//! evidence, and only then can `aub calibrate activate` accept it. No fixture
//! command appears anywhere in that chain.
//!
//! - Promotion records a result and never activates it (invariant 14)
//! - The promoted result activates on its own recorded figures
//! - Empty validation, overlapping sets and a second promotion are refused by name

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
use agent_usage_book::store::calibration::{CalibrationExperiment, ExperimentId, PlanTier};
use agent_usage_book::store::cost_model::{ProviderKey, ValidityInterval, seed_initial_cost_model};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, NewMeterAttemptResult};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow,
};
use agent_usage_book::store::usage_component::NewUsageComponent;
use agent_usage_book::store::usage_event::NewUsageEvent;
use agent_usage_book::store::{
    account as account_store, calibration as calibration_store, connection, meter_attempt,
    meter_evidence as evidence_store, migrate, migrations, sample_run as run_store,
    sampling_policy_snapshot as snapshot_store, usage_component as component_store,
    usage_event as event_store,
};
use rusqlite::Connection;
use test_support::StateDir;

const SECOND: i64 = 1_000_000_000;
const EXPERIMENT_ID: &str = "exp-promote-1";

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

fn test_experiment() -> CalibrationExperiment {
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
        id: ExperimentId::new(EXPERIMENT_ID),
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("test-tier"),
        window_semantic_key: WindowSemanticKey::new("seven_day"),
        meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
        billing_semantics_id: BillingSemanticsId::new("billing-v1"),
        settlement_policy: policy,
        // The experiment closes at 100_000s; the validation readings are
        // recorded after it, which is what keeps them out of the fit.
        validity: ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(1_000 * SECOND),
            UtcTimestamp::from_unix_nanos(100_000 * SECOND),
        )
        .unwrap(),
        knowledge_time: UtcTimestamp::from_unix_nanos(100_000 * SECOND),
    }
}

struct Seeding {
    account_id: agent_usage_book::store::account::AccountId,
    run_id: agent_usage_book::store::sample_run::SampleRunId,
    snapshot_id: agent_usage_book::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
}

fn insert_reading(conn: &Connection, seeding: &Seeding, at_nanos: i64, used_ppm: i32, tag: &str) {
    let ts = UtcTimestamp::from_unix_nanos(at_nanos);
    let attempt_id = meter_attempt::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: seeding.run_id,
            account_id: seeding.account_id,
            provider: "anthropic".into(),
            request_started_at: ts,
            credential_context_id: None,
            policy_snapshot_id: seeding.snapshot_id,
            due_at: ts,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "contract-v1".into(),
            meter_semantics_id: "semantics-v1".into(),
        },
    )
    .unwrap();

    meter_attempt::record_meter_attempt_result(
        conn,
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
        conn,
        &NewMeterResponseEvidence {
            attempt_id,
            response_classification: "200".into(),
            received_at: ts,
            provider_observed_at_original: None,
            evidence_capsule: format!("{{\"hash\":\"{tag}\"}}"),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "san-v1".into(),
            capture_truncated: false,
        },
    )
    .unwrap();

    let observation_row_id = evidence_store::insert_observation(
        conn,
        &NewMeterObservation {
            attempt_id,
            evidence_id: evidence_row_id,
            account_id: seeding.account_id,
            provider: "anthropic".into(),
            provider_observed_at: Some(ts),
            received_at: ts,
            measurement_basis: agent_usage_book::domain::time::MeasurementBasis::ProviderObserved,
            observed_plan: Some("test-tier".into()),
            observed_tier: Some("test-tier".into()),
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("contract-v1"),
            meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
            normalized_fingerprint: format!("fp-{tag}"),
        },
    )
    .unwrap();

    evidence_store::insert_window(
        conn,
        &NewMeterWindow {
            observation_id: observation_row_id,
            semantic_key: WindowSemanticKey::new("seven_day"),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::RoundedToNearest,
            resets_at: UtcTimestamp::from_unix_nanos(at_nanos + 86_400 * SECOND).into(),
            nominal_duration: NominalWindowDuration::from_nanos(7 * 86_400 * 1_000_000_000),
        },
    )
    .unwrap();
}

fn insert_spend(conn: &Connection, at_nanos: i64, event_id: &str) {
    let ts = UtcTimestamp::from_unix_nanos(at_nanos);
    let ev_id = event_store::insert_event(
        conn,
        &NewUsageEvent {
            canonical_event_id: event_id,
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

    // 1,000,000 input tokens is 3.0 credits under anthropic-claude-messages-v1.
    component_store::insert_component(
        conn,
        &NewUsageComponent {
            event_id: ev_id,
            token_class: "input",
            count: 1_000_000,
        },
    )
    .unwrap();
}

/// Seeds one experiment whose four readings move 10,000 ppm per credit (100
/// micros per point), then three further readings after the experiment closed:
/// the held-out series promotion validates against. `holdout_step_ppm` is how
/// far the meter moves per 3 credits of held-out spend, so a caller can seed a
/// series the fitted coefficient does not predict.
fn seed_ledger(state: &StateDir, holdout_step_ppm: i32) {
    let mut conn = open_test_ledger(state);

    calibration_store::insert_experiment(&conn, &test_experiment()).unwrap();
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
    let seeding = Seeding {
        account_id,
        run_id,
        snapshot_id,
    };

    for (ts_nanos, used_ppm, tag) in [
        (1_000 * SECOND, 100_000, "ev-fit-1"),
        (2_000 * SECOND, 130_000, "ev-fit-2"),
        (3_000 * SECOND, 160_000, "ev-fit-3"),
        (4_000 * SECOND, 190_000, "ev-fit-4"),
    ] {
        insert_reading(&conn, &seeding, ts_nanos, used_ppm, tag);
    }
    for (ts_nanos, event_id) in [
        (1_500 * SECOND, "usage-fit-1"),
        (2_500 * SECOND, "usage-fit-2"),
        (3_500 * SECOND, "usage-fit-3"),
    ] {
        insert_spend(&conn, ts_nanos, event_id);
    }

    for (ts_nanos, used_ppm, tag) in [
        (110_000 * SECOND, 190_000, "ev-holdout-1"),
        (120_000 * SECOND, 190_000 + holdout_step_ppm, "ev-holdout-2"),
        (
            130_000 * SECOND,
            190_000 + 2 * holdout_step_ppm,
            "ev-holdout-3",
        ),
    ] {
        insert_reading(&conn, &seeding, ts_nanos, used_ppm, tag);
    }
    for (ts_nanos, event_id) in [
        (115_000 * SECOND, "usage-holdout-1"),
        (125_000 * SECOND, "usage-holdout-2"),
    ] {
        insert_spend(&conn, ts_nanos, event_id);
    }

    std::fs::write(state.path().join("aub.toml"), "").unwrap();
}

/// The content hashes of the seeded readings, in observation-time order: the
/// evidence ids the operator names on the command line.
fn evidence_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT re.content_hash
             FROM meter_observation mo
             JOIN meter_response_evidence re ON re.id = mo.evidence_id
             ORDER BY mo.received_at ASC, mo.id ASC",
        )
        .unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

fn run_aub(state: &StateDir, args: &[&str]) -> std::process::Output {
    aub()
        .args(args)
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .current_dir(state.path())
        .output()
        .expect("aub binary must run")
}

struct Seeded {
    state: StateDir,
    candidate_id: String,
    training: String,
    validation: String,
}

/// Seeds the ledger and runs the real `aub calibrate fit`, returning the
/// candidate it recorded and the two evidence sets promotion will be given.
fn seed_and_fit() -> Seeded {
    seed_and_fit_with_holdout(30_000)
}

/// The same chain over a held-out series whose meter moves `holdout_step_ppm`
/// per three credits, so a caller can seed evidence the fit does not predict.
fn seed_and_fit_with_holdout(holdout_step_ppm: i32) -> Seeded {
    let state = StateDir::new();
    seed_ledger(&state, holdout_step_ppm);

    let output = run_aub(&state, &["calibrate", "fit", "--experiment", EXPERIMENT_ID]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "fit must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidate_id = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("Candidate ID: ").map(str::to_string))
        .expect("fit must report a candidate id");

    let conn = open_test_ledger(&state);
    let ids = evidence_ids(&conn);
    assert_eq!(ids.len(), 7, "four fitted readings and three held out");
    drop(conn);

    Seeded {
        training: ids[..4].join(","),
        validation: ids[4..].join(","),
        candidate_id,
        state,
    }
}

fn lifecycle_count(state: &StateDir) -> i64 {
    let conn = open_test_ledger(state);
    calibration_store::count_calibration_lifecycles(&conn).unwrap()
}

#[test]
fn promote_records_a_result_and_activation_then_accepts_it() {
    let seeded = seed_and_fit();
    let state = &seeded.state;

    let promote = run_aub(
        state,
        &[
            "calibrate",
            "promote",
            &seeded.candidate_id,
            "--training",
            &seeded.training,
            "--validation",
            &seeded.validation,
        ],
    );
    let stdout = String::from_utf8_lossy(&promote.stdout).into_owned();
    assert_eq!(
        promote.status.code(),
        Some(0),
        "promote must succeed: {}",
        String::from_utf8_lossy(&promote.stderr)
    );
    let result_id = format!("promoted-{}", seeded.candidate_id);
    assert!(
        stdout.contains(&result_id),
        "promote must name the result it recorded: {stdout}"
    );
    assert!(
        stdout.contains("not activated"),
        "promote must say it activated nothing: {stdout}"
    );

    // The row is a real result, carrying the validation half a candidate lacks.
    let conn = open_test_ledger(state);
    let (fitted, held_out, method, policy): (i64, Option<i64>, String, String) = conn
        .query_row(
            "SELECT fitted_micros_per_point, out_of_sample_residual_micros, validation_method,
                    activation_policy_version
             FROM window_calibration_result WHERE calibration_id = ?1",
            [&result_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("the promoted result row must exist");
    assert_eq!(
        fitted, 100,
        "the result carries the candidate's coefficient"
    );
    assert_eq!(
        held_out,
        Some(0),
        "the held-out series has the fitted physics, so its residual is zero"
    );
    assert_eq!(method, "held-out-interval-residual");
    assert_eq!(policy, "promote-v1");
    drop(conn);

    // Invariant 14: promotion records evidence, it does not activate.
    assert_eq!(
        lifecycle_count(state),
        0,
        "promote must write no lifecycle event"
    );

    // `calibrate history` lists it, with no lifecycle event yet.
    let history = run_aub(state, &["calibrate", "history", "--format", "json"]);
    assert_eq!(history.status.code(), Some(0));
    let history_json: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    let entries = history_json["entries"].as_array().unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry["calibration_id"] == result_id.as_str())
        .expect("history must list the promoted result");
    assert!(
        entry["events"].as_array().unwrap().is_empty(),
        "history must show no lifecycle event before activation"
    );

    // The result activates on its own recorded figures, with no fixture in the path.
    let activate = run_aub(
        state,
        &[
            "calibrate",
            "activate",
            &result_id,
            "--actor",
            "operator",
            "--training",
            &seeded.training,
            "--validation",
            &seeded.validation,
            "--max-residual-micros",
            "1000",
        ],
    );
    assert_eq!(
        activate.status.code(),
        Some(0),
        "the promoted result must activate: {}",
        String::from_utf8_lossy(&activate.stderr)
    );
    assert_eq!(
        lifecycle_count(state),
        1,
        "activation adds exactly one lifecycle event"
    );
}

#[test]
fn promote_json_reports_the_validation_figures_and_that_nothing_was_activated() {
    let seeded = seed_and_fit();
    let output = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "promote",
            &seeded.candidate_id,
            "--training",
            &seeded.training,
            "--validation",
            &seeded.validation,
            "--format",
            "json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["command"], "calibrate-promote");
    assert_eq!(
        json["result_id"],
        format!("promoted-{}", seeded.candidate_id)
    );
    assert_eq!(json["candidate_id"], seeded.candidate_id);
    assert_eq!(json["experiment_id"], EXPERIMENT_ID);
    assert_eq!(json["fitted"]["value"], "100");
    assert_eq!(json["fitted"]["unit"], "micros_per_point");
    assert_eq!(json["held_out_residual"]["value"], "0");
    assert_eq!(json["held_out_residual"]["unit"], "credits");
    assert_eq!(json["validation_observations"], 3);
    assert_eq!(json["validation_method"], "held-out-interval-residual");
    assert_eq!(json["validation_version"], "v1");
    assert_eq!(json["activation_policy_version"], "promote-v1");
    assert_eq!(json["activated"], false);
    assert!(json["fitting_evidence_digest"].is_string());
    assert!(json["validation_evidence_digest"].is_string());
}

#[test]
fn a_second_promotion_of_one_candidate_is_refused_naming_the_existing_result() {
    let seeded = seed_and_fit();
    let args = [
        "calibrate",
        "promote",
        &seeded.candidate_id,
        "--training",
        &seeded.training,
        "--validation",
        &seeded.validation,
    ];
    assert_eq!(run_aub(&seeded.state, &args).status.code(), Some(0));

    let second = run_aub(&seeded.state, &args);
    let stderr = String::from_utf8_lossy(&second.stderr).into_owned();
    assert_ne!(
        second.status.code(),
        Some(0),
        "a second promotion must fail"
    );
    assert!(
        stderr.contains(&format!("promoted-{}", seeded.candidate_id)),
        "the refusal must name the existing result: {stderr}"
    );

    let conn = open_test_ledger(&seeded.state);
    let results: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_calibration_result",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(results, 1, "one fit has exactly one result identity");
}

#[test]
fn overlapping_training_and_validation_evidence_is_refused_naming_the_overlap() {
    let seeded = seed_and_fit();
    let shared = seeded.training.split(',').next().unwrap().to_string();
    let output = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "promote",
            &seeded.candidate_id,
            "--training",
            &seeded.training,
            "--validation",
            &format!("{},{shared}", seeded.validation),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.contains(&shared),
        "the refusal must name the overlapping evidence id: {stderr}"
    );
    assert_eq!(
        lifecycle_count(&seeded.state),
        0,
        "a refused promotion writes nothing"
    );
}

#[test]
fn an_empty_validation_set_is_refused() {
    let seeded = seed_and_fit();
    let output = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "promote",
            &seeded.candidate_id,
            "--training",
            &seeded.training,
            "--validation",
            ",,",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.contains("--validation"),
        "the refusal must name the flag that was empty: {stderr}"
    );

    let conn = open_test_ledger(&seeded.state);
    let results: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_calibration_result",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(results, 0, "a refused promotion records no result");
}

#[test]
fn a_training_set_that_is_not_the_fitted_evidence_is_refused() {
    let seeded = seed_and_fit();
    let partial = seeded
        .training
        .split(',')
        .take(3)
        .collect::<Vec<_>>()
        .join(",");
    let output = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "promote",
            &seeded.candidate_id,
            "--training",
            &partial,
            "--validation",
            &seeded.validation,
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_ne!(
        output.status.code(),
        Some(0),
        "a training set that is not the candidate's own evidence must be refused"
    );
    assert!(
        stderr.contains("--training"),
        "the refusal must name the flag: {stderr}"
    );
}

#[test]
fn an_unknown_candidate_is_refused_by_name() {
    let seeded = seed_and_fit();
    let output = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "promote",
            "cand-does-not-exist",
            "--training",
            &seeded.training,
            "--validation",
            &seeded.validation,
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.contains("cand-does-not-exist"),
        "the refusal must name the candidate asked for: {stderr}"
    );
}

/// The exit criterion the whole validation half exists for: a candidate that
/// fits its own training evidence and fails held-out evidence is promoted, so
/// the failure is recorded rather than hidden, and then refused by activation
/// on the residual that promotion computed.
#[test]
fn a_candidate_that_fails_its_held_out_evidence_cannot_be_activated() {
    // Half the fitted movement per credit: the coefficient over-predicts the
    // held-out series, and the residual is the size of that error.
    let seeded = seed_and_fit_with_holdout(15_000);
    let promote = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "promote",
            &seeded.candidate_id,
            "--training",
            &seeded.training,
            "--validation",
            &seeded.validation,
            "--format",
            "json",
        ],
    );
    assert_eq!(
        promote.status.code(),
        Some(0),
        "promotion records the failure: {}",
        String::from_utf8_lossy(&promote.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&promote.stdout).unwrap();
    let residual: i64 = json["held_out_residual"]["value"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        residual > 100_000,
        "a series moving half as fast must leave a large held-out residual, got {residual}"
    );

    let result_id = format!("promoted-{}", seeded.candidate_id);
    let activate = run_aub(
        &seeded.state,
        &[
            "calibrate",
            "activate",
            &result_id,
            "--actor",
            "operator",
            "--training",
            &seeded.training,
            "--validation",
            &seeded.validation,
            "--max-residual-micros",
            "1000",
        ],
    );
    let stderr = String::from_utf8_lossy(&activate.stderr).into_owned();
    assert_ne!(
        activate.status.code(),
        Some(0),
        "activation must refuse a result whose held-out residual exceeds the policy"
    );
    assert!(
        stderr.contains("held-out") || stderr.contains("residual"),
        "the refusal must name the held-out residual: {stderr}"
    );
    assert_eq!(
        lifecycle_count(&seeded.state),
        0,
        "a refused activation writes no lifecycle event"
    );
}
