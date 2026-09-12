//! Process-level tests for the multivariate path of `aub calibrate fit`
//! (`aub-73xa`): the premise recorded at `begin` decides the fitter, the
//! joint candidate is recorded once and never activated, and a design that
//! cannot separate the kinds is refused by name with nothing recorded.
//!
//! Both arm fixtures live under `tests/fixtures/calibration/`. The
//! independent one is the four-arm, three-magnitude design the controlled
//! burst will run; the proportional one is the shape organic traffic has.

use std::process::Command;

use agent_usage_book::calibration::contamination::ContaminationThresholds;
use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
use agent_usage_book::domain::provenance::CostModelId;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::tokens::TokenKind;
use agent_usage_book::domain::window::{
    NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    WindowSemanticKey,
};
use agent_usage_book::store::calibration::PlanTier;
use agent_usage_book::store::calibration_controlled::{
    ControlledExperimentId, ControlledExperimentRun, insert_begin, record_end,
};
use agent_usage_book::store::cost_model::ProviderKey;
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, NewMeterAttemptResult};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, ObservationRowId,
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
const EXPERIMENT: &str = "exp-multivariate";
const ACCOUNT: &str = "bianca";
const WINDOW: &str = "five_hour";
const BASELINE_PPM: i64 = 100_000;

const INDEPENDENT_ARMS: &str =
    include_str!("fixtures/calibration/multivariate-independent-arms.json");
const PROPORTIONAL_ARMS: &str =
    include_str!("fixtures/calibration/multivariate-proportional-arms.json");

struct ArmBlock {
    counts: [(TokenKind, u64); 4],
}

struct ArmFixture {
    truth_ppm_per_token: [(TokenKind, f64); 4],
    blocks: Vec<ArmBlock>,
}

fn kind_field(value: &serde_json::Value, kind: TokenKind) -> &serde_json::Value {
    &value[kind.label()]
}

fn parse_fixture(text: &str) -> ArmFixture {
    let json: serde_json::Value = serde_json::from_str(text).expect("fixture must be JSON");
    let truth = &json["truth_ppm_per_token"];
    let blocks = json["blocks"]
        .as_array()
        .expect("fixture blocks must be an array")
        .iter()
        .map(|block| ArmBlock {
            counts: TokenKind::ALL.map(|kind| {
                (
                    kind,
                    kind_field(block, kind)
                        .as_u64()
                        .expect("a block count must be a non-negative integer"),
                )
            }),
        })
        .collect();
    ArmFixture {
        truth_ppm_per_token: TokenKind::ALL.map(|kind| {
            (
                kind,
                kind_field(truth, kind)
                    .as_f64()
                    .expect("a truth coefficient must be a number"),
            )
        }),
        blocks,
    }
}

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

struct MeterChain {
    account_id: agent_usage_book::store::account::AccountId,
    run_id: agent_usage_book::store::sample_run::SampleRunId,
    snapshot_id: agent_usage_book::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
}

fn meter_chain(conn: &Connection) -> MeterChain {
    let at = UtcTimestamp::from_unix_nanos(100 * SECOND);
    let account_id = account_store::observe_account(conn, "anthropic", ACCOUNT, at).unwrap();
    let run_id = run_store::start_sample_run(conn, run_store::Trigger::Manual, at, "seed").unwrap();
    let snapshot_id = snapshot_store::resolve_policy_snapshot(
        conn,
        account_id,
        at,
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
    MeterChain {
        account_id,
        run_id,
        snapshot_id,
    }
}

fn reading(
    conn: &Connection,
    chain: &MeterChain,
    at_nanos: i64,
    used_ppm: i64,
) -> ObservationRowId {
    let ts = UtcTimestamp::from_unix_nanos(at_nanos);
    let attempt_id = attempt_store::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: chain.run_id,
            account_id: chain.account_id,
            provider: "anthropic".into(),
            request_started_at: ts,
            credential_context_id: None,
            policy_snapshot_id: chain.snapshot_id,
            due_at: ts,
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "contract-v1".into(),
            meter_semantics_id: "semantics-v1".into(),
        },
    )
    .unwrap();
    attempt_store::record_meter_attempt_result(
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
    let evidence_id = evidence_store::insert_response_evidence(
        conn,
        &NewMeterResponseEvidence {
            attempt_id,
            response_classification: "200".into(),
            received_at: ts,
            provider_observed_at_original: None,
            evidence_capsule: format!("{{\"at\":{at_nanos},\"used\":{used_ppm}}}"),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "san-v1".into(),
            capture_truncated: false,
        },
    )
    .unwrap();
    let observation_id = evidence_store::insert_observation(
        conn,
        &NewMeterObservation {
            attempt_id,
            evidence_id,
            account_id: chain.account_id,
            provider: "anthropic".into(),
            provider_observed_at: Some(ts),
            received_at: ts,
            measurement_basis: agent_usage_book::domain::time::MeasurementBasis::ProviderObserved,
            observed_plan: Some("max-5x".into()),
            observed_tier: Some("max-5x".into()),
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("contract-v1"),
            meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
            normalized_fingerprint: format!("fp-{at_nanos}"),
        },
    )
    .unwrap();
    evidence_store::insert_window(
        conn,
        &NewMeterWindow {
            observation_id,
            semantic_key: WindowSemanticKey::new(WINDOW),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(
                QuotaFractionPpm::new(i32::try_from(used_ppm).unwrap()).unwrap(),
            ),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::Exact,
            resets_at: UtcTimestamp::from_unix_nanos(at_nanos + 18_000 * SECOND).into(),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        },
    )
    .unwrap();
    observation_id
}

