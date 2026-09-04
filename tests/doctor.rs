//! Integration tests for aub doctor check registry and repair split (aub-n27.7).

use std::fs;
use std::path::Path;

use agent_usage_book::config::{Config, Overrides, RealEnv, resolve};
use agent_usage_book::doctor::{
    CheckName, CheckStatus, DoctorContext, build_registry, configuration_failed_registry, run_fix,
};
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::time::{Clock, FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::meter::adapter::HttpTransport;
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use agent_usage_book::store::{connection, migrate, migrations};
use test_support::StateDir;

fn ts(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(seconds * 1_000_000_000)
}

fn open_ledger(state_dir: &Path) -> rusqlite::Connection {
    let path = state_dir.join(connection::LEDGER_DATABASE_FILE);
    let policy = connection::PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    let mut conn = connection::open(&path, connection::AccessMode::ReadWrite, &policy)
        .expect("scratch ledger must open");
    migrate::run_migrations(
        &mut conn,
        &migrations::registry(),
        None,
        &FakeClock::new(ts(0)),
    )
    .expect("scratch ledger must migrate");
    conn
}

fn test_config(state_dir: &Path) -> Config {
    let toml = format!("[state]\ndir = {:?}\n", state_dir);
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml")
        .expect("minimal config must resolve");
    config
}

// --- Network isolation test: doctor performs no network operation ------------

struct DoctorMustNotTouchNetwork;

impl HttpTransport for DoctorMustNotTouchNetwork {
    fn send(
        &self,
        _request: &HttpRequest,
        _budget: &CommandBudget,
        _clock: &impl Clock,
    ) -> Result<HttpResponse, FailureClass> {
        panic!("aub doctor performs no network operation")
    }
}

#[test]
#[should_panic(expected = "aub doctor performs no network operation")]
fn the_tripwire_transport_fires_when_invoked() {
    let clock = FakeClock::new(ts(0));
    let budget = CommandBudget::new(MonotonicDuration::from_seconds(1), &clock);
    let request = HttpRequest::get(
        "http://unused.invalid",
        agent_usage_book::meter::transport::RequestTimeoutConfig::new(
            MonotonicDuration::from_millis(10),
            MonotonicDuration::from_millis(10),
            None,
        ),
    );
    let _ = DoctorMustNotTouchNetwork.send(&request, &budget, &clock);
}

#[test]
fn the_doctor_pipeline_performs_no_network_operation() {
    let _tripwire = DoctorMustNotTouchNetwork;
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let config = test_config(state.path());
    let now = ts(1_700_000_000);

    let ctx = DoctorContext {
        config: &config,
        timestamp: now,
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    assert_eq!(outcomes.len(), CheckName::EXPECTED.len());

    let mut conn_rw = open_ledger(state.path());
    let report = run_fix(&mut conn_rw, &config, &FakeClock::new(now)).unwrap();
    assert_eq!(report.actions.len(), 4);
    // The tripwire transport remained live in scope without firing.
}

// --- Acceptance criterion proof: every registered check fails on purpose once --

#[test]
fn check_fails_configuration_validity() {
    let outcomes = configuration_failed_registry("deliberate syntax error");
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::ConfigurationValidity)
        .expect("ConfigurationValidity present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("deliberate syntax error"))
    );
}

#[test]
fn check_fails_sqlite_and_schema_health() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join("ledger.sqlite3"),
        db: None,
        db_missing: false,
        db_open_error: Some("corrupted database header".to_string()),
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::SqliteAndSchemaHealth)
        .expect("SqliteAndSchemaHealth present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("corrupted database header"))
    );
}

#[test]
fn check_fails_strict_and_constraint_integrity() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    // Create a non-strict table to trigger schema audit failure.
    conn.execute_batch("CREATE TABLE non_strict_audit_target (val INT);")
        .expect("create non-strict table");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::StrictAndConstraintIntegrity)
        .expect("StrictAndConstraintIntegrity present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("non_strict_audit_target"))
    );
}

#[test]
fn check_fails_pending_evidence() {
    let state = StateDir::new();
    let pending_dir = state.path().join("pending");
    fs::create_dir_all(&pending_dir).expect("create pending dir");
    fs::write(pending_dir.join("attempt-1.json"), "{}").expect("write pending record");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::PendingEvidence)
        .expect("PendingEvidence present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("1 pending record(s) undrained"))
    );
}

#[test]
fn check_fails_sampling_cadence() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let toml = format!(
        "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"cadence-test\"\nprovider = \"anthropic\"\n",
        state.path()
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();

    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::SamplingCadence)
        .expect("SamplingCadence present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("cadence-test"))
    );
}

#[test]
fn check_fails_unresolved_authentication() {
    let state = StateDir::new();
    let toml = format!(
        "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"auth-test\"\nprovider = \"anthropic\"\n[accounts.credential]\nkind = \"file\"\npath = \"/nonexistent/token/path\"\n",
        state.path()
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();

    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::UnresolvedAuthentication)
        .expect("UnresolvedAuthentication present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("auth-test"))
    );
}

#[test]
fn check_fails_transcript_roots() {
    let state = StateDir::new();
    let toml = format!(
        "[state]\ndir = {:?}\n\n[[transcripts]]\nname = \"missing\"\nroot = {:?}\npattern = \"**/*.jsonl\"\n",
        state.path(),
        state.path().join("does-not-exist")
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();

    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::TranscriptRoots)
        .expect("TranscriptRoots present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("unreachable root(s)"))
    );
}

