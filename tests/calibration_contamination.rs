//! Integration tests for controlled-experiment contamination detection (`aub-c0b.6`).
//!
//! The property under test is end to end through the ledger: a synthetic
//! experiment with injected hidden traffic (the meter moves while no local
//! work is attributed) is detected by at least the flat-credits signal, a
//! contaminated run is refused for activation, and every threshold the
//! detector reads comes from the experiment row `begin` recorded.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::calibration::contamination::{ContaminationSignal, ContaminationThresholds};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::provenance::CostModelId;
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::window::WindowSemanticKey;
use agent_usage_book::store::account::account_id_by_identity;
use agent_usage_book::store::calibration::PlanTier;
use agent_usage_book::store::calibration_controlled::{
    ControlledExperimentId, ControlledExperimentRun, baseline_plateau_start_for,
    default_expected_token_kinds, evaluate_contamination_for_run, insert_begin,
    refuse_activation_for_contaminated_run,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::cost_model::ProviderKey;
use agent_usage_book::store::meter_evidence::{
    ObservationRowId, newest_observation_for_account, windows_by_observation,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aub-calibration-contamination-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn db_path(&self) -> PathBuf {
        self.0.join("ledger.db")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_fixture_db(path: &std::path::Path) -> rusqlite::Connection {
    let mut conn = open(
        path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("ledger must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .expect("migrations must apply");
    conn
}

fn insert_meter_chain(
    conn: &rusqlite::Connection,
    account: &str,
    received_at: UtcTimestamp,
    quota_ppm: i32,
) -> ObservationRowId {
    use rusqlite::params;
    let account_id: i64 = conn
        .query_row(
            "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
             VALUES (?1, 'anthropic', ?2, ?2)
             ON CONFLICT (provider_key, logical_name) DO UPDATE SET
                 last_observed_at = MAX(last_observed_at, excluded.last_observed_at)
             RETURNING id",
            params![account, received_at.unix_nanos()],
            |row| row.get(0),
        )
        .expect("account insert must work");
    let run_id: i64 = conn
        .query_row(
            "INSERT INTO sample_run (trigger, started_at, ended_at, aub_version, configuration_fingerprint)
             VALUES ('manual', ?1, NULL, 'test', 'fp') RETURNING id",
            params![received_at.unix_nanos()],
            |row| row.get(0),
        )
        .expect("sample run insert must work");
    let snapshot_id: i64 = conn
        .query_row(
            "INSERT INTO sampling_policy_snapshot (
                account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
                reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version
             ) VALUES (?1, ?2, 3600000000000, 300000000000, 'lead-60s', 'none', 10000000000, 'v1')
             RETURNING id",
            params![account_id, received_at.unix_nanos()],
            |row| row.get(0),
        )
        .expect("policy snapshot insert must work");
    let attempt_id: i64 = conn
        .query_row(
            "INSERT INTO meter_attempt (
                run_id, account_id, provider, request_started_at, policy_snapshot_id,
                due_at, due_reason, provider_contract_id, meter_semantics_id
             ) VALUES (?1, ?2, 'anthropic', ?3, ?4, ?3, 'forced_or_manual', 'contract-v1', 'meter-v1')
             RETURNING id",
            params![run_id, account_id, received_at.unix_nanos(), snapshot_id],
            |row| row.get(0),
        )
        .expect("attempt insert must work");
    let evidence_id: i64 = conn
        .query_row(
            "INSERT INTO meter_response_evidence (
                attempt_id, response_classification, received_at, evidence_capsule,
                capsule_schema_version, sanitizer_version, content_hash, capture_truncated
             ) VALUES (?1, 'success', ?2, 'capsule', 'v1', 'v1', 'hash', 0) RETURNING id",
            params![attempt_id, received_at.unix_nanos()],
            |row| row.get(0),
        )
        .expect("evidence insert must work");
    let observation_id: i64 = conn
        .query_row(
            "INSERT INTO meter_observation (
                attempt_id, evidence_id, account_id, provider, received_at,
                measurement_basis, adapter_version, provider_contract_id,
                meter_semantics_id, normalized_fingerprint
             ) VALUES (?1, ?2, ?3, 'anthropic', ?4, 'locally_received', 'adapter-v1',
                'contract-v1', 'meter-v1', 'fingerprint') RETURNING id",
            params![
                attempt_id,
                evidence_id,
                account_id,
                received_at.unix_nanos()
            ],
            |row| row.get(0),
        )
        .expect("observation insert must work");
    conn.execute(
        "INSERT INTO meter_window (
            observation_id, semantic_key, scope_kind, quota_used_ppm,
            reported_resolution_ppm, quantization, resets_at, reset_state,
            nominal_duration_nanos, is_active, severity
         ) VALUES (?1, 'five_hour', 'account_wide', ?2, 10000, 'exact', ?3, 'known', 18000000000000, 1, 'unknown')",
        params![
            observation_id,
            quota_ppm,
            received_at.unix_nanos() + 18_000_000_000_000
        ],
    )
    .expect("window insert must work");
    ObservationRowId::new(observation_id)
}

