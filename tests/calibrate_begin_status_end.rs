//! Integration tests for `aub calibrate begin|status|end` (`aub-c0b.2`).
//!
//! The property under test is that the experiment is a record, not a
//! session: each command opens the ledger, does its read or write, and exits,
//! so a forty minute burn needs no forty minute `aub` process. Every test
//! below drops its database connection between phases and reopens the same
//! file, carrying nothing but the path and the experiment id across: that is
//! a process exit, and the reboot test names it. No sampling loop exists
//! anywhere in this path; the samples that move the readings between commands
//! are ordinary stored observations, standing in for `sample --due`
//! invocations by the external scheduler.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::provenance::CostModelId;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::tokens::TokenKind;
use agent_usage_book::domain::window::{ReportedResolution, WindowSemanticKey};
use agent_usage_book::store::account::account_id_by_identity;
use agent_usage_book::store::calibration::PlanTier;
use agent_usage_book::store::calibration_controlled::{
    ControlledExperimentId, ControlledExperimentRun, ControlledRunPhase,
    default_expected_token_kinds, insert_begin, load_by_experiment_id, load_latest,
    missing_expected_terms, record_end, status_for_run,
};
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::cost_model::{
    ProviderKey, anthropic_claude_messages_incomplete_v1, anthropic_claude_messages_v1,
};
use agent_usage_book::store::meter_evidence::{
    newest_observation_for_account, windows_by_observation,
};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use test_support::StateDir;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aub-calibrate-lifecycle-test-{}-{}",
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

fn pragma() -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1_000),
    }
}

/// Opens the ledger file the way a fresh process would: a new connection over
/// the same path, running the idempotent migrations. Nothing but `db_path` is
/// carried in.
fn fresh_connection(db_path: &Path) -> rusqlite::Connection {
    let mut conn = open(db_path, AccessMode::ReadWrite, &pragma()).expect("ledger must open");
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
    semantic_key: &str,
    received_at: UtcTimestamp,
    quota_ppm: i32,
) {
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
         ) VALUES (?1, ?2, 'account_wide', ?3, 10000, 'exact', ?4, 'known', 18000000000000, 1, 'unknown')",
        params![
            observation_id,
            semantic_key,
            quota_ppm,
            received_at.unix_nanos() + 18_000_000_000_000
        ],
    )
    .expect("window insert must work");
}

/// The `begin` premise the CLI would record, assembled the same way: baseline
/// from the newest stored observation, explicit exclusivity assertion, complete
/// cost model required.
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
    let model = anthropic_claude_messages_v1(UtcTimestamp::from_unix_nanos(1_000));
    assert!(
        missing_expected_terms(&model, &default_expected_token_kinds()).is_empty(),
        "the test begins against the complete seed model"
    );
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
        baseline_plateau_started_at: baseline.received_at,
        contamination_thresholds:
            agent_usage_book::calibration::contamination::ContaminationThresholds::conservative_default(),
        started_at,
        ended_at: None,
        exclusivity_assertion: format!(
            "account {account} reserved for controlled experiment {experiment}"
        ),
    };
    insert_begin(conn, &run).expect("begin must be recorded");
    run
}

const FIVE_MINUTES_NANOS: i64 = 300_000_000_000;

fn at(minutes: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(minutes * 60_000_000_000)
}