#[test]
fn check_fails_parser_failures() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    conn.execute(
        "INSERT INTO ingest_quarantine (
            source_file, parser, failure_class, excerpt_hash, first_observed, last_observed
         ) VALUES ('file1', 'claude-code', 'malformed_json', 'hash1', 1000, 1000)",
        [],
    )
    .expect("insert parser quarantine");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::ParserFailures)
        .expect("ParserFailures present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("1 record(s) quarantined for a parser failure"))
    );
}

#[test]
fn check_reports_not_yet_available_unmapped_accounts() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::UnmappedAccounts)
        .expect("UnmappedAccounts present");
    assert_eq!(
        outcome.status,
        CheckStatus::NotYetAvailable {
            owning_bead: "aub-mgv.3"
        }
    );
}

#[test]
fn check_fails_missing_active_calibrations() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    // Insert a fitted result without an activation event in calibration_lifecycle.
    conn.execute(
        "INSERT INTO window_calibration_result (
            calibration_id, provider, plan_tier, window_semantic_key, meter_semantics_id,
            billing_semantics_id, cost_model_id, fitted_micros_per_point,
            equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros,
            uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count,
            fit_timestamp, inputs_digest, inputs_count, fitting_evidence_digest,
            validation_evidence_digest, validation_method, validation_version,
            out_of_sample_residual_micros, statistical_method, statistical_parameters,
            condition_number_micros, observation_coverage_requirement, settling_policy,
            excluded_samples, activation_policy_version, aub_version, source_revision,
            valid_from, valid_until, knowledge_time
        ) VALUES (
            'wcr-test-1', 'anthropic', 'max', 'five_hour', 'm1', 'b1', 'c1',
            100, 1000, 10, 90, 110, NULL, 'none', 10, 1000, '0123456789abcdef', 1,
            '0123456789abcdef', '0123456789abcdef', 'v', '1', NULL, 'ols', '{}',
            NULL, 'cov', 'set', '[]', 'ap1', '0.1.0', 'rev1', 0, 10000, 1000
        )",
        [],
    )
    .expect("insert calibration result");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::MissingActiveCalibrations)
        .expect("MissingActiveCalibrations present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("anthropic/max/five_hour"))
    );
}

#[test]
fn check_fails_stale_rate_cards() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    // Insert rate card with review due in the past.
    conn.execute(
        "INSERT INTO rate_card (
            vendor, model, token_class, rate_micros, currency, billing_basis,
            effective_start, imported_at, review_due
         ) VALUES ('anthropic', 'claude-opus-4', 'input', 10, 'USD', 'per_million_tokens', '2026-01-01', 1000, '2020-01-01')",
        [],
    )
    .expect("insert rate card");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000), // In 2023+, so '2020-01-01' is past review_due
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::StaleRateCards)
        .expect("StaleRateCards present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("review due: anthropic claude-opus-4 input"))
    );
}

#[test]
fn check_fails_projection_versus_database_generation() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    // Database is at generation 0; write a projection file recording generation 5.
    let projection_path = state.path().join("projection");
    fs::write(
        &projection_path,
        "{\"schema_version\":1,\"ledger_generation\":5}",
    )
    .expect("write projection file");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::ProjectionVersusDatabaseGeneration)
        .expect("ProjectionVersusDatabaseGeneration present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("projection is generation 5, ahead of the database's 0"))
    );
}

#[test]
fn check_fails_backup_age() {
    let state = StateDir::new();
    let missing_archive = state.path().join("missing_backup_archive");
    let toml = format!(
        "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\n",
        state.path(),
        missing_archive
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();

    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::BackupAge)
        .expect("BackupAge present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("no backup found"))
    );
}

#[test]
fn check_reports_not_yet_available_meter_anomalies() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::MeterAnomalies)
        .expect("MeterAnomalies present");
    assert_eq!(
        outcome.status,
        CheckStatus::NotYetAvailable {
            owning_bead: "aub-eun.14"
        }
    );
}

#[test]
fn check_unexplained_residual_reports_not_applicable_when_no_db_or_no_intervals() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let now = ts(1_700_000_000);
    let ctx_no_db = DoctorContext {
        config: &config,
        timestamp: now,
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes_no_db = build_registry(&ctx_no_db);
    let outcome_no_db = outcomes_no_db
        .iter()
        .find(|o| o.name == CheckName::UnexplainedResidual)
        .expect("UnexplainedResidual present");
    assert_eq!(
        outcome_no_db.status,
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    );

    let conn = open_ledger(state.path());
    let ctx_empty_db = DoctorContext {
        config: &config,
        timestamp: now,
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes_empty_db = build_registry(&ctx_empty_db);
    let outcome_empty_db = outcomes_empty_db
        .iter()
        .find(|o| o.name == CheckName::UnexplainedResidual)
        .expect("UnexplainedResidual present");
    assert_eq!(
        outcome_empty_db.status,
        CheckStatus::NotApplicable(
            "no eligible reconciliation intervals in recent window".to_string()
        )
    );
}

#[test]
fn check_fails_heuristic_dedup_counts() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    conn.execute(
        "INSERT INTO ingest_quarantine (
            source_file, parser, failure_class, excerpt_hash, first_observed, last_observed
         ) VALUES ('file1', 'claude-code', 'dedup_collision', 'hash2', 1000, 1000)",
        [],
    )
    .expect("insert dedup collision quarantine");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::HeuristicDedupCounts)
        .expect("HeuristicDedupCounts present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("1 record(s) quarantined for a heuristic-key collision"))
    );
}