fn spend(conn: &Connection, at_nanos: i64, index: usize, block: &ArmBlock) {
    let ts = UtcTimestamp::from_unix_nanos(at_nanos);
    let event_id = event_store::insert_event(
        conn,
        &NewUsageEvent {
            canonical_event_id: &format!("burst-block-{index}"),
            session_id: Some("burst-session"),
            event_timestamp: Some(ts),
            model_id: Some("claude-opus"),
            evidence_kind: "transcript",
            source_provenance: "test",
            parser_version: "v1",
            created_at: ts,
        },
    )
    .unwrap();
    for (kind, count) in block.counts {
        component_store::insert_component(
            conn,
            &NewUsageComponent {
                event_id,
                token_class: kind.label(),
                count,
            },
        )
        .unwrap();
    }
}

/// Seeds a whole controlled burst from an arm fixture: the baseline reading,
/// `begin` with the given premise, then per block one usage event followed
/// by a lagging reading and two settled ones, then `end`.
fn seed_burst(state: &StateDir, fixture: &ArmFixture, expected_kinds: Vec<TokenKind>) {
    let conn = open_test_ledger(state);
    let chain = meter_chain(&conn);
    let t0 = 1_000 * SECOND;
    let baseline = reading(&conn, &chain, t0, BASELINE_PPM);
    let run = ControlledExperimentRun {
        id: ControlledExperimentId::new(EXPERIMENT),
        account: ACCOUNT.into(),
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("max-5x"),
        window_semantic_key: WindowSemanticKey::new(WINDOW),
        cost_model_id: CostModelId::new("anthropic-claude-messages-v1"),
        expected_token_kinds: expected_kinds,
        baseline_observation_id: baseline,
        baseline_quota_used: QuotaUsed::new(
            QuotaFractionPpm::new(i32::try_from(BASELINE_PPM).unwrap()).unwrap(),
        ),
        baseline_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
            .unwrap(),
        baseline_observed_at: UtcTimestamp::from_unix_nanos(t0),
        baseline_plateau_started_at: UtcTimestamp::from_unix_nanos(t0),
        contamination_thresholds: ContaminationThresholds::conservative_default(),
        started_at: UtcTimestamp::from_unix_nanos(t0 + SECOND),
        ended_at: None,
        exclusivity_assertion: format!("account {ACCOUNT} reserved for {EXPERIMENT}"),
    };
    insert_begin(&conn, &run).unwrap();

    let mut cumulative_ppm = BASELINE_PPM as f64;
    let mut t = t0 + 60 * SECOND;
    for (index, block) in fixture.blocks.iter().enumerate() {
        spend(&conn, t, index, block);
        let movement: f64 = block
            .counts
            .iter()
            .map(|(kind, count)| {
                let truth = fixture
                    .truth_ppm_per_token
                    .iter()
                    .find(|(k, _)| k == kind)
                    .map(|(_, v)| *v)
                    .unwrap();
                truth * *count as f64
            })
            .sum();
        let settled = cumulative_ppm + movement;
        // The first reading after the spend is the meter still catching up.
        reading(
            &conn,
            &chain,
            t + 10 * SECOND,
            (cumulative_ppm + movement / 2.0).round() as i64,
        );
        reading(&conn, &chain, t + 20 * SECOND, settled.round() as i64);
        reading(&conn, &chain, t + 30 * SECOND, settled.round() as i64);
        cumulative_ppm = settled;
        t += 60 * SECOND;
    }
    record_end(&conn, &run.id, UtcTimestamp::from_unix_nanos(t)).unwrap();
    reading(
        &conn,
        &chain,
        t + 30 * SECOND,
        cumulative_ppm.round() as i64,
    );
    std::fs::write(state.path().join("aub.toml"), "").unwrap();
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

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// The independent design yields one coefficient per named kind, each within
/// its own interval of the truth the fixture was generated from, recorded
/// once, and the ledger holds no activation of it.
#[test]
fn multivariate_fit_records_a_candidate_and_never_activates() {
    let state = StateDir::new();
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    seed_burst(&state, &fixture, TokenKind::ALL.to_vec());

    let output = run_aub(
        &state,
        &[
            "calibrate",
            "fit",
            "--experiment",
            EXPERIMENT,
            "--format",
            "json",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout must be JSON");

    assert_eq!(json["fit_kind"], "multivariate");
    assert_eq!(json["experiment_id"], EXPERIMENT);
    assert_eq!(json["activated"], false);
    assert_eq!(
        json["token_kinds"],
        serde_json::json!(["input", "output", "cache_read", "cache_write"])
    );
    let coefficients = json["coefficients"]
        .as_array()
        .expect("coefficients must be an array");
    assert_eq!(coefficients.len(), 4);
    for (kind, truth) in fixture.truth_ppm_per_token {
        let row = coefficients
            .iter()
            .find(|c| c["token_kind"] == kind.label())
            .unwrap_or_else(|| panic!("no coefficient for {}", kind.label()));
        let estimate = row["estimate_ppm_per_token"].as_f64().unwrap();
        let low = row["interval_low_ppm_per_token"].as_f64().unwrap();
        let high = row["interval_high_ppm_per_token"].as_f64().unwrap();
        assert!(
            (estimate - truth).abs() < truth * 0.05,
            "{} estimate {estimate} is not within 5% of truth {truth}",
            kind.label()
        );
        assert!(
            low <= estimate && estimate <= high,
            "interval must bracket the estimate"
        );
    }
    assert!(json["condition_number"].as_f64().unwrap() < 30.0);
    assert_eq!(json["condition_number_threshold"], 30.0);
    assert!(
        json["fit_residual_ppm"].as_f64().unwrap() < 100.0,
        "residual: {}",
        json["fit_residual_ppm"]
    );
    assert_eq!(json["usable_observations"], 12);
    assert_eq!(json["sample_count"], 12);
    assert!(
        json["phase_design"]
            .as_str()
            .unwrap()
            .contains("frame=settled-blocks")
    );

    let conn = open_test_ledger(&state);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_candidate"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_coefficient"
        ),
        4
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM window_calibration_candidate"),
        0,
        "no univariate candidate is written by the joint path"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM calibration_lifecycle"),
        0,
        "the fit never activates"
    );

    // The same evidence fitted again yields the same candidate id and no second row.
    let again = run_aub(
        &state,
        &[
            "calibrate",
            "fit",
            "--experiment",
            EXPERIMENT,
            "--format",
            "json",
        ],
    );
    assert_eq!(again.status.code(), Some(0));
    let json_again: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&again.stdout).trim()).unwrap();
    assert_eq!(json_again["candidate_id"], json["candidate_id"]);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_candidate"
        ),
        1
    );
}