/// Records `begin` the way the CLI does: baseline from the newest stored
/// observation, plateau asserted by scanning the trailing stable run,
/// thresholds recorded from configuration (here the conservative defaults).
fn begin_experiment(
    conn: &rusqlite::Connection,
    experiment: &str,
    account: &str,
    started_at: UtcTimestamp,
) -> ControlledExperimentRun {
    let account_id = account_id_by_identity(conn, "anthropic", account)
        .expect("account lookup must work")
        .expect("account must exist");
    let baseline = newest_observation_for_account(conn, account_id)
        .expect("baseline lookup must work")
        .expect("baseline must exist");
    let windows = windows_by_observation(conn, baseline.row_id).expect("windows must load");
    let window = windows
        .iter()
        .find(|entry| entry.semantic_key.as_str() == "five_hour")
        .expect("five_hour window must exist");
    let thresholds = ContaminationThresholds::conservative_default();
    let plateau_start = baseline_plateau_start_for(
        conn,
        "anthropic",
        account,
        &WindowSemanticKey::new("five_hour"),
        window.quota_used,
        baseline.received_at,
        thresholds.pre_burn_max_movement_ppm(),
    )
    .expect("plateau scan must work");
    let run = ControlledExperimentRun {
        id: ControlledExperimentId::new(experiment),
        account: account.to_string(),
        provider: ProviderKey::new("anthropic"),
        plan_tier: PlanTier::new("pro-5h"),
        window_semantic_key: WindowSemanticKey::new("five_hour"),
        cost_model_id: CostModelId::new("anthropic-claude-messages-v1"),
        expected_token_kinds: default_expected_token_kinds(),
        baseline_observation_id: baseline.row_id,
        baseline_quota_used: window.quota_used,
        baseline_resolution: window.reported_resolution,
        baseline_observed_at: baseline.received_at,
        baseline_plateau_started_at: plateau_start,
        contamination_thresholds: thresholds,
        started_at,
        ended_at: None,
        exclusivity_assertion: format!(
            "account {account} reserved for controlled experiment {experiment}"
        ),
    };
    insert_begin(conn, &run).expect("begin must be recorded");
    run
}

fn at(minutes: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(minutes * 60_000_000_000)
}