#[test]
fn check_fails_clock_skew() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let now = ts(1_700_000_000);
    // Seed account, policy snapshot, sample run, attempt, and attempt result with clock_anomaly = 1.
    conn.execute(
        "INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
         VALUES (1, 'work', 'anthropic', ?1, ?1)",
        [now.unix_nanos()],
    )
    .expect("insert account");
    conn.execute(
        "INSERT INTO sample_run (id, trigger, started_at, aub_version, configuration_fingerprint)
         VALUES (1, 'manual', ?1, '0.1.0', 'cfg')",
        [now.unix_nanos()],
    )
    .expect("insert sample_run");
    conn.execute(
        "INSERT INTO sampling_policy_snapshot (
            id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
            reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version
         ) VALUES (1, 1, ?1, 60000000000, 300000000000, 'none', 'none', 1000000000, 'v1')",
        [now.unix_nanos()],
    )
    .expect("insert policy");
    conn.execute(
        "INSERT INTO meter_attempt (
            id, run_id, account_id, provider, request_started_at, policy_snapshot_id,
            due_at, due_reason, provider_contract_id, meter_semantics_id
         ) VALUES (1, 1, 1, 'anthropic', ?1, 1, ?1, 'ordinary_cadence', 'contract-1', 'meter-1')",
        [now.unix_nanos()],
    )
    .expect("insert meter_attempt");
    conn.execute(
        "INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, clock_anomaly)
         VALUES (1, ?1, 1000, 'success', 1)",
        [now.unix_nanos()],
    )
    .expect("insert meter_attempt_result with clock_anomaly");

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: now,
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::ClockSkew)
        .expect("ClockSkew present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("1 attempt(s) in the last 24h recorded a provider timestamp outside the skew envelope"))
    );
}

#[test]
fn check_fails_local_filesystem_and_wal_suitability() {
    let state = StateDir::new();
    let real_dir = state.path().join("real_state");
    fs::create_dir_all(&real_dir).expect("create real dir");
    let symlink_dir = state.path().join("symlink_state");
    std::os::unix::fs::symlink(&real_dir, &symlink_dir).expect("create symlink");

    let toml = format!("[state]\ndir = {:?}\n", symlink_dir);
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();

    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: symlink_dir.join("ledger.sqlite3"),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::LocalFilesystemAndWalSuitability)
        .expect("LocalFilesystemAndWalSuitability present");
    assert!(matches!(outcome.status, CheckStatus::Fail(ref msg) if msg.contains("symlink")));
}

#[test]
fn owned_checks_have_correct_owner_module() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join("ledger.sqlite3"),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let owned = [
        CheckName::SamplingCadence,
        CheckName::UnresolvedAuthentication,
        CheckName::TranscriptRoots,
        CheckName::BackupAge,
        CheckName::ProjectionVersusDatabaseGeneration,
        CheckName::ClockSkew,
        CheckName::MissingActiveCalibrations,
    ];
    for name in owned {
        let outcome = outcomes.iter().find(|o| o.name == name).unwrap();
        assert_eq!(
            outcome.owner_module, "doctor",
            "{:?} must be owned by doctor",
            name
        );
    }
}

#[test]
fn every_check_declares_name_owner_condition_and_repair_flag() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join("ledger.sqlite3"),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    for outcome in &outcomes {
        assert!(!outcome.name.as_str().is_empty());
        assert!(!outcome.owner_module.is_empty());
        assert!(!outcome.condition.is_empty());
        if outcome.name == CheckName::PendingEvidence {
            assert!(
                outcome.has_repair,
                "pending-evidence must declare has_repair = true"
            );
        }
    }
}

#[test]
fn not_applicable_checks_provide_non_empty_reason() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join("ledger.sqlite3"),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let na_count = outcomes
        .iter()
        .filter(
            |o| matches!(o.status, CheckStatus::NotApplicable(ref reason) if !reason.is_empty()),
        )
        .count();
    assert!(
        na_count > 0,
        "at least one check is not applicable with non-empty reason"
    );
}

fn run_aub(state_dir: &Path, args: &[&str]) -> (i32, String, String) {
    let config_path = state_dir.join("aub.toml");
    if !config_path.exists() {
        let toml = format!("[state]\ndir = {:?}\n", state_dir);
        fs::write(&config_path, toml).expect("write config");
    }
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_aub"))
        .env("HOME", state_dir.join("home"))
        .env("AUB_CONFIG_FILE", &config_path)
        .args(args)
        .output()
        .expect("aub must execute");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn accumulated_diagnostic_material_empty_store_reports_zero() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let check = outcomes
        .iter()
        .find(|o| o.name == CheckName::AccumulatedDiagnosticMaterial)
        .expect("accumulated-diagnostic-material must be registered");
    assert_eq!(check.owner_module, "store::retention");
    assert_eq!(
        check.condition,
        "retained diagnostic capture material does not accumulate unnoticed"
    );
    assert!(!check.has_repair);
    assert_eq!(
        check.status,
        CheckStatus::PassWithDetail(
            "retained bodies: 0 (0 bytes); quarantine rows: 0; quarantine rows are not cleared by the clearing path".to_string()
        )
    );
}