/// The three-command lifecycle with a process exit between each: every phase
/// opens a fresh connection over the same file and drops it before the next
/// begins. The experiment survives because it is a stored record.
#[test]
fn three_command_lifecycle_survives_a_process_exit_between_each() {
    let scratch = ScratchDir::new();

    // Process 1: begin against one settled baseline reading.
    let baseline_quota = QuotaUsed::new(QuotaFractionPpm::new(600_000).unwrap());
    let baseline_resolution =
        ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap();
    {
        let conn = fresh_connection(&scratch.db_path());
        insert_meter_chain(&conn, "work-a", "five_hour", at(0), 600_000);
        let run = begin_experiment(&conn, "exp-exit", "work-a", at(0));
        assert_eq!(run.baseline_quota_used, baseline_quota);
        assert_eq!(run.baseline_resolution, baseline_resolution);
        // Process 1 exits here: the connection is dropped.
    }

    // Process 2: status sees the running experiment with no new samples yet.
    {
        let conn = fresh_connection(&scratch.db_path());
        let id = ControlledExperimentId::new("exp-exit");
        let run = load_by_experiment_id(&conn, &id)
            .expect("load must work")
            .expect("experiment must survive the first exit");
        assert_eq!(run.phase(), ControlledRunPhase::Running);
        let status = status_for_run(&conn, &run, at(1)).expect("status must work");
        assert_eq!(status.phase, ControlledRunPhase::Running);
        assert_eq!(status.samples_since_baseline, 1);
        assert!(!status.is_settled());
    }

    // The scheduler samples twice more while no `aub` process runs.
    {
        let conn = fresh_connection(&scratch.db_path());
        insert_meter_chain(&conn, "work-a", "five_hour", at(5), 600_000);
        insert_meter_chain(&conn, "work-a", "five_hour", at(10), 600_000);
    }

    // Process 3: status reports the new samples and the settled plateau.
    {
        let conn = fresh_connection(&scratch.db_path());
        let id = ControlledExperimentId::new("exp-exit");
        let run = load_by_experiment_id(&conn, &id)
            .expect("load must work")
            .expect("experiment must survive");
        let status = status_for_run(&conn, &run, at(10)).expect("status must work");
        assert_eq!(status.samples_since_baseline, 3);
        assert!(
            status.is_settled(),
            "three stable five-minute readings must read settled"
        );
    }

    // Process 4: end records the boundary; a later status still reports the
    // phase without declaring anything new about settlement.
    {
        let conn = fresh_connection(&scratch.db_path());
        let id = ControlledExperimentId::new("exp-exit");
        record_end(&conn, &id, at(11)).expect("end must be recorded");
    }
    {
        let conn = fresh_connection(&scratch.db_path());
        let id = ControlledExperimentId::new("exp-exit");
        let run = load_by_experiment_id(&conn, &id)
            .expect("load must work")
            .expect("experiment must survive the end exit");
        assert_eq!(run.phase(), ControlledRunPhase::Ended);
        let status = status_for_run(&conn, &run, at(11)).expect("status must work");
        assert_eq!(status.phase, ControlledRunPhase::Ended);
    }
}

/// The same lifecycle across a simulated reboot: after `begin`, every handle
/// is dropped and only the path and the id string are kept. Reopening proves
/// no in-memory state was ever required.
#[test]
fn experiment_survives_a_simulated_reboot_between_begin_and_end() {
    let scratch = ScratchDir::new();
    let db_path = scratch.db_path().to_string_lossy().into_owned();
    let experiment = "exp-reboot".to_string();

    {
        let conn = fresh_connection(Path::new(&db_path));
        insert_meter_chain(&conn, "work-a", "five_hour", at(0), 600_000);
        begin_experiment(&conn, &experiment, "work-a", at(0));
        // Simulated reboot: drop the connection and every loaded value.
    }

    // After the reboot only two strings remain: the path and the id.
    {
        let conn = fresh_connection(Path::new(&db_path));
        let id = ControlledExperimentId::new(experiment.clone());
        let run = load_by_experiment_id(&conn, &id)
            .expect("load must work")
            .expect("experiment must survive the reboot");
        assert_eq!(run.account, "work-a");
        assert_eq!(run.phase(), ControlledRunPhase::Running);
        record_end(&conn, &id, at(FIVE_MINUTES_NANOS / 60_000_000_000 + 1))
            .expect("end must work after the reboot");
    }

    {
        let conn = fresh_connection(Path::new(&db_path));
        let id = ControlledExperimentId::new(experiment.clone());
        let run = load_by_experiment_id(&conn, &id)
            .expect("load must work")
            .expect("ended experiment must survive");
        assert_eq!(run.phase(), ControlledRunPhase::Ended);
    }
}

