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
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
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
use agent_usage_book::store::cost_model::{ProviderKey, seed_initial_cost_model};
use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, NewMeterAttemptResult};
use agent_usage_book::store::meter_evidence::{
    NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, ObservationRowId,
};
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
};
use agent_usage_book::store::usage_component::NewUsageComponent;
use agent_usage_book::store::usage_event::NewUsageEvent;
use agent_usage_book::store::usage_occurrence::{NewUsageOccurrence, insert_occurrence};
use agent_usage_book::store::{
    account as account_store, connection, meter_attempt as attempt_store,
    meter_evidence as evidence_store, migrate, migrations, sample_run as run_store,
    sampling_policy_snapshot as snapshot_store, usage_component as component_store,
    usage_event as event_store,
};
use agent_usage_book::transcripts::parser::ParserVersion;
use rusqlite::Connection;
use test_support::StateDir;

const SECOND: i64 = 1_000_000_000;
const EXPERIMENT: &str = "exp-multivariate";
const ACCOUNT: &str = "bianca";
const WINDOW: &str = "five_hour";
const SESSION_SOURCE: &str = "claude-code";
const BURST_SESSION: &str = "burst-session";
const BASELINE_PPM: i64 = 100_000;
const SINGLE_KIND_EXPERIMENT: &str = "exp-single-kind";
const SINGLE_KIND_BLOCKS: usize = 4;
/// The baseline reading plus one settled reading per block.
const SINGLE_KIND_READINGS: usize = SINGLE_KIND_BLOCKS + 1;
/// Priced at 15 credits per million output tokens, this is 3 credits a block.
const OUTPUT_TOKENS_PER_BLOCK: u64 = 200_000;
const CREDITS_PER_BLOCK: i64 = 3;
const PPM_PER_BLOCK: i64 = 30_000;
const SEEDED_MICROS_PER_POINT: i64 = 1_000_000 / (PPM_PER_BLOCK / CREDITS_PER_BLOCK);

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
    meter_chain_for(conn, ACCOUNT)
}