/// The text rendering names every kind and says activation was not performed.
#[test]
fn multivariate_fit_text_report_names_each_kind_and_the_gate() {
    let state = StateDir::new();
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    seed_burst(&state, &fixture, TokenKind::ALL.to_vec());

    let output = run_aub(&state, &["calibrate", "fit", "--experiment", EXPERIMENT]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("Fit:          multivariate over input,output,cache_read,cache_write"));
    for kind in TokenKind::ALL {
        assert!(stdout.contains(&format!("  {}:", kind.label())), "{stdout}");
    }
    assert!(stdout.contains("Condition:    "), "{stdout}");
    assert!(stdout.contains("(threshold 30.00)"), "{stdout}");
    assert!(stdout.contains("Activation:   not performed"), "{stdout}");
    assert!(
        !stdout.contains("micros/point"),
        "no scalar coefficient is printed for a joint fit"
    );
}

/// The proportional design is refused by name: the message carries the
/// entangled pair, the condition number and the bound, the exit is the typed
/// insufficient-evidence status, and nothing is recorded.
#[test]
fn proportional_arms_are_refused_by_name_and_record_nothing() {
    let state = StateDir::new();
    let fixture = parse_fixture(PROPORTIONAL_ARMS);
    seed_burst(&state, &fixture, TokenKind::ALL.to_vec());
    let show_before = run_aub(&state, &["calibrate", "show", "--format", "json"]);

    let output = run_aub(&state, &["calibrate", "fit", "--experiment", EXPERIMENT]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(6), "stderr: {stderr}");
    assert!(stderr.contains("ill-conditioned fit"), "{stderr}");
    assert!(stderr.contains("exceeds threshold 30.00"), "{stderr}");
    assert!(stderr.contains("cannot separate"), "{stderr}");
    assert!(
        stderr.contains("condition number infinite") || stderr.contains("condition number "),
        "{stderr}"
    );

    let conn = open_test_ledger(&state);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_candidate"
        ),
        0
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_coefficient"
        ),
        0
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM window_calibration_candidate"),
        0
    );
    let show_after = run_aub(&state, &["calibrate", "show", "--format", "json"]);
    let entries = |output: &std::process::Output| -> serde_json::Value {
        let json: serde_json::Value =
            serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
        json["entries"].clone()
    };
    assert_eq!(
        entries(&show_before),
        entries(&show_after),
        "calibrate show must be unchanged by a refusal"
    );
    assert_eq!(entries(&show_after), serde_json::json!([]));
}