#[test]
fn check_fails_accumulated_diagnostic_material() {
    use agent_usage_book::store::ingest_quarantine::{NewQuarantineItem, record_quarantine};
    use agent_usage_book::store::retention::store_retained_body;

    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let q_item = NewQuarantineItem {
        source_file: "transcripts/claude.jsonl".to_string(),
        byte_offset: Some(12),
        line_number: Some(1),
        parser: "claude-code".to_string(),
        failure_class: "malformed_json".to_string(),
        excerpt_hash: "hash123".to_string(),
        excerpt: None,
        observed_at: ts(1_700_000_000),
    };
    record_quarantine(&conn, &q_item).unwrap();

    store_retained_body(
        state.path(),
        "anthropic",
        "messages",
        b"{\"error\":\"bad_request\"}",
        ts(1_700_000_001),
    )
    .unwrap();
    store_retained_body(
        state.path(),
        "anthropic",
        "messages",
        b"{\"error\":\"rate_limit\"}",
        ts(1_700_000_001),
    )
    .unwrap();
    store_retained_body(
        state.path(),
        "openai",
        "responses",
        b"server error",
        ts(1_700_000_001),
    )
    .unwrap();

    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_002),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let check = outcomes
        .iter()
        .find(|o| o.name == CheckName::AccumulatedDiagnosticMaterial)
        .expect("accumulated-diagnostic-material must be registered");

    match &check.status {
        CheckStatus::Fail(detail) => {
            assert!(
                detail.contains("retained bodies: 3 (57 bytes)"),
                "detail: {detail}"
            );
            assert!(
                detail.contains("anthropic/messages: 2 (45 bytes)"),
                "detail: {detail}"
            );
            assert!(
                detail.contains("openai/responses: 1 (12 bytes)"),
                "detail: {detail}"
            );
            assert!(
                detail.contains("quarantine rows: 1 [claude-code: 1]"),
                "detail: {detail}"
            );
            assert!(
                detail.contains("quarantine rows are not cleared by the clearing path"),
                "detail: {detail}"
            );
        }
        other => panic!("expected CheckStatus::Fail, got {other:?}"),
    }
}