fn meter_chain_for(conn: &Connection, account: &str) -> MeterChain {
    let at = UtcTimestamp::from_unix_nanos(100 * SECOND);
    let account_id = account_store::observe_account(conn, "anthropic", account, at).unwrap();
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

/// Places `native` on `account` from `at_nanos` onward, the way a launcher
/// hook records which subscription a session runs on.
fn attribute_session(conn: &Connection, native: &str, account: &str, at_nanos: i64) {
    insert_marker(
        conn,
        &NewSessionAccountMarker {
            session_id: SessionId::new(
                SourceNamespace::new(SESSION_SOURCE),
                NativeSessionId::new(native),
            ),
            observed_at: UtcTimestamp::from_unix_nanos(at_nanos),
            source_ordering_key: None,
            logical_account: account.to_string(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        },
    )
    .unwrap();
}

/// Inserts one usage event of `native`'s session with its transcript
/// occurrence, which is where the session's source namespace is recorded.
fn usage_event(
    conn: &Connection,
    native: &str,
    canonical_id: &str,
    at_nanos: i64,
    counts: &[(TokenKind, u64)],
) {
    let ts = UtcTimestamp::from_unix_nanos(at_nanos);
    let event_id = event_store::insert_event(
        conn,
        &NewUsageEvent {
            canonical_event_id: canonical_id,
            session_id: Some(native),
            event_timestamp: Some(ts),
            model_id: Some("claude-opus"),
            evidence_kind: "transcript",
            source_provenance: "test",
            parser_version: "v1",
            created_at: ts,
        },
    )
    .unwrap();
    for (kind, count) in counts {
        component_store::insert_component(
            conn,
            &NewUsageComponent {
                event_id,
                token_class: kind.label(),
                count: *count,
            },
        )
        .unwrap();
    }
    insert_occurrence(
        conn,
        &NewUsageOccurrence {
            source_namespace: &SourceNamespace::new(SESSION_SOURCE),
            native_event_id: Some(canonical_id),
            parser_version: &ParserVersion::new("claude-code-1"),
            heuristic_key: None,
            source_file: &format!("corpus/{native}.jsonl"),
            occurred_at_nanos: Some(at_nanos),
            event_id: Some(event_id),
            transcript_file_id: None,
            source_location: None,
            canonical_fingerprint: None,
            identity_strength: None,
            heuristic_algorithm_version: None,
            canonical_payload_digest: None,
        },
    )
    .unwrap();
}

fn spend(conn: &Connection, at_nanos: i64, index: usize, block: &ArmBlock) {
    usage_event(
        conn,
        BURST_SESSION,
        &format!("burst-block-{index}"),
        at_nanos,
        &block.counts,
    );
}

/// Seeds a whole controlled burst from an arm fixture: the baseline reading,
/// `begin` with the given premise, then per block one usage event followed
/// by a lagging reading and two settled ones, then `end`.
fn seed_burst(state: &StateDir, fixture: &ArmFixture, expected_kinds: Vec<TokenKind>) {
    seed_burst_ending(state, fixture, expected_kinds, RunEnd::AfterLastSettle);
}

/// Where `end` falls relative to the last block's readings.
#[derive(Clone, Copy, PartialEq)]
enum RunEnd {
    /// After the last block's settled readings, as a driver that waits does.
    AfterLastSettle,
    /// Between the last block's lagging reading and its settled ones, so the
    /// reading that measures the last block is taken after `end`.
    BeforeLastSettle,
}

/// [`seed_burst`] with the end placed as `end` says; returns the `end`
/// instant in unix nanos.
fn seed_burst_ending(
    state: &StateDir,
    fixture: &ArmFixture,
    expected_kinds: Vec<TokenKind>,
    end: RunEnd,
) -> i64 {
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
    attribute_session(&conn, BURST_SESSION, ACCOUNT, t0);

    let mut cumulative_ppm = BASELINE_PPM as f64;
    let mut t = t0 + 60 * SECOND;
    let last_block = fixture.blocks.len() - 1;
    let mut ended_at = None;
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
        if index == last_block && end == RunEnd::BeforeLastSettle {
            let at = t + 15 * SECOND;
            record_end(&conn, &run.id, UtcTimestamp::from_unix_nanos(at)).unwrap();
            ended_at = Some(at);
        }
        reading(&conn, &chain, t + 20 * SECOND, settled.round() as i64);
        reading(&conn, &chain, t + 30 * SECOND, settled.round() as i64);
        cumulative_ppm = settled;
        t += 60 * SECOND;
    }
    let ended_at = ended_at.unwrap_or_else(|| {
        record_end(&conn, &run.id, UtcTimestamp::from_unix_nanos(t)).unwrap();
        t
    });
    reading(
        &conn,
        &chain,
        t + 30 * SECOND,
        cumulative_ppm.round() as i64,
    );
    std::fs::write(state.path().join("aub.toml"), "").unwrap();
    ended_at
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

/// The `aub-ks5n` premise rule through the binary: a joint premise begins
/// with no `--cost-model` on a ledger holding no cost model at all, and
/// `status`, `end` and `fit` all work on it, with `fit` recording a
/// multivariate candidate. Every command here is its own process; the
/// readings between them are stored rows standing in for scheduler samples.
#[test]
fn joint_premise_begins_ends_and_fits_with_no_cost_model_in_the_ledger() {
    use std::time::{SystemTime, UNIX_EPOCH};
    const JOINT_EXPERIMENT: &str = "exp-joint-no-model";
    let now_nanos = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time must be readable")
            .as_nanos() as i64
    };
    let state = StateDir::new();
    std::fs::write(
        state.path().join("aub.toml"),
        "[[accounts]]\nname = \"bianca\"\nprovider = \"anthropic\"\nplan_tier = \"max-5x\"\n",
    )
    .expect("config must be writable");
    let fixture = parse_fixture(INDEPENDENT_ARMS);

    // One settled baseline reading a minute in the past, then `begin` with a
    // four-kind premise and no `--cost-model` as its own process.
    let baseline_at = now_nanos() - 60 * SECOND;
    {
        let conn = open_test_ledger(&state);
        let chain = meter_chain(&conn);
        reading(&conn, &chain, baseline_at, BASELINE_PPM);
    }
    let begin = run_aub(
        &state,
        &[
            "calibrate",
            "--account",
            ACCOUNT,
            "begin",
            "--window",
            WINDOW,
            "--expect-kinds",
            "input,output,cache_read,cache_write",
            "--experiment",
            JOINT_EXPERIMENT,
            "--assert-exclusive",
        ],
    );
    let begin_stdout = String::from_utf8_lossy(&begin.stdout).into_owned();
    let begin_stderr = String::from_utf8_lossy(&begin.stderr).into_owned();
    assert_eq!(
        begin.status.code(),
        Some(0),
        "begin must succeed with no cost model.\nstdout: {begin_stdout}\nstderr: {begin_stderr}"
    );
    assert!(
        begin_stdout.contains(&format!("experiment={JOINT_EXPERIMENT}")),
        "begin must print the premise:\n{begin_stdout}"
    );
    assert!(
        begin_stdout.contains("cost_model=none"),
        "begin must print the sentinel:\n{begin_stdout}"
    );
    assert!(
        begin_stdout.contains("expect_kinds=input,output,cache_read,cache_write"),
        "begin must print the premise kinds:\n{begin_stdout}"
    );
    {
        let conn = open_test_ledger(&state);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM cost_model"),
            0,
            "the ledger must hold no cost model at all"
        );
        let stored: String = conn
            .query_row(
                "SELECT cost_model_id FROM calibration_controlled_run WHERE experiment_id = ?1",
                [JOINT_EXPERIMENT],
                |row| row.get(0),
            )
            .expect("the run must be recorded");
        assert_eq!(stored, "none");
    }

    // The burst between the two boundaries: one usage event per block, each
    // followed by a lagging reading and two settled ones, stamped after the
    // recorded `started_at` so the run's own frame holds them.
    let started_at: i64 = {
        let conn = open_test_ledger(&state);
        conn.query_row(
            "SELECT started_at FROM calibration_controlled_run WHERE experiment_id = ?1",
            [JOINT_EXPERIMENT],
            |row| row.get(0),
        )
        .expect("the run must be recorded")
    };
    {
        let conn = open_test_ledger(&state);
        let chain = meter_chain(&conn);
        attribute_session(&conn, BURST_SESSION, ACCOUNT, baseline_at);
        let mut cumulative_ppm = BASELINE_PPM as f64;
        let mut t = started_at + SECOND / 1_000;
        for (index, block) in fixture.blocks.iter().enumerate() {
            spend(&conn, t, 1_000_000 + index, block);
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
            reading(
                &conn,
                &chain,
                t + 2 * SECOND / 1_000,
                (cumulative_ppm + movement / 2.0).round() as i64,
            );
            reading(
                &conn,
                &chain,
                t + 4 * SECOND / 1_000,
                settled.round() as i64,
            );
            reading(
                &conn,
                &chain,
                t + 6 * SECOND / 1_000,
                settled.round() as i64,
            );
            cumulative_ppm = settled;
            t += 10 * SECOND / 1_000;
        }
        // `end` stamps the real instant it runs, so wait until the wall
        // clock has passed the last block before closing the boundary.
        while now_nanos() <= t {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    let status_running = run_aub(
        &state,
        &["calibrate", "status", "--experiment", JOINT_EXPERIMENT],
    );
    let status_stdout = String::from_utf8_lossy(&status_running.stdout).into_owned();
    assert_eq!(
        status_running.status.code(),
        Some(0),
        "status must work on the joint run: {status_stdout}"
    );
    assert!(status_stdout.contains("phase=running"), "{status_stdout}");

    let end = run_aub(
        &state,
        &["calibrate", "end", "--experiment", JOINT_EXPERIMENT],
    );
    let end_stdout = String::from_utf8_lossy(&end.stdout).into_owned();
    let end_stderr = String::from_utf8_lossy(&end.stderr).into_owned();
    assert_eq!(
        end.status.code(),
        Some(0),
        "end must work on the joint run.\nstdout: {end_stdout}\nstderr: {end_stderr}"
    );

    let fit = run_aub(
        &state,
        &[
            "calibrate",
            "fit",
            "--experiment",
            JOINT_EXPERIMENT,
            "--format",
            "json",
        ],
    );
    let fit_stdout = String::from_utf8_lossy(&fit.stdout).into_owned();
    let fit_stderr = String::from_utf8_lossy(&fit.stderr).into_owned();
    assert_eq!(
        fit.status.code(),
        Some(0),
        "fit must work on the joint run.\nstdout: {fit_stdout}\nstderr: {fit_stderr}"
    );
    let json: serde_json::Value =
        serde_json::from_str(fit_stdout.trim()).expect("fit stdout must be JSON");
    assert_eq!(json["fit_kind"], "multivariate");
    assert_eq!(json["experiment_id"], JOINT_EXPERIMENT);
    let conn = open_test_ledger(&state);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_candidate"
        ),
        1,
        "fit must record a multivariate candidate"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_coefficient"
        ),
        4
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

/// The keys the univariate JSON report carries, in the shape
/// `tests/calibrate_fit_command.rs` pins for an experiment-table fit. A
/// controlled run reports through the same renderer, so this set is what says
/// the two are one contract and not two that happen to agree today.
const UNIVARIATE_JSON_KEYS: [&str; 20] = [
    "candidate_id",
    "diagnostic_findings",
    "equivalent_full_window_capacity_micros",
    "excluded_samples",
    "experiment_id",
    "fit_residual_micros",
    "fitted_micros_per_point",
    "inputs_count",
    "inputs_digest",
    "lag_handling",
    "plan_tier",
    "provider",
    "residual_percentage_points",
    "sample_count",
    "statistical_method",
    "statistical_parameters",
    "uncertainty_high_micros",
    "uncertainty_low_micros",
    "usable_observations",
    "window_semantic_key",
];

fn spend_output(conn: &Connection, at_nanos: i64, index: usize, tokens: u64) {
    usage_event(
        conn,
        BURST_SESSION,
        &format!("single-kind-block-{index}"),
        at_nanos,
        &[(TokenKind::Output, tokens)],
    );
}

/// Seeds a controlled run whose premise names output alone: the baseline
/// reading, `begin`, then one output-only spend per block each followed by
/// the settled reading it moved the meter to, and `end`.
///
/// The truth is exact by construction. Each block spends
/// `OUTPUT_TOKENS_PER_BLOCK` output tokens, which the built-in cost model
/// prices at `CREDITS_PER_BLOCK` credits, and moves the meter
/// `PPM_PER_BLOCK`, so the seeded coefficient is
/// `1_000_000 / (PPM_PER_BLOCK / CREDITS_PER_BLOCK)` micros per point.
fn seed_single_kind_burst(state: &StateDir, end_the_run: bool, decoy_account: bool) {
    let mut conn = open_test_ledger(state);
    seed_initial_cost_model(&mut conn, UtcTimestamp::from_unix_nanos(500 * SECOND)).unwrap();
    let chain = meter_chain(&conn);
    let t0 = 1_000 * SECOND;
    let baseline = reading(&conn, &chain, t0, BASELINE_PPM);
    let run = ControlledExperimentRun {
        id: ControlledExperimentId::new(SINGLE_KIND_EXPERIMENT),
        account: ACCOUNT.into(),
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("max-5x"),
        window_semantic_key: WindowSemanticKey::new(WINDOW),
        cost_model_id: CostModelId::new("anthropic-claude-messages-v1"),
        expected_token_kinds: vec![TokenKind::Output],
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
        exclusivity_assertion: format!("account {ACCOUNT} reserved for {SINGLE_KIND_EXPERIMENT}"),
    };
    insert_begin(&conn, &run).unwrap();
    attribute_session(&conn, BURST_SESSION, ACCOUNT, t0);

    let mut t = t0 + 60 * SECOND;
    let mut used_ppm = BASELINE_PPM;
    for index in 0..SINGLE_KIND_BLOCKS {
        spend_output(&conn, t, index, OUTPUT_TOKENS_PER_BLOCK);
        used_ppm += PPM_PER_BLOCK;
        reading(&conn, &chain, t + 30 * SECOND, used_ppm);
        t += 60 * SECOND;
    }
    if decoy_account {
        // Another account of the same provider, reporting its own window
        // between two of the run's readings. It belongs to no controlled run
        // and a fit that took the provider's readings would swallow it.
        let other = meter_chain_for(&conn, "someone-else");
        reading(&conn, &other, t0 + 75 * SECOND, 900_000);
    }
    if end_the_run {
        record_end(&conn, &run.id, UtcTimestamp::from_unix_nanos(t)).unwrap();
    }
    std::fs::write(state.path().join("aub.toml"), "").unwrap();
}

fn json_keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("the report must be a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// A controlled run whose premise names one kind fits univariately over that
/// run's own observations: the candidate names the run, its coefficient is the
/// seeded truth, its validity is the run's spend window, and the report is the
/// univariate one key for key.
#[test]
fn a_single_kind_premise_fits_the_univariate_candidate_from_the_run() {
    let state = StateDir::new();
    seed_single_kind_burst(&state, true, true);

    let output = run_aub(
        &state,
        &[
            "calibrate",
            "fit",
            "--experiment",
            SINGLE_KIND_EXPERIMENT,
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

    // A controlled run adds exactly one key to the univariate contract: the
    // contamination verdict an experiment-table fit has no run to judge.
    let mut expected_keys = UNIVARIATE_JSON_KEYS.to_vec();
    expected_keys.push("contamination");
    expected_keys.sort_unstable();
    assert_eq!(
        json_keys(&json),
        expected_keys,
        "the controlled-run fit reports the univariate contract plus its contamination verdict"
    );
    assert_eq!(json["experiment_id"], SINGLE_KIND_EXPERIMENT);
    assert_eq!(json["provider"], "anthropic");
    assert_eq!(json["plan_tier"], "max-5x");
    assert_eq!(json["window_semantic_key"], WINDOW);
    assert_eq!(
        json["fitted_micros_per_point"], SEEDED_MICROS_PER_POINT,
        "the coefficient must be the seeded truth"
    );
    let low = json["uncertainty_low_micros"].as_i64().unwrap();
    let high = json["uncertainty_high_micros"].as_i64().unwrap();
    assert!(
        low <= SEEDED_MICROS_PER_POINT && SEEDED_MICROS_PER_POINT <= high,
        "the seeded truth must lie inside [{low}, {high}]"
    );
    assert_eq!(
        json["sample_count"], SINGLE_KIND_READINGS,
        "the baseline reading and every reading after it are the run's own"
    );
    assert_eq!(json["usable_observations"], SINGLE_KIND_READINGS);

    let conn = open_test_ledger(&state);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM window_calibration_candidate"),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_candidate"
        ),
        0,
        "a one-kind premise records no joint candidate"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM calibration_lifecycle"),
        0,
        "the fit never activates"
    );
    let (experiment_id, valid_from, valid_until): (String, i64, i64) = conn
        .query_row(
            "SELECT e.experiment_id, c.valid_from, c.valid_until
             FROM window_calibration_candidate c
             JOIN calibration_experiment e ON e.id = c.experiment_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("the candidate must resolve to the experiment row it names");
    assert_eq!(experiment_id, SINGLE_KIND_EXPERIMENT);
    assert_eq!(
        valid_from,
        1_001 * SECOND,
        "the candidate's usage frame opens at the run's started_at"
    );
    assert_eq!(
        valid_until,
        1_000 * SECOND + 60 * SECOND * (SINGLE_KIND_BLOCKS as i64 + 1),
        "the candidate's usage frame closes at the run's ended_at"
    );

    // The same evidence fitted again records nothing further.
    let again = run_aub(
        &state,
        &[
            "calibrate",
            "fit",
            "--experiment",
            SINGLE_KIND_EXPERIMENT,
            "--format",
            "json",
        ],
    );
    assert_eq!(again.status.code(), Some(0));
    let json_again: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&again.stdout).trim()).unwrap();
    assert_eq!(json_again["candidate_id"], json["candidate_id"]);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM window_calibration_candidate"),
        1
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM calibration_experiment"),
        1,
        "the run's experiment row is written once, however often it is fitted"
    );
}

