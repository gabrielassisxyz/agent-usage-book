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