/// A synthetic experiment with injected hidden traffic: the meter climbs by
/// ten percentage points while no local credits are attributed. At least the
/// flat-credits signal must detect it.
#[test]
fn synthetic_experiment_with_injected_hidden_traffic_is_detected() {
    let scratch = ScratchDir::new();
    let conn = open_fixture_db(&scratch.db_path());
    insert_meter_chain(&conn, "work-a", at(0), 100_000);
    let run = begin_experiment(&conn, "exp-hidden", "work-a", at(0));
    // Hidden traffic, standing in for work no local transcript records.
    insert_meter_chain(&conn, "work-a", at(5), 150_000);
    insert_meter_chain(&conn, "work-a", at(10), 200_000);

    let verdict = evaluate_contamination_for_run(&conn, &run, Credits::from_micros(0), at(10))
        .expect("evaluation must work");
    assert!(verdict.is_contaminated());
    let flat = verdict.findings_for(ContaminationSignal::FlatCreditsWithMeterMovement);
    assert_eq!(
        flat.len(),
        1,
        "injected hidden traffic must fire the flat-credits signal"
    );
    assert!(
        flat[0].summary().contains("100000"),
        "the finding must report the 100000 ppm meter movement, got: {}",
        flat[0].summary()
    );
}

/// The same meter climb with real local credits attributed stays clean on the
/// flat-credits signal: the movement is explained.
#[test]
fn explained_meter_climb_with_local_credits_is_not_flat_credits_contamination() {
    let scratch = ScratchDir::new();
    let conn = open_fixture_db(&scratch.db_path());
    insert_meter_chain(&conn, "work-a", at(0), 100_000);
    let run = begin_experiment(&conn, "exp-explained", "work-a", at(0));
    insert_meter_chain(&conn, "work-a", at(5), 150_000);
    insert_meter_chain(&conn, "work-a", at(10), 200_000);

    let verdict =
        evaluate_contamination_for_run(&conn, &run, Credits::from_micros(9_000_000_000), at(10))
            .expect("evaluation must work");
    assert!(
        verdict
            .findings_for(ContaminationSignal::FlatCreditsWithMeterMovement)
            .is_empty()
    );
}

/// A contaminated run is refused for activation, and the refusal names the
/// firing signal.
#[test]
fn contaminated_run_is_refused_for_activation() {
    let scratch = ScratchDir::new();
    let conn = open_fixture_db(&scratch.db_path());
    insert_meter_chain(&conn, "work-a", at(0), 100_000);
    let run = begin_experiment(&conn, "exp-refused", "work-a", at(0));
    insert_meter_chain(&conn, "work-a", at(5), 200_000);

    let refusal =
        refuse_activation_for_contaminated_run(&conn, &run, Credits::from_micros(0), at(5))
            .unwrap_err();
    assert!(refusal.to_string().contains("contaminated"), "{refusal}");
    assert!(
        refusal
            .to_string()
            .contains(ContaminationSignal::FlatCreditsWithMeterMovement.label()),
        "{refusal}"
    );
}

/// An overlapping session marked against the same account inside the window is
/// reported by name from the marker timeline.
#[test]
fn overlapping_session_is_reported_from_the_marker_timeline() {
    let scratch = ScratchDir::new();
    let conn = open_fixture_db(&scratch.db_path());
    insert_meter_chain(&conn, "work-a", at(0), 100_000);
    let run = begin_experiment(&conn, "exp-overlap", "work-a", at(0));
    insert_meter_chain(&conn, "work-a", at(5), 100_000);
    insert_meter(
        &conn,
        "claude-code",
        "sess-intruder",
        "work-a",
        at(3).unix_nanos(),
    );

    let verdict =
        evaluate_contamination_for_run(&conn, &run, Credits::from_micros(9_000_000_000), at(5))
            .expect("evaluation must work");
    let overlap = verdict.findings_for(ContaminationSignal::OverlappingSession);
    assert_eq!(overlap.len(), 1);
    assert!(
        overlap[0].summary().contains("claude-code/sess-intruder"),
        "the finding must name the overlapping session, got: {}",
        overlap[0].summary()
    );
}

fn insert_meter(
    conn: &rusqlite::Connection,
    source: &str,
    native: &str,
    account: &str,
    observed_at: i64,
) {
    insert_marker(
        conn,
        &NewSessionAccountMarker {
            session_id: SessionId::new(SourceNamespace::new(source), NativeSessionId::new(native)),
            observed_at: UtcTimestamp::from_unix_nanos(observed_at),
            source_ordering_key: None,
            logical_account: account.to_string(),
            resolved_account_id: None,
            marker_source: MarkerSource::new("hook"),
            run_id: None,
            evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
        },
    )
    .expect("marker insert must work");
}