/// A controlled run whose premise names one kind is not the joint path: the
/// command falls through to the univariate fitter exactly as before, which
/// on a run with no univariate experiment row is its existing refusal.
#[test]
fn a_single_kind_premise_keeps_the_univariate_path() {
    let state = StateDir::new();
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    seed_burst(&state, &fixture, vec![TokenKind::Output]);

    let output = run_aub(&state, &["calibrate", "fit", "--experiment", EXPERIMENT]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(6), "stderr: {stderr}");
    assert!(stderr.contains("no calibration experiment"), "{stderr}");
    let conn = open_test_ledger(&state);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_candidate"
        ),
        0
    );
}

/// A run that has not recorded `end` is refused before any fitting.
#[test]
fn a_running_experiment_is_refused_before_fitting() {
    let state_running = StateDir::new();
    {
        let conn = open_test_ledger(&state_running);
        let chain = meter_chain(&conn);
        let baseline = reading(&conn, &chain, 1_000 * SECOND, BASELINE_PPM);
        let run = ControlledExperimentRun {
            id: ControlledExperimentId::new(EXPERIMENT),
            account: ACCOUNT.into(),
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new("max-5x"),
            window_semantic_key: WindowSemanticKey::new(WINDOW),
            cost_model_id: CostModelId::new("anthropic-claude-messages-v1"),
            expected_token_kinds: TokenKind::ALL.to_vec(),
            baseline_observation_id: baseline,
            baseline_quota_used: QuotaUsed::new(
                QuotaFractionPpm::new(i32::try_from(BASELINE_PPM).unwrap()).unwrap(),
            ),
            baseline_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            baseline_observed_at: UtcTimestamp::from_unix_nanos(1_000 * SECOND),
            baseline_plateau_started_at: UtcTimestamp::from_unix_nanos(1_000 * SECOND),
            contamination_thresholds: ContaminationThresholds::conservative_default(),
            started_at: UtcTimestamp::from_unix_nanos(1_001 * SECOND),
            ended_at: None,
            exclusivity_assertion: "reserved".into(),
        };
        insert_begin(&conn, &run).unwrap();
        std::fs::write(state_running.path().join("aub.toml"), "").unwrap();
    }
    let output = run_aub(
        &state_running,
        &["calibrate", "fit", "--experiment", EXPERIMENT],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(6), "stderr: {stderr}");
    assert!(stderr.contains("is still running"), "{stderr}");
}