/// A one-kind run that has not recorded `end` is refused by the univariate
/// path in the joint path's words, and records nothing.
#[test]
fn a_running_single_kind_experiment_is_refused_before_fitting() {
    let state = StateDir::new();
    seed_single_kind_burst(&state, false, false);

    let output = run_aub(
        &state,
        &["calibrate", "fit", "--experiment", SINGLE_KIND_EXPERIMENT],
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(6), "stderr: {stderr}");
    assert!(stderr.contains("is still running"), "{stderr}");
    assert!(stderr.contains("aub calibrate end"), "{stderr}");

    let conn = open_test_ledger(&state);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM window_calibration_candidate"),
        0
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM calibration_experiment"),
        0,
        "a refusal writes no experiment row either"
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

/// A joint candidate has one coefficient per token kind and the result table
/// carries a single scalar, so promotion refuses rather than reducing the
/// coefficients through a cost model, which would reintroduce the assumption
/// the joint fit exists to test. The refusal names the bead that owns the
/// multivariate result shape.
#[test]
fn promoting_a_joint_candidate_is_refused_naming_the_successor_bead() {
    let state = StateDir::new();
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    seed_burst(&state, &fixture, TokenKind::ALL.to_vec());

    let fit = run_aub(
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
    assert_eq!(fit.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&fit.stdout).trim()).unwrap();
    let candidate_id = json["candidate_id"]
        .as_str()
        .expect("a candidate id")
        .to_string();

    let promote = run_aub(
        &state,
        &[
            "calibrate",
            "promote",
            &candidate_id,
            "--training",
            "ev-training",
            "--validation",
            "ev-validation",
        ],
    );
    let stderr = String::from_utf8_lossy(&promote.stderr).into_owned();
    assert_ne!(
        promote.status.code(),
        Some(0),
        "a joint candidate must not be promoted"
    );
    assert!(
        stderr.contains("no scalar result shape yet"),
        "the refusal must state why a joint candidate cannot be promoted: {stderr}"
    );
    assert!(
        stderr.contains("aub-multivariate-result-shape-2hvt"),
        "the refusal must name the bead that owns the multivariate result shape: {stderr}"
    );

    let conn = open_test_ledger(&state);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM window_calibration_result"),
        0,
        "a refused promotion records no result"
    );
}

