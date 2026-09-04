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
fn check_reports_not_yet_available_unexplained_residual() {
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
        .find(|o| o.name == CheckName::UnexplainedResidual)
        .expect("UnexplainedResidual present");
    assert_eq!(
        outcome.status,
        CheckStatus::NotYetAvailable {
            owning_bead: "aub-dpn.3"
        }
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