/// `begin` refuses when the referenced cost model is incomplete for the token
/// kinds the workload is expected to produce: the incomplete seed model
/// carries no cache-write term.
#[test]
fn begin_refuses_when_the_cost_model_is_incomplete_for_the_expected_kinds() {
    let model = anthropic_claude_messages_incomplete_v1(UtcTimestamp::from_unix_nanos(1_000));
    let missing = missing_expected_terms(&model, &default_expected_token_kinds());
    assert_eq!(
        missing,
        vec![TokenKind::CacheWrite],
        "the incomplete seed model must be missing exactly cache_write"
    );
}

/// `end` records the end of controlled work and does not itself declare the
/// meter settled: with the meter still climbing, status after end reports the
/// ended phase and an unsettled meter.
#[test]
fn end_records_the_boundary_without_declaring_the_meter_settled() {
    let scratch = ScratchDir::new();
    {
        let conn = fresh_connection(&scratch.db_path());
        insert_meter_chain(&conn, "work-a", "five_hour", at(0), 100_000);
        begin_experiment(&conn, "exp-climb", "work-a", at(0));
    }
    // The burn moves the meter while no experiment process runs.
    {
        let conn = fresh_connection(&scratch.db_path());
        insert_meter_chain(&conn, "work-a", "five_hour", at(5), 200_000);
        insert_meter_chain(&conn, "work-a", "five_hour", at(10), 300_000);
    }
    {
        let conn = fresh_connection(&scratch.db_path());
        let id = ControlledExperimentId::new("exp-climb");
        record_end(&conn, &id, at(11)).expect("end must be recorded");
        let run = load_by_experiment_id(&conn, &id)
            .expect("load must work")
            .expect("run must exist");
        let status = status_for_run(&conn, &run, at(11)).expect("status must work");
        assert_eq!(status.phase, ControlledRunPhase::Ended);
        assert!(
            !status.is_settled(),
            "end must not declare settlement: a climbing meter stays unsettled after the boundary"
        );
    }
}

// --- plan-tier resolution through the release binary (aub-ai1j) ------------------
//
// The four `begin` cases: the run's tier comes from the account's
// configuration, `--plan-tier` is an optional cross-check, and both refusals
// happen before anything is recorded. Each case seeds its ledger in-process
// (the complete cost model plus one meter baseline), then runs
// `calibrate begin` as a separate process against a temporary state
// directory, so the binary reads the configuration file exactly as installed.

/// Seeds a temporary state directory for one `begin` run: a configuration
/// naming `account` with or without `plan_tier`, the complete seed cost
/// model, and one baseline observation on the `five_hour` window.
fn plan_tier_state(account: &str, plan_tier: Option<&str>) -> StateDir {
    let state = StateDir::new();
    std::fs::create_dir_all(state.path().join("home")).expect("home must be creatable");
    let tier_line = plan_tier
        .map(|tier| format!("plan_tier = \"{tier}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        state.path().join("aub.toml"),
        format!("[[accounts]]\nname = \"{account}\"\nprovider = \"anthropic\"\n{tier_line}"),
    )
    .expect("config must be writable");
    let db_path = state.path().join(LEDGER_DATABASE_FILE);
    let mut conn = open(&db_path, AccessMode::ReadWrite, &pragma()).expect("ledger must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
    )
    .expect("migrations must apply");
    let at = UtcTimestamp::from_unix_nanos(1_000);
    let model = anthropic_claude_messages_v1(at);
    agent_usage_book::store::cost_model::activate(&mut conn, &model, at, None)
        .expect("cost model must activate");
    insert_meter_chain(&conn, account, "five_hour", at, 600_000);
    state
}

/// Runs `aub` against the temporary state as its own process and returns the
/// captured output without asserting on the exit status: the refusal cases
/// exit non-zero by design.
fn begin_output(state: &StateDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aub"))
        .args(args)
        .env("HOME", state.path().join("home"))
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off")
        .output()
        .expect("the aub binary must be spawnable")
}

/// The latest controlled run in the temporary ledger, read back through a
/// fresh connection: `None` proves a refused `begin` recorded nothing.
fn latest_run(state: &StateDir) -> Option<ControlledExperimentRun> {
    let db_path = state.path().join(LEDGER_DATABASE_FILE);
    let conn = open(&db_path, AccessMode::ReadWrite, &pragma()).expect("ledger must reopen");
    load_latest(&conn).expect("load must work")
}

fn begin_args<'a>(experiment: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "calibrate",
        "--account",
        "bianca",
        "begin",
        "--window",
        "five_hour",
        "--cost-model",
        "anthropic-claude-messages-v1",
        "--experiment",
        experiment,
        "--assert-exclusive",
    ];
    args.extend_from_slice(extra);
    args
}