fn fit_json(state: &StateDir) -> serde_json::Value {
    let output = run_aub(
        state,
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
    serde_json::from_str(stdout.trim()).expect("stdout must be JSON")
}

/// The first block of `seed_burst` spends at this instant; a foreign event a
/// few seconds later lands inside that block's interval.
const FIRST_BLOCK_AT: i64 = 1_060 * SECOND;

/// A session on another account spending millions of cache-read tokens in
/// the middle of the run is the machine the burst ran on, not the run. The
/// coefficients fitted with it present are the ones fitted without it.
#[test]
fn usage_of_another_account_in_the_run_window_never_enters_the_fit() {
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    let clean = StateDir::new();
    seed_burst(&clean, &fixture, TokenKind::ALL.to_vec());
    let clean_json = fit_json(&clean);

    let contaminated = StateDir::new();
    seed_burst(&contaminated, &fixture, TokenKind::ALL.to_vec());
    let conn = open_test_ledger(&contaminated);
    attribute_session(&conn, "orchestrator-session", "someone-else", 900 * SECOND);
    usage_event(
        &conn,
        "orchestrator-session",
        "orchestrator-turn",
        FIRST_BLOCK_AT + 5 * SECOND,
        &[
            (TokenKind::CacheRead, 3_400_000),
            (TokenKind::Output, 3_300),
        ],
    );
    drop(conn);
    let contaminated_json = fit_json(&contaminated);

    for kind in TokenKind::ALL {
        let estimate = |json: &serde_json::Value| {
            json["coefficients"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["token_kind"] == kind.label())
                .map(|c| c["estimate_ppm_per_token"].clone())
                .unwrap()
        };
        assert_eq!(
            estimate(&contaminated_json),
            estimate(&clean_json),
            "{} moved when a foreign account's usage was added",
            kind.label()
        );
    }
    assert_eq!(
        contaminated_json["excluded_samples"],
        clean_json["excluded_samples"]
    );
}

/// The single-kind run's third block spends at this instant; the decoy and
/// the two held-out readings are placed around it. A decoy in the first or the
/// last block would shift one end of the series, which the robust fit sets
/// aside as a single outlier and so would not detect a priced decoy; in the
/// middle it splits the series in two and moves the slope.
const SINGLE_KIND_DECOY_BLOCK_AT: i64 = 1_180 * SECOND;

/// Two readings of a third account's window, one before and one after the
/// single-kind run's third block, moved by exactly that block's credits at the
/// seeded coefficient: the held-out residual is zero on the run's own spend.
/// A decoy session spending between them would push the predicted movement far
/// past the second reading if its credits were priced.
fn seed_holdout_readings(state: &StateDir) {
    let conn = open_test_ledger(state);
    let holdout = meter_chain_for(&conn, "holdout");
    reading(
        &conn,
        &holdout,
        SINGLE_KIND_DECOY_BLOCK_AT - 7 * SECOND,
        BASELINE_PPM,
    );
    reading(
        &conn,
        &holdout,
        SINGLE_KIND_DECOY_BLOCK_AT + 23 * SECOND,
        BASELINE_PPM + PPM_PER_BLOCK,
    );
}

/// A session on another account spending 3,000,000 output tokens, the kind the
/// premise names, inside the single-kind run's third block.
fn seed_decoy_session(state: &StateDir) {
    let conn = open_test_ledger(state);
    attribute_session(&conn, "decoy-session", "someone-else", 900 * SECOND);
    usage_event(
        &conn,
        "decoy-session",
        "decoy-turn",
        SINGLE_KIND_DECOY_BLOCK_AT + 10 * SECOND,
        &[(TokenKind::Output, 3_000_000)],
    );
}

/// The evidence ids of `account`'s readings, comma-joined in time order.
fn evidence_of_account(conn: &Connection, account: &str) -> String {
    let mut stmt = conn
        .prepare(
            "SELECT re.content_hash
             FROM meter_observation mo
             JOIN meter_response_evidence re ON re.id = mo.evidence_id
             JOIN account a ON a.id = mo.account_id
             WHERE a.logical_name = ?1
             ORDER BY mo.received_at ASC, mo.id ASC",
        )
        .unwrap();
    let rows = stmt
        .query_map([account], |row| row.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect::<Vec<_>>().join(",")
}

fn single_kind_fit_json(state: &StateDir) -> serde_json::Value {
    let fit = run_aub(
        state,
        &[
            "calibrate",
            "fit",
            "--experiment",
            SINGLE_KIND_EXPERIMENT,
            "--format",
            "json",
        ],
    );
    assert_eq!(
        fit.status.code(),
        Some(0),
        "fit: {}",
        String::from_utf8_lossy(&fit.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&fit.stdout).trim()).unwrap()
}

/// Promotes the fitted single-kind candidate against the holdout readings.
fn promote_single_kind_json(state: &StateDir, fit_json: &serde_json::Value) -> serde_json::Value {
    let conn = open_test_ledger(state);
    let training = evidence_of_account(&conn, ACCOUNT);
    let validation = evidence_of_account(&conn, "holdout");
    drop(conn);
    let candidate_id = fit_json["candidate_id"].as_str().unwrap().to_string();
    let promote = run_aub(
        state,
        &[
            "calibrate",
            "promote",
            &candidate_id,
            "--training",
            &training,
            "--validation",
            &validation,
            "--format",
            "json",
        ],
    );
    assert_eq!(
        promote.status.code(),
        Some(0),
        "promote: {}",
        String::from_utf8_lossy(&promote.stderr)
    );
    serde_json::from_slice(&promote.stdout).unwrap()
}

/// A session on another account spending 3,000,000 output tokens inside a
/// one-kind run is not the run's spend. The univariate coefficient fitted with
/// it present is the one fitted without it, to the micro, and promotion
/// reproduces that coefficient and judges it on the same held-out residual.
#[test]
fn a_decoy_account_never_enters_the_single_kind_fit_or_its_promotion() {
    let clean = StateDir::new();
    seed_single_kind_burst(&clean, true, true);
    seed_holdout_readings(&clean);
    let clean_fit = single_kind_fit_json(&clean);

    let contaminated = StateDir::new();
    seed_single_kind_burst(&contaminated, true, true);
    seed_holdout_readings(&contaminated);
    seed_decoy_session(&contaminated);
    let fit = single_kind_fit_json(&contaminated);

    assert_eq!(
        fit["fitted_micros_per_point"], SEEDED_MICROS_PER_POINT,
        "the decoy moved the fitted coefficient"
    );
    assert_eq!(
        fit["fitted_micros_per_point"],
        clean_fit["fitted_micros_per_point"]
    );
    assert_eq!(fit["excluded_samples"], clean_fit["excluded_samples"]);

    let clean_promote = promote_single_kind_json(&clean, &clean_fit);
    let promote = promote_single_kind_json(&contaminated, &fit);
    assert_eq!(promote["fitted"], clean_promote["fitted"]);
    assert_eq!(
        promote["held_out_residual"]["value"], "0",
        "the held-out readings moved by the run's own block alone"
    );
    assert_eq!(
        promote["held_out_residual"], clean_promote["held_out_residual"],
        "the decoy reached the promotion's validation series"
    );
    assert_eq!(promote["validation_observations"], 2);
}

/// The settled value of the newest reading in the ledger.
fn newest_reading_ppm(conn: &Connection) -> i64 {
    count(
        conn,
        "SELECT mw.quota_used_ppm FROM meter_window mw
         JOIN meter_observation mo ON mo.id = mw.observation_id
         ORDER BY mo.received_at DESC, mw.id DESC LIMIT 1",
    )
}

/// Every part of a joint fit's answer that the readings decide: the
/// coefficients with their errors and intervals, the block residual, the
/// block count and the exclusions.
fn assert_same_fit(actual: &serde_json::Value, expected: &serde_json::Value, what: &str) {
    for key in [
        "coefficients",
        "fit_residual_ppm",
        "usable_observations",
        "excluded_samples",
    ] {
        assert_eq!(actual[key], expected[key], "{key} changed when {what}");
    }
}

/// The last block has no next spend to close it. A session on the run's own
/// account resumed after `end`, and the meter moving 130,000 ppm for it
/// inside the settlement grace, measure the window after the run: the fit
/// with them present is the fit without them.
#[test]
fn usage_on_the_run_account_after_end_never_reaches_the_last_block() {
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    let clean = StateDir::new();
    seed_burst(&clean, &fixture, TokenKind::ALL.to_vec());
    let clean_json = fit_json(&clean);

    let late = StateDir::new();
    let ended_at = seed_burst_ending(
        &late,
        &fixture,
        TokenKind::ALL.to_vec(),
        RunEnd::AfterLastSettle,
    );
    let conn = open_test_ledger(&late);
    let chain = meter_chain(&conn);
    let settled_ppm = newest_reading_ppm(&conn);
    usage_event(
        &conn,
        BURST_SESSION,
        "cold-resume-probe",
        ended_at + 60 * SECOND,
        &[
            (TokenKind::CacheWrite, 122_598),
            (TokenKind::CacheRead, 8_033),
        ],
    );
    reading(
        &conn,
        &chain,
        ended_at + 120 * SECOND,
        settled_ppm + 130_000,
    );
    drop(conn);

    assert_same_fit(
        &fit_json(&late),
        &clean_json,
        "the run's account was used after end",
    );
}

/// With no usage after `end` at all, a reading past the settlement grace
/// recorded at `begin` is still the window after the run, not a late
/// settlement of its last block.
#[test]
fn a_reading_past_the_settlement_grace_never_reaches_the_last_block() {
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    let clean = StateDir::new();
    seed_burst(&clean, &fixture, TokenKind::ALL.to_vec());
    let clean_json = fit_json(&clean);

    let late = StateDir::new();
    let ended_at = seed_burst_ending(
        &late,
        &fixture,
        TokenKind::ALL.to_vec(),
        RunEnd::AfterLastSettle,
    );
    let grace = ContaminationThresholds::conservative_default().post_settlement_grace();
    let conn = open_test_ledger(&late);
    let chain = meter_chain(&conn);
    let settled_ppm = newest_reading_ppm(&conn);
    let past_grace = ended_at + i64::try_from(grace.as_nanos()).unwrap() + 60 * SECOND;
    reading(&conn, &chain, past_grace, settled_ppm + 130_000);
    drop(conn);

    assert_same_fit(
        &fit_json(&late),
        &clean_json,
        "a reading arrived past the settlement grace",
    );
}

/// The bound must not cut the settlement it exists for: when `end` is
/// recorded before the last block's meter caught up, the settled readings
/// taken after `end` still close that block.
#[test]
fn the_last_block_still_settles_on_readings_taken_after_end() {
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    let clean = StateDir::new();
    seed_burst(&clean, &fixture, TokenKind::ALL.to_vec());
    let clean_json = fit_json(&clean);

    let early_end = StateDir::new();
    seed_burst_ending(
        &early_end,
        &fixture,
        TokenKind::ALL.to_vec(),
        RunEnd::BeforeLastSettle,
    );

    assert_same_fit(
        &fit_json(&early_end),
        &clean_json,
        "end was recorded before the last block settled",
    );
}

/// Usage no marker places on any account is left out and named, so a run
/// whose own sessions lost their markers shows up instead of fitting blind.
#[test]
fn unattributed_usage_in_the_run_window_is_excluded_by_session() {
    let fixture = parse_fixture(INDEPENDENT_ARMS);
    let state = StateDir::new();
    seed_burst(&state, &fixture, TokenKind::ALL.to_vec());
    let conn = open_test_ledger(&state);
    usage_event(
        &conn,
        "orphan-session",
        "orphan-turn",
        FIRST_BLOCK_AT + 5 * SECOND,
        &[(TokenKind::CacheRead, 3_400_000)],
    );
    drop(conn);

    let json = fit_json(&state);
    let clean = StateDir::new();
    seed_burst(&clean, &fixture, TokenKind::ALL.to_vec());
    assert_eq!(
        json["coefficients"],
        fit_json(&clean)["coefficients"],
        "unattributed usage must not reach the blocks"
    );
    let excluded = json["excluded_samples"].as_array().unwrap();
    let orphan = excluded
        .iter()
        .find(|sample| sample["sample_ref"] == "session:claude-code/orphan-session")
        .unwrap_or_else(|| panic!("orphan session not excluded: {excluded:?}"));
    assert!(
        orphan["reason"].as_str().unwrap().contains("unattributed"),
        "{orphan}"
    );
    assert_eq!(json["usable_observations"], 12);
}

const FREE_CACHE_READ: &str =
    include_str!("fixtures/calibration/multivariate-free-cache-read.json");
const FREE_CACHE_READ_GOLDEN: &str = "tests/fixtures/calibration/fit-free-cache-read.golden.json";

/// Fields whose value is the moment the fit ran rather than what it found.
const VOLATILE_FIT_FIELDS: [&str; 2] = ["knowledge_time", "run_id"];

/// A kind that costs nothing is the finding, not a defect: the fit exits 0,
/// records all four coefficients with cache read at zero within its error,
/// and the report matches its golden once the fit's own timestamp is removed.
#[test]
fn a_free_kind_is_recorded_as_a_coefficient_at_zero() {
    let state = StateDir::new();
    let fixture = parse_fixture(FREE_CACHE_READ);
    seed_burst(&state, &fixture, TokenKind::ALL.to_vec());
    let mut json = fit_json(&state);

    let coefficients = json["coefficients"].as_array().unwrap();
    assert_eq!(coefficients.len(), 4);
    for row in coefficients {
        let estimate = row["estimate_ppm_per_token"].as_f64().unwrap();
        let error = row["std_error_ppm_per_token"].as_f64().unwrap();
        if row["token_kind"] == "cache_read" {
            assert!(estimate.abs() <= error, "cache_read {estimate} +- {error}");
        } else {
            assert!(estimate > 0.0, "{row}");
        }
    }
    let conn = open_test_ledger(&state);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM window_calibration_multivariate_coefficient"
        ),
        4
    );

    for field in VOLATILE_FIT_FIELDS {
        json.as_object_mut().unwrap().remove(field);
    }
    round_floats(&mut json);
    let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FREE_CACHE_READ_GOLDEN);
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&golden_path).expect("the golden must exist"),
    )
    .expect("the golden must parse");
    assert_eq!(json, golden, "fit JSON must match {FREE_CACHE_READ_GOLDEN}");
}

/// Rounds every float to nine decimal places, so the golden pins what the fit
/// found rather than the last bits of its arithmetic; an exact design leaves
/// errors near 1e-16 whose trailing digits carry no meaning.
fn round_floats(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(number) if number.is_f64() => {
            let rounded = (number.as_f64().unwrap() * 1e9).round() / 1e9 + 0.0;
            *value = serde_json::json!(rounded);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(round_floats),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(round_floats),
        _ => {}
    }
}