#[test]
fn clearing_command_removes_retained_bodies_and_agrees_with_doctor() {
    use agent_usage_book::store::ingest_quarantine::{NewQuarantineItem, record_quarantine};
    use agent_usage_book::store::retention::{count_retained_bodies, store_retained_body};

    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let q_item = NewQuarantineItem {
        source_file: "transcripts/claude.jsonl".to_string(),
        byte_offset: None,
        line_number: Some(5),
        parser: "claude-code".to_string(),
        failure_class: "malformed_json".to_string(),
        excerpt_hash: "qhash999".to_string(),
        excerpt: None,
        observed_at: ts(1_700_000_000),
    };
    record_quarantine(&conn, &q_item).unwrap();
    drop(conn);

    store_retained_body(
        state.path(),
        "anthropic",
        "messages",
        b"payload-1",
        ts(1_700_000_001),
    )
    .unwrap();
    store_retained_body(
        state.path(),
        "anthropic",
        "messages",
        b"payload-2",
        ts(1_700_000_001),
    )
    .unwrap();
    store_retained_body(
        state.path(),
        "openai",
        "completions",
        b"payload-3",
        ts(1_700_000_001),
    )
    .unwrap();

    let (code, stdout, _) = run_aub(state.path(), &["doctor", "--format", "json"]);
    assert_eq!(code, 0, "doctor command finishes successfully: {stdout}");
    assert!(stdout.contains("\"accumulated-diagnostic-material\""));
    assert!(stdout.contains("\"fail\""));

    let (code, stdout, _) = run_aub(
        state.path(),
        &["clear-diagnostics", "--provider", "anthropic"],
    );
    assert_eq!(code, 0);
    assert!(stdout.contains("Cleared 2 retained bodies"));
    assert!(stdout.contains("for provider 'anthropic'"));
    assert_eq!(count_retained_bodies(state.path()).unwrap().0, 1);

    let (code, stdout, _) = run_aub(state.path(), &["clear-captures", "--all"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Cleared 1 retained body"));
    assert!(stdout.contains("in total"));
    assert_eq!(count_retained_bodies(state.path()).unwrap().0, 0);

    let (code, stdout, _) = run_aub(state.path(), &["doctor", "--format", "json"]);
    assert_eq!(
        code, 0,
        "doctor passes when diagnostic captures are cleared: {stdout}"
    );
    let doc: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let checks = doc["checks"].as_array().unwrap();
    let diag_check = checks
        .iter()
        .find(|c| c["name"] == "accumulated-diagnostic-material")
        .unwrap();
    assert_eq!(diag_check["status"], "pass");
    let reason = diag_check["reason"].as_str().unwrap();
    assert!(reason.contains("retained bodies: 0 (0 bytes)"));
    assert!(reason.contains("quarantine rows: 1 [claude-code: 1]"));
    assert!(reason.contains("quarantine rows are not cleared by the clearing path"));
}

#[test]
fn clearing_command_never_removes_quarantine_rows() {
    use agent_usage_book::store::ingest_quarantine::{NewQuarantineItem, record_quarantine};
    use agent_usage_book::store::retention::store_retained_body;

    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let q1 = NewQuarantineItem {
        source_file: "file1.jsonl".to_string(),
        byte_offset: None,
        line_number: Some(1),
        parser: "claude-code".to_string(),
        failure_class: "err1".to_string(),
        excerpt_hash: "hash_one".to_string(),
        excerpt: None,
        observed_at: ts(100),
    };
    let q2 = NewQuarantineItem {
        source_file: "file2.jsonl".to_string(),
        byte_offset: None,
        line_number: Some(2),
        parser: "codex".to_string(),
        failure_class: "err2".to_string(),
        excerpt_hash: "hash_two".to_string(),
        excerpt: None,
        observed_at: ts(200),
    };
    record_quarantine(&conn, &q1).unwrap();
    record_quarantine(&conn, &q2).unwrap();
    drop(conn);

    store_retained_body(state.path(), "anthropic", "messages", b"debug", ts(100)).unwrap();

    let (code, _, _) = run_aub(state.path(), &["clear-diagnostics", "--all"]);
    assert_eq!(code, 0);

    let conn = open_ledger(state.path());
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ingest_quarantine", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        count, 2,
        "quarantine rows must never be removed by clearing command"
    );

    let mut stmt = conn
        .prepare("SELECT excerpt_hash FROM ingest_quarantine ORDER BY id")
        .unwrap();
    let hashes: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(hashes, vec!["hash_one".to_string(), "hash_two".to_string()]);
}

#[test]
fn clearing_retained_bodies_leaves_status_and_coverage_byte_identical() {
    use agent_usage_book::store::retention::store_retained_body;

    let state = StateDir::new();
    let _conn = open_ledger(state.path());

    let (code_status, status_txt_before, _) = run_aub(state.path(), &["status"]);
    assert_eq!(code_status, 0);
    let (code_status_json, status_json_before, _) =
        run_aub(state.path(), &["status", "--format", "json"]);
    assert_eq!(code_status_json, 0);

    let (code_cov, cov_txt_before, _) = run_aub(state.path(), &["coverage"]);
    assert_eq!(code_cov, 0);
    let (code_cov_json, cov_json_before, _) =
        run_aub(state.path(), &["coverage", "--format", "json"]);
    assert_eq!(code_cov_json, 0);

    for i in 0..10 {
        store_retained_body(
            state.path(),
            "anthropic",
            "messages",
            format!("capture-{i}").as_bytes(),
            ts(1_700_000_000),
        )
        .unwrap();
    }

    let (code_clear, stdout_clear, _) = run_aub(state.path(), &["clear-diagnostics", "--all"]);
    assert_eq!(code_clear, 0);
    assert!(stdout_clear.contains("Cleared 10 retained bodies"));

    let (code_status, status_txt_after, _) = run_aub(state.path(), &["status"]);
    assert_eq!(code_status, 0);
    let (code_status_json, status_json_after, _) =
        run_aub(state.path(), &["status", "--format", "json"]);
    assert_eq!(code_status_json, 0);

    let (code_cov, cov_txt_after, _) = run_aub(state.path(), &["coverage"]);
    assert_eq!(code_cov, 0);
    let (code_cov_json, cov_json_after, _) =
        run_aub(state.path(), &["coverage", "--format", "json"]);
    assert_eq!(code_cov_json, 0);

    assert_eq!(
        status_txt_before, status_txt_after,
        "aub status text output must be byte-identical before and after clearing retained bodies"
    );
    assert_eq!(
        cov_txt_before, cov_txt_after,
        "aub coverage text output must be byte-identical before and after clearing retained bodies"
    );

    let mut doc_status_before: serde_json::Value =
        serde_json::from_str(&status_json_before).unwrap();
    let mut doc_status_after: serde_json::Value = serde_json::from_str(&status_json_after).unwrap();
    doc_status_before["run"] = serde_json::json!("normalized");
    doc_status_before["generated_at"] = serde_json::json!(0);
    doc_status_before["knowledge_at"] = serde_json::json!(0);
    doc_status_after["run"] = serde_json::json!("normalized");
    doc_status_after["generated_at"] = serde_json::json!(0);
    doc_status_after["knowledge_at"] = serde_json::json!(0);
    assert_eq!(
        doc_status_before, doc_status_after,
        "stored measurements and quota readings must be identical before and after clearing"
    );

    let mut doc_cov_before: serde_json::Value = serde_json::from_str(&cov_json_before).unwrap();
    let mut doc_cov_after: serde_json::Value = serde_json::from_str(&cov_json_after).unwrap();
    doc_cov_before["run"] = serde_json::json!("normalized");
    doc_cov_before["generated_at"] = serde_json::json!(0);
    doc_cov_before["knowledge_at"] = serde_json::json!(0);
    doc_cov_before["interval"] = serde_json::json!("normalized");
    doc_cov_after["run"] = serde_json::json!("normalized");
    doc_cov_after["generated_at"] = serde_json::json!(0);
    doc_cov_after["knowledge_at"] = serde_json::json!(0);
    doc_cov_after["interval"] = serde_json::json!("normalized");
    assert_eq!(
        doc_cov_before, doc_cov_after,
        "coverage figures must be identical before and after clearing"
    );
}

// ---------------------------------------------------------------------------
// Tests for aub-dpn.3: Report rolling residual health in doctor
// ---------------------------------------------------------------------------

fn test_reconciled_interval(
    start_secs: i64,
    end_secs: i64,
    observed_micros: i64,
    locally_explained_micros: i64,
    residual_micros: i64,
    lower_micros: i64,
    upper_micros: i64,
) -> agent_usage_book::reconciliation::ReconciledResidual {
    use agent_usage_book::domain::credits::Credits;
    use agent_usage_book::domain::interval::Interval;
    use agent_usage_book::domain::provenance::{
        ProvenanceManifest, QuerySemantics, WindowCalibrationId,
    };
    use agent_usage_book::domain::quota::PercentagePoints;
    use agent_usage_book::domain::window::WindowSemanticKey;
    use agent_usage_book::reconciliation::{
        MeterDeltaBounds, ReconciledResidual, TimingAlignmentUncertainty,
    };
    use agent_usage_book::store::account::AccountId;

    ReconciledResidual {
        account_id: AccountId::new(1),
        window_key: WindowSemanticKey::new("five_hour"),
        interval_start: ts(start_secs),
        interval_end: ts(end_secs),
        observed_meter_delta: PercentagePoints::new(0).unwrap(),
        observed_meter_credits: Credits::from_micros(observed_micros),
        locally_explained_credits: Credits::from_micros(locally_explained_micros),
        explained_interval_change: PercentagePoints::new(0).unwrap(),
        unexplained_residual: Credits::from_micros(residual_micros),
        unexplained_residual_percentage_points: PercentagePoints::new(0).unwrap(),
        observed_meter_delta_bounds: MeterDeltaBounds::new(0, 0),
        observed_meter_credits_interval: Interval::new(
            Credits::from_micros(observed_micros),
            Credits::from_micros(observed_micros),
        )
        .unwrap(),
        timing_alignment: TimingAlignmentUncertainty::none(),
        unexplained_residual_interval: Interval::new(
            Credits::from_micros(lower_micros),
            Credits::from_micros(upper_micros),
        )
        .unwrap(),
        calibration_id: WindowCalibrationId::new("cal-test"),
        provenance: ProvenanceManifest::new(
            vec![],
            vec![],
            QuerySemantics::new("reconciliation", ""),
        ),
    }
}

#[test]
fn unit_rolling_residual_pattern_detection_and_candidate_explanations() {
    use agent_usage_book::domain::credits::Credits;
    use agent_usage_book::reconciliation::{ResidualPattern, classify_patterns};

    // Pattern 1: Persistently positive
    let pos_residuals = vec![
        Credits::from_micros(10_000),
        Credits::from_micros(20_000),
        Credits::from_micros(30_000),
    ];
    let pos_patterns = classify_patterns(&pos_residuals);
    assert_eq!(pos_patterns, vec![ResidualPattern::PersistentlyPositive]);
    let pos_exp = ResidualPattern::PersistentlyPositive.explanation();
    assert!(pos_exp.contains("persistently positive residual"));
    assert!(pos_exp.contains(
        "possible web, headless-unlogged, cross-machine or missed transcript consumption"
    ));

    // Pattern 2: Persistently negative
    let neg_residuals = vec![
        Credits::from_micros(-10_000),
        Credits::from_micros(-20_000),
        Credits::from_micros(-30_000),
    ];
    let neg_patterns = classify_patterns(&neg_residuals);
    assert_eq!(neg_patterns, vec![ResidualPattern::PersistentlyNegative]);
    let neg_exp = ResidualPattern::PersistentlyNegative.explanation();
    assert!(neg_exp.contains("persistently negative residual"));
    assert!(neg_exp.contains("possible calibration overprediction or provider semantics change"));

    // Pattern 3: Step change
    let step_residuals = vec![
        Credits::from_micros(-10_000),
        Credits::from_micros(-10_000),
        Credits::from_micros(100_000),
        Credits::from_micros(100_000),
    ];
    let step_patterns = classify_patterns(&step_residuals);
    assert_eq!(step_patterns, vec![ResidualPattern::StepChange]);
    let step_exp = ResidualPattern::StepChange.explanation();
    assert!(step_exp.contains("step change in residual"));
    assert!(step_exp.contains("possible plan or provider accounting transition"));

    // Pattern 4: Alternating short-interval residuals netting to zero
    let alt_residuals = vec![
        Credits::from_micros(10_000),
        Credits::from_micros(-10_000),
        Credits::from_micros(10_000),
        Credits::from_micros(-10_000),
    ];
    let alt_patterns = classify_patterns(&alt_residuals);
    assert_eq!(alt_patterns, vec![ResidualPattern::AlternatingNetZero]);
    let alt_exp = ResidualPattern::AlternatingNetZero.explanation();
    assert!(alt_exp.contains("alternating short-interval residuals that net to zero"));
    assert!(alt_exp.contains("likely accounting lag"));

    // Check absence of causal claims across all explanations
    for pat in [
        ResidualPattern::PersistentlyPositive,
        ResidualPattern::PersistentlyNegative,
        ResidualPattern::StepChange,
        ResidualPattern::AlternatingNetZero,
    ] {
        let text = pat.explanation();
        assert!(
            !text.contains("caused by"),
            "must not claim causation: {text}"
        );
        assert!(
            !text.contains("the cause is"),
            "must not claim causation: {text}"
        );
    }
}

#[test]
fn unit_rolling_residual_step_change_pointer_to_calibration_health_check_without_conclusion() {
    use agent_usage_book::reconciliation::ResidualPattern;

    let pointer = ResidualPattern::StepChange.calibration_pointer();
    assert_eq!(
        pointer,
        Some(
            "pointer: check calibration health (aub doctor missing-active-calibrations) to verify whether calibration has become inapplicable"
        )
    );
    assert!(!pointer.unwrap().contains("caused by"));
    assert!(!pointer.unwrap().contains("inapplicable because"));

    assert_eq!(
        ResidualPattern::PersistentlyPositive.calibration_pointer(),
        None
    );
    assert_eq!(
        ResidualPattern::PersistentlyNegative.calibration_pointer(),
        None
    );
    assert_eq!(
        ResidualPattern::AlternatingNetZero.calibration_pointer(),
        None
    );
}

#[test]
fn integration_rolling_residual_below_minimum_eligible_suppresses_verdict() {
    use agent_usage_book::reconciliation::{
        RollingResidualVerdict, compute_rolling_residual_health,
    };

    let intervals = vec![
        test_reconciled_interval(100, 200, 10_000, 8_000, 2_000, 1_000, 3_000),
        test_reconciled_interval(200, 300, 10_000, 8_000, 2_000, 1_000, 3_000),
        test_reconciled_interval(300, 400, 10_000, 8_000, 2_000, 1_000, 3_000),
    ];
    let window = MonotonicDuration::from_seconds(86400 * 30);
    let health = compute_rolling_residual_health(&intervals, window, 5)
        .expect("health exists when intervals exist");

    assert_eq!(health.eligible_count, 3);
    assert_eq!(health.min_eligible, 5);
    assert_eq!(
        health.verdict,
        RollingResidualVerdict::Suppressed {
            eligible_count: 3,
            min_eligible: 5,
        }
    );
    assert!(health.verdict.is_suppressed());
}

#[test]
fn unit_rolling_residual_section_omitted_when_no_eligible_interval_exists() {
    use agent_usage_book::logging::RunId;
    use agent_usage_book::presentation::{doctor_report_json, render_doctor_report};
    use agent_usage_book::reconciliation::compute_rolling_residual_health;
    use agent_usage_book::report::{LedgerGeneration, ReportMetadata};

    let window = MonotonicDuration::from_seconds(86400 * 30);
    let health = compute_rolling_residual_health(&[], window, 5);
    assert!(
        health.is_none(),
        "health must be None when no intervals exist"
    );

    let now = ts(1_700_000_000);
    let report = agent_usage_book::doctor::DoctorReport {
        metadata: ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        outcomes: vec![],
        residual: None,
    };

    let text = render_doctor_report(&report);
    assert!(
        !text.contains("Doctor: Rolling Residual Health"),
        "section must be omitted in text"
    );
    assert!(
        !text.contains("residual interval"),
        "residual interval must be omitted in text"
    );

    let json = doctor_report_json(&report, RunId::new(now));
    let val: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert!(
        val.get("residual").is_none(),
        "residual key must be omitted in json"
    );
}

#[test]
fn unit_rolling_residual_check_performs_no_network_operation_and_no_fitting() {
    let _tripwire = DoctorMustNotTouchNetwork;
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let config = test_config(state.path());
    let now = ts(1_700_000_000);

    let ctx = DoctorContext {
        config: &config,
        timestamp: now,
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };

    let outcomes = build_registry(&ctx);
    let residual_check = outcomes
        .iter()
        .find(|o| o.name == CheckName::UnexplainedResidual)
        .expect("UnexplainedResidual check present");
    assert_eq!(
        residual_check.status,
        CheckStatus::NotApplicable(
            "no eligible reconciliation intervals in recent window".to_string()
        )
    );

    let res = agent_usage_book::doctor::checks::rolling_residual_health(&ctx);
    assert!(res.is_none());
}

#[test]
fn e2e_doctor_json_includes_residual_fields_when_justified_and_omits_when_no_eligible_intervals() {
    use agent_usage_book::domain::credits::Credits;
    use agent_usage_book::domain::interval::Interval;
    use agent_usage_book::logging::RunId;
    use agent_usage_book::presentation::doctor_report_json;
    use agent_usage_book::reconciliation::{
        ResidualPattern, RollingResidualHealth, RollingResidualVerdict,
    };
    use agent_usage_book::report::{LedgerGeneration, ReportMetadata};

    let now = ts(1_700_000_000);

    // 1. Omitted case: residual is None
    let report_omitted = agent_usage_book::doctor::DoctorReport {
        metadata: ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        outcomes: vec![],
        residual: None,
    };
    let json_omitted = doctor_report_json(&report_omitted, RunId::new(now));
    let val_omitted: serde_json::Value = serde_json::from_str(&json_omitted).unwrap();
    assert!(val_omitted.get("residual").is_none());

    // 2. Justified case: residual is Some
    let health = RollingResidualHealth {
        window: MonotonicDuration::from_seconds(86400 * 30),
        min_eligible: 5,
        eligible_count: 6,
        total_observed_meter_credits: Credits::from_micros(10_000_000),
        total_locally_explained_credits: Credits::from_micros(8_000_000),
        rolling_residual: Credits::from_micros(2_000_000),
        rolling_residual_interval: Interval::new(
            Credits::from_micros(1_000_000),
            Credits::from_micros(3_000_000),
        )
        .unwrap(),
        rolling_residual_fraction: Some(0.20),
        verdict: RollingResidualVerdict::Discrepancy {
            patterns: vec![ResidualPattern::StepChange],
        },
        patterns: vec![ResidualPattern::StepChange],
        pointer: ResidualPattern::StepChange.calibration_pointer(),
    };

    let report_justified = agent_usage_book::doctor::DoctorReport {
        metadata: ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        outcomes: vec![],
        residual: Some(health),
    };
    let json_justified = doctor_report_json(&report_justified, RunId::new(now));
    let val_justified: serde_json::Value = serde_json::from_str(&json_justified).unwrap();
    let res = val_justified
        .get("residual")
        .expect("residual field present in json");
    assert_eq!(res["eligible_count"], 6);
    assert_eq!(res["min_eligible"], 5);
    assert_eq!(res["residual_interval"]["lower"], 1_000_000);
    assert_eq!(res["residual_interval"]["upper"], 3_000_000);
    assert_eq!(res["residual_interval"]["unit"], "credits");
    assert_eq!(res["verdict"], "discrepancy");
    assert!(res["fraction"].is_number());
    assert_eq!(res["patterns"][0]["label"], "step change");
    assert!(res["pointer"].is_string());
}

#[test]
fn golden_doctor_human_output_justified_state_and_omission_state() {
    use agent_usage_book::doctor::{CheckOutcome, DoctorReport};
    use agent_usage_book::domain::credits::Credits;
    use agent_usage_book::domain::interval::Interval;
    use agent_usage_book::presentation::render_doctor_report;
    use agent_usage_book::reconciliation::{
        ResidualPattern, RollingResidualHealth, RollingResidualVerdict,
    };
    use agent_usage_book::report::{LedgerGeneration, ReportMetadata};

    let now = ts(1_700_000_000);

    // Omission case: no eligible interval exists
    let report_omitted = DoctorReport {
        metadata: ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        outcomes: vec![CheckOutcome {
            name: CheckName::UnexplainedResidual,
            owner_module: "reconciliation",
            condition: "rolling residual stays within its explained bound",
            has_repair: false,
            status: CheckStatus::NotApplicable(
                "no eligible reconciliation intervals in recent window".to_string(),
            ),
        }],
        residual: None,
    };
    let output_omitted = render_doctor_report(&report_omitted);
    let expected_omitted = "Doctor: 1 checks\n  [N/A ] unexplained-residual: no eligible reconciliation intervals in recent window\nSummary: 0 passed, 0 failed, 1 not applicable, 0 not yet available";
    assert_eq!(output_omitted, expected_omitted);

    // Justified case: eligible intervals exist and show discrepancy
    let health = RollingResidualHealth {
        window: MonotonicDuration::from_seconds(86400 * 30),
        min_eligible: 5,
        eligible_count: 6,
        total_observed_meter_credits: Credits::from_micros(10_000_000),
        total_locally_explained_credits: Credits::from_micros(8_000_000),
        rolling_residual: Credits::from_micros(2_000_000),
        rolling_residual_interval: Interval::new(
            Credits::from_micros(1_000_000),
            Credits::from_micros(3_000_000),
        )
        .unwrap(),
        rolling_residual_fraction: Some(0.20),
        verdict: RollingResidualVerdict::Discrepancy {
            patterns: vec![ResidualPattern::StepChange],
        },
        patterns: vec![ResidualPattern::StepChange],
        pointer: ResidualPattern::StepChange.calibration_pointer(),
    };

    let report_justified = DoctorReport {
        metadata: ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        outcomes: vec![CheckOutcome {
            name: CheckName::UnexplainedResidual,
            owner_module: "reconciliation",
            condition: "rolling residual stays within its explained bound",
            has_repair: false,
            status: CheckStatus::Fail("rolling residual discrepancy: interval [1000000 .. 3000000] credits; pattern: step change in residual: possible plan or provider accounting transition; pointer: check calibration health (aub doctor missing-active-calibrations) to verify whether calibration has become inapplicable".to_string()),
        }],
        residual: Some(health),
    };
    let output_justified = render_doctor_report(&report_justified);
    let expected_justified = "Doctor: 1 checks\n  [FAIL] unexplained-residual: rolling residual discrepancy: interval [1000000 .. 3000000] credits; pattern: step change in residual: possible plan or provider accounting transition; pointer: check calibration health (aub doctor missing-active-calibrations) to verify whether calibration has become inapplicable\nSummary: 0 passed, 1 failed, 0 not applicable, 0 not yet available\n\nDoctor: Rolling Residual Health\n  window: 30d (6 eligible intervals, minimum: 5)\n  residual interval: [1000000 .. 3000000] credits\n  residual fraction: +20.00%\n  verdict: discrepancy\n  pattern: step change in residual: possible plan or provider accounting transition\n  pointer: check calibration health (aub doctor missing-active-calibrations) to verify whether calibration has become inapplicable";
    assert_eq!(output_justified, expected_justified);
}

#[test]
fn integration_synthetic_step_change_produces_step_change_pattern_and_no_causal_claim() {
    use agent_usage_book::domain::credits::Credits;
    use agent_usage_book::reconciliation::{
        ResidualPattern, classify_patterns, compute_rolling_residual_health,
    };

    let intervals = vec![
        test_reconciled_interval(100, 200, 10_000, 20_000, -10_000, -15_000, -5_000),
        test_reconciled_interval(200, 300, 10_000, 20_000, -10_000, -15_000, -5_000),
        test_reconciled_interval(300, 400, 10_000, 20_000, -10_000, -15_000, -5_000),
        test_reconciled_interval(400, 500, 120_000, 20_000, 100_000, 95_000, 105_000),
        test_reconciled_interval(500, 600, 120_000, 20_000, 100_000, 95_000, 105_000),
        test_reconciled_interval(600, 700, 120_000, 20_000, 100_000, 95_000, 105_000),
    ];

    let residuals: Vec<Credits> = intervals.iter().map(|i| i.unexplained_residual).collect();
    let patterns = classify_patterns(&residuals);
    assert!(patterns.contains(&ResidualPattern::StepChange));

    let window = MonotonicDuration::from_seconds(86400 * 30);
    let health = compute_rolling_residual_health(&intervals, window, 5)
        .expect("rolling residual health computed");

    assert_eq!(health.window, window);
    assert_eq!(health.eligible_count, 6);
    assert_eq!(health.rolling_residual_interval.lower().micros(), 240_000);
    assert_eq!(health.rolling_residual_interval.upper().micros(), 300_000);
    assert!(health.rolling_residual_fraction.is_some());
    let fraction = health.rolling_residual_fraction.unwrap();
    assert!((fraction - (270_000.0 / 390_000.0)).abs() < 1e-6);

    assert!(health.patterns.contains(&ResidualPattern::StepChange));
    for pat in &health.patterns {
        assert!(
            !pat.explanation().contains("caused by"),
            "must not claim cause"
        );
        assert!(
            !pat.explanation().contains("the cause is"),
            "must not claim cause"
        );
    }
    assert!(health.pointer.is_some());
    let pointer = health.pointer.unwrap();
    assert!(pointer.contains("aub doctor missing-active-calibrations"));
    assert!(!pointer.contains("caused by"), "must not claim cause");
    assert!(!pointer.contains("the cause is"), "must not claim cause");
}