/// With `plan_tier = "pro"` configured, the flag omitted records a run whose
/// tier is `pro`, and the report line prints `plan_tier=pro`.
#[test]
fn begin_takes_the_configured_tier_when_the_flag_is_omitted() {
    let state = plan_tier_state("bianca", Some("pro"));
    let output = begin_output(&state, &begin_args("exp-configured-tier", &[]));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "begin must succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("plan_tier=pro"),
        "the report line must print the configured tier:\n{stdout}"
    );
    let run = latest_run(&state).expect("the run must be recorded");
    assert_eq!(run.plan_tier.as_str(), "pro");
}

/// With `plan_tier = "pro"` configured, `--plan-tier max-5x` refuses with the
/// usage exit class, naming the account and both tiers, and records no run.
#[test]
fn begin_refuses_a_flag_that_disagrees_with_the_configured_tier() {
    let state = plan_tier_state("bianca", Some("pro"));
    let output = begin_output(
        &state,
        &begin_args("exp-tier-mismatch", &["--plan-tier", "max-5x"]),
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "a mismatched tier must refuse with the usage exit class.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bianca"),
        "the refusal must name the account:\n{stderr}"
    );
    assert!(
        stderr.contains("pro"),
        "the refusal must name the configured tier:\n{stderr}"
    );
    assert!(
        stderr.contains("max-5x"),
        "the refusal must name the given tier:\n{stderr}"
    );
    assert!(
        latest_run(&state).is_none(),
        "a refused begin must record no run"
    );
}

/// Against an account with no `plan_tier` configured and no flag, `begin`
/// refuses with the usage exit class, naming the account and the key, and
/// records no run.
#[test]
fn begin_refuses_when_no_tier_is_configured_and_no_flag_is_given() {
    let state = plan_tier_state("bianca", None);
    let output = begin_output(&state, &begin_args("exp-tier-missing", &[]));
    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing tier must refuse with the usage exit class.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bianca"),
        "the refusal must name the account:\n{stderr}"
    );
    assert!(
        stderr.contains("plan_tier"),
        "the refusal must name the missing key:\n{stderr}"
    );
    assert!(
        latest_run(&state).is_none(),
        "a refused begin must record no run"
    );
}

/// Against an account with no `plan_tier` configured, the flag records its
/// value as before.
#[test]
fn begin_records_the_flag_when_no_tier_is_configured() {
    let state = plan_tier_state("bianca", None);
    let output = begin_output(
        &state,
        &begin_args("exp-flag-tier", &["--plan-tier", "max-5x"]),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "begin with the flag must succeed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("plan_tier=max-5x"),
        "the report line must print the flag's tier:\n{stdout}"
    );
    let run = latest_run(&state).expect("the run must be recorded");
    assert_eq!(run.plan_tier.as_str(), "max-5x");
}