// --- process level: `aub calibrate fit` reports the verdict, `activate` enforces it (aub-lhfh)

mod through_fit_and_activate {
    use std::process::Command;

    use agent_usage_book::calibration::contamination::ContaminationThresholds;
    use agent_usage_book::domain::attempt::AttemptOutcome;
    use agent_usage_book::domain::ids::{
        AdapterVersion, MeterSemanticsId, NativeSessionId, ProviderContractId, SessionId,
        SourceNamespace,
    };
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
    use agent_usage_book::store::meter_attempt::{
        DueReason, NewMeterAttempt, NewMeterAttemptResult,
    };
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
        account as account_store, calibration as calibration_store, connection,
        meter_attempt as attempt_store, meter_evidence as evidence_store, migrate, migrations,
        sample_run as run_store, sampling_policy_snapshot as snapshot_store,
        usage_component as component_store, usage_event as event_store,
    };
    use agent_usage_book::transcripts::parser::ParserVersion;
    use rusqlite::Connection;
    use test_support::StateDir;

    const SECOND: i64 = 1_000_000_000;
    const EXPERIMENT: &str = "exp-contamination-process";
    const ACCOUNT: &str = "work-a";
    const HOLDOUT_ACCOUNT: &str = "holdout";
    const WINDOW: &str = "five_hour";
    const SESSION_SOURCE: &str = "claude-code";
    const RUN_SESSION: &str = "run-session";
    const BASELINE_PPM: i64 = 100_000;
    const BLOCKS: i64 = 4;
    /// 200,000 output tokens are 3 credits under the built-in cost model, and
    /// each block moves the meter 30,000 ppm: 100 micros per point.
    const OUTPUT_TOKENS_PER_BLOCK: u64 = 200_000;
    const PPM_PER_BLOCK: i64 = 30_000;
    const T0: i64 = 1_000 * SECOND;
    const ENDED_AT: i64 = T0 + 60 * SECOND * (BLOCKS + 1);
    /// The window every reading reports resets this long after the reading.
    const RESET_AFTER: i64 = 18_000 * SECOND;
    /// The settlement grace `begin` records by default.
    const GRACE: i64 = 3_600 * SECOND;

    /// What the run's account does after the settlement grace.
    #[derive(Clone, Copy)]
    enum AfterGrace {
        /// Two settled readings, neither moving.
        Quiet,
        /// The meter moves this many ppm between two readings past the grace.
        Moves(i64),
        /// A quiet tail, then the window resets and its next instance reads zero.
        QuietThenReset,
    }

    fn aub(state: &StateDir, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_aub"))
            .args(args)
            .env("AUB_STATE_DIR", state.path())
            .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
            .env("AUB_LOG_LEVEL", "off")
            .current_dir(state.path())
            .output()
            .expect("aub binary must run")
    }

    fn ledger(state: &StateDir) -> Connection {
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

    struct Chain {
        account_id: agent_usage_book::store::account::AccountId,
        run_id: agent_usage_book::store::sample_run::SampleRunId,
        snapshot_id: agent_usage_book::store::sampling_policy_snapshot::SamplingPolicySnapshotId,
    }

    fn chain_for(conn: &Connection, account: &str) -> Chain {
        let at = UtcTimestamp::from_unix_nanos(100 * SECOND);
        let account_id = account_store::observe_account(conn, "anthropic", account, at).unwrap();
        let run_id =
            run_store::start_sample_run(conn, run_store::Trigger::Manual, at, "seed").unwrap();
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
        Chain {
            account_id,
            run_id,
            snapshot_id,
        }
    }

    fn reading(conn: &Connection, chain: &Chain, at_nanos: i64, used_ppm: i64) -> ObservationRowId {
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
                evidence_capsule: format!(
                    "{{\"account\":{},\"at\":{at_nanos},\"used\":{used_ppm}}}",
                    chain.account_id.value()
                ),
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
                measurement_basis:
                    agent_usage_book::domain::time::MeasurementBasis::ProviderObserved,
                observed_plan: Some("max-5x".into()),
                observed_tier: Some("max-5x".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("contract-v1"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: format!("fp-{}-{at_nanos}", chain.account_id.value()),
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
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::Exact,
                resets_at: UtcTimestamp::from_unix_nanos(at_nanos + RESET_AFTER).into(),
                nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            },
        )
        .unwrap();
        observation_id
    }

    fn mark_session(conn: &Connection, native: &str, account: &str, at_nanos: i64) {
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

    fn spend_output(conn: &Connection, index: i64, at_nanos: i64) {
        let canonical_id = format!("run-block-{index}");
        let ts = UtcTimestamp::from_unix_nanos(at_nanos);
        let event_id = event_store::insert_event(
            conn,
            &NewUsageEvent {
                canonical_event_id: &canonical_id,
                session_id: Some(RUN_SESSION),
                event_timestamp: Some(ts),
                model_id: Some("claude-opus"),
                evidence_kind: "transcript",
                source_provenance: "test",
                parser_version: "v1",
                created_at: ts,
            },
        )
        .unwrap();
        component_store::insert_component(
            conn,
            &NewUsageComponent {
                event_id,
                token_class: TokenKind::Output.label(),
                count: OUTPUT_TOKENS_PER_BLOCK,
            },
        )
        .unwrap();
        insert_occurrence(
            conn,
            &NewUsageOccurrence {
                source_namespace: &SourceNamespace::new(SESSION_SOURCE),
                native_event_id: Some(&canonical_id),
                parser_version: &ParserVersion::new("claude-code-1"),
                heuristic_key: None,
                source_file: &format!("corpus/{RUN_SESSION}.jsonl"),
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

    /// Seeds a one-kind controlled run of four output blocks, ended at
    /// `ENDED_AT`, with `after_grace` shaping the run account's readings past
    /// the settlement grace, and two readings of a held-out account that
    /// promotion validates against. `intruder` marks another session on the
    /// run's account inside the run.
    fn seed_run(state: &StateDir, after_grace: AfterGrace, intruder: bool) {
        let mut conn = ledger(state);
        seed_initial_cost_model(&mut conn, UtcTimestamp::from_unix_nanos(500 * SECOND)).unwrap();
        let chain = chain_for(&conn, ACCOUNT);
        let baseline = reading(&conn, &chain, T0, BASELINE_PPM);
        let run = ControlledExperimentRun {
            id: ControlledExperimentId::new(EXPERIMENT),
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
            baseline_observed_at: UtcTimestamp::from_unix_nanos(T0),
            baseline_plateau_started_at: UtcTimestamp::from_unix_nanos(T0),
            contamination_thresholds: ContaminationThresholds::conservative_default(),
            started_at: UtcTimestamp::from_unix_nanos(T0 + SECOND),
            ended_at: None,
            exclusivity_assertion: format!("account {ACCOUNT} reserved for {EXPERIMENT}"),
        };
        insert_begin(&conn, &run).unwrap();
        // Marked before `begin`, so the run's own session is not an overlap.
        mark_session(&conn, RUN_SESSION, ACCOUNT, T0);

        let mut used = BASELINE_PPM;
        for index in 0..BLOCKS {
            let t = T0 + 60 * SECOND * (index + 1);
            spend_output(&conn, index, t);
            used += PPM_PER_BLOCK;
            reading(&conn, &chain, t + 30 * SECOND, used);
        }
        if intruder {
            mark_session(&conn, "intruder-session", ACCOUNT, T0 + 150 * SECOND);
        }
        record_end(&conn, &run.id, UtcTimestamp::from_unix_nanos(ENDED_AT)).unwrap();

        let past_grace = ENDED_AT + GRACE;
        match after_grace {
            AfterGrace::Quiet => {
                reading(&conn, &chain, past_grace + 100 * SECOND, used);
                reading(&conn, &chain, past_grace + 200 * SECOND, used);
            }
            AfterGrace::Moves(ppm) => {
                reading(&conn, &chain, past_grace + 100 * SECOND, used);
                reading(&conn, &chain, past_grace + 200 * SECOND, used + ppm);
            }
            AfterGrace::QuietThenReset => {
                reading(&conn, &chain, past_grace + 100 * SECOND, used);
                reading(&conn, &chain, past_grace + 200 * SECOND, used);
                // The last reading before `end` names this reset instant.
                let reset = T0 + 60 * SECOND * BLOCKS + 30 * SECOND + RESET_AFTER;
                reading(&conn, &chain, reset + 2 * SECOND, 0);
                reading(&conn, &chain, reset + 300 * SECOND, 0);
            }
        }

        // The held-out account moves by exactly the third block's credits at
        // the run's coefficient, so promotion's held-out residual is zero.
        let holdout = chain_for(&conn, HOLDOUT_ACCOUNT);
        let third_block = T0 + 180 * SECOND;
        reading(&conn, &holdout, third_block - 7 * SECOND, BASELINE_PPM);
        reading(
            &conn,
            &holdout,
            third_block + 23 * SECOND,
            BASELINE_PPM + PPM_PER_BLOCK,
        );
        std::fs::write(state.path().join("aub.toml"), "").unwrap();
    }

    fn evidence_of(conn: &Connection, account: &str) -> String {
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

    fn fit_json(state: &StateDir) -> serde_json::Value {
        let fit = aub(
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
        assert_eq!(
            fit.status.code(),
            Some(0),
            "fit: {}",
            String::from_utf8_lossy(&fit.stderr)
        );
        serde_json::from_slice(&fit.stdout).expect("fit stdout must be JSON")
    }

    /// Promotes the fitted candidate, then activates the result; returns the
    /// activation's output.
    fn promote_and_activate(state: &StateDir, fit: &serde_json::Value) -> std::process::Output {
        let conn = ledger(state);
        let training = evidence_of(&conn, ACCOUNT);
        let validation = evidence_of(&conn, HOLDOUT_ACCOUNT);
        drop(conn);
        let candidate_id = fit["candidate_id"].as_str().unwrap().to_string();
        let promote = aub(
            state,
            &[
                "calibrate",
                "promote",
                &candidate_id,
                "--training",
                &training,
                "--validation",
                &validation,
            ],
        );
        assert_eq!(
            promote.status.code(),
            Some(0),
            "promote: {}",
            String::from_utf8_lossy(&promote.stderr)
        );
        aub(
            state,
            &[
                "calibrate",
                "activate",
                &format!("promoted-{candidate_id}"),
                "--actor",
                "operator",
                "--training",
                &training,
                "--validation",
                &validation,
                // Wide enough that the late movement's own held-out residual
                // (30,000 micros) passes: the contaminated run is refused for
                // contamination alone.
                "--max-residual-micros",
                "100000",
            ],
        )
    }

    fn lifecycle_events(state: &StateDir) -> i64 {
        calibration_store::count_calibration_lifecycles(&ledger(state)).unwrap()
    }

    fn signals(fit: &serde_json::Value) -> Vec<String> {
        fit["contamination"]["findings"]
            .as_array()
            .expect("findings must be an array")
            .iter()
            .map(|finding| finding["signal"].as_str().unwrap().to_string())
            .collect()
    }

    /// The meter moving 50,000 ppm past the grace, against the recorded
    /// tolerance of 10,000, is extended settlement drift: `fit` reports the run
    /// contaminated by it, and its promoted result is refused activation.
    #[test]
    fn late_movement_past_the_grace_is_reported_by_fit_and_refused_by_activate() {
        let state = StateDir::new();
        seed_run(&state, AfterGrace::Moves(50_000), false);

        let fit = fit_json(&state);
        assert_eq!(fit["contamination"]["verdict"], "contaminated", "{fit}");
        assert_eq!(signals(&fit), vec!["extended_settlement_drift"]);
        let detail = fit["contamination"]["findings"][0]["detail"]
            .as_str()
            .unwrap();
        assert!(detail.contains("50000 ppm"), "{detail}");
        assert_eq!(fit["contamination"]["refuses_activation"], true);

        let activate = promote_and_activate(&state, &fit);
        let stderr = String::from_utf8_lossy(&activate.stderr).into_owned();
        assert_ne!(activate.status.code(), Some(0), "activation must refuse");
        assert!(
            stderr.contains(&format!(
                "controlled experiment '{EXPERIMENT}' is contaminated"
            )),
            "the refusal must be the contaminated-run refusal: {stderr}"
        );
        assert!(stderr.contains("extended_settlement_drift"), "{stderr}");
        assert_eq!(lifecycle_events(&state), 0, "a refusal writes nothing");
    }

    /// The same run with a quiet meter past the grace is clean and activates.
    #[test]
    fn the_same_run_without_late_movement_is_clean_and_activates() {
        let state = StateDir::new();
        seed_run(&state, AfterGrace::Quiet, false);

        let fit = fit_json(&state);
        assert_eq!(fit["contamination"]["verdict"], "clean", "{fit}");
        assert!(signals(&fit).is_empty());
        assert_eq!(fit["contamination"]["refuses_activation"], false);

        let activate = promote_and_activate(&state, &fit);
        assert_eq!(
            activate.status.code(),
            Some(0),
            "a clean run must activate: {}",
            String::from_utf8_lossy(&activate.stderr)
        );
        assert_eq!(lifecycle_events(&state), 1);
    }

    /// A window reset after the grace drops the meter to the next window's
    /// usage. That reading measures another window, so the run stays clean,
    /// as `cal-2026-09-14-bianca` would not with an unbounded tail.
    #[test]
    fn a_window_reset_after_the_grace_is_not_settlement_drift() {
        let state = StateDir::new();
        seed_run(&state, AfterGrace::QuietThenReset, false);

        let fit = fit_json(&state);
        assert_eq!(fit["contamination"]["verdict"], "clean", "{fit}");
        let activate = promote_and_activate(&state, &fit);
        assert_eq!(
            activate.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&activate.stderr)
        );
    }

    /// Another session on the run's account inside the run is reported, and
    /// on its own does not refuse activation: the marker timeline cannot tell
    /// a run's own arm sessions from another's.
    #[test]
    fn an_overlapping_session_alone_is_reported_without_refusing_activation() {
        let state = StateDir::new();
        seed_run(&state, AfterGrace::Quiet, true);

        let fit = fit_json(&state);
        assert_eq!(fit["contamination"]["verdict"], "contaminated", "{fit}");
        assert_eq!(signals(&fit), vec!["overlapping_session"]);
        assert_eq!(fit["contamination"]["refuses_activation"], false);

        let activate = promote_and_activate(&state, &fit);
        assert_eq!(
            activate.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&activate.stderr)
        );
        assert_eq!(lifecycle_events(&state), 1);
    }

    /// The text report prints the verdict and one line per finding.
    #[test]
    fn the_text_report_prints_the_verdict_and_each_finding() {
        let state = StateDir::new();
        seed_run(&state, AfterGrace::Moves(50_000), true);

        let fit = aub(&state, &["calibrate", "fit", "--experiment", EXPERIMENT]);
        let stdout = String::from_utf8_lossy(&fit.stdout).into_owned();
        assert_eq!(fit.status.code(), Some(0), "{stdout}");
        assert!(
            stdout.contains("Contamination: contaminated; activation will refuse"),
            "{stdout}"
        );
        assert!(
            stdout.contains("  - extended_settlement_drift: quota moved 50000 ppm"),
            "{stdout}"
        );
        assert!(
            stdout.contains("  - overlapping_session: 1 other session(s)"),
            "{stdout}"
        );
    }
}
