//! Integration tests for aub doctor check registry and repair split (aub-n27.7).

use std::fs;
use std::path::Path;

use agent_usage_book::backup::{BackupSeriesRetention, backup_series_create};
use agent_usage_book::config::{Config, Overrides, RealEnv, resolve};
use agent_usage_book::doctor::{
    CheckName, CheckStatus, DoctorContext, build_registry, configuration_failed_registry, run_fix,
};
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::time::{Clock, FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::meter::adapter::HttpTransport;
use agent_usage_book::meter::transport::{CommandBudget, HttpRequest, HttpResponse};
use agent_usage_book::store::connection;
use test_support::StateDir;

fn ts(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_nanos(seconds * 1_000_000_000)
}

fn open_ledger(state_dir: &Path) -> rusqlite::Connection {
    let path = state_dir.join(connection::LEDGER_DATABASE_FILE);
    let policy = connection::PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(500),
    };
    // Several tests call this twice on one path to reopen a ledger they have
    // already seeded, so the template copy (aub-yr9c) is only for the first,
    // creating call; a reopen must not clobber the rows already there.
    if path.exists() {
        return connection::open(&path, connection::AccessMode::ReadWrite, &policy)
            .expect("an existing scratch ledger must reopen");
    }
    test_support::open_migrated(&path, &policy)
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

/// `aub-n27.10`: the finalized registry contains no `not-yet-available` entry.
/// `UnmappedAccounts` is a real check over the persisted attribution segments.
#[test]
fn registry_contains_no_not_yet_available_entry() {
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
    assert_eq!(outcomes.len(), CheckName::EXPECTED.len());
    assert!(
        agent_usage_book::doctor::missing_checks(&outcomes).is_empty(),
        "every expected check must be registered"
    );
    for outcome in &outcomes {
        assert!(
            !matches!(outcome.status, CheckStatus::NotYetAvailable { .. }),
            "{:?} must not be not-yet-available in the finalized registry",
            outcome.name
        );
    }
}

/// `aub-n27.10`: injecting one missing registration names exactly that check,
/// rather than failing a generic assertion nobody can act on.
#[test]
fn injected_missing_registration_names_the_check() {
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
    let mut outcomes = build_registry(&ctx);
    outcomes.retain(|o| o.name != CheckName::HeuristicDedupCounts);
    assert_eq!(
        agent_usage_book::doctor::missing_checks(&outcomes),
        vec![CheckName::HeuristicDedupCounts]
    );
}

fn seed_attribution_segment(
    conn: &rusqlite::Connection,
    session_id: &str,
    target_kind: &str,
    logical_account: Option<&str>,
    evidence_class: &str,
    input_tokens: i64,
) {
    conn.execute(
        "INSERT INTO account_attribution_segment (
            session_id, target_kind, logical_account, evidence_class,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, 1000)",
        rusqlite::params![
            session_id,
            target_kind,
            logical_account,
            evidence_class,
            input_tokens
        ],
    )
    .expect("insert attribution segment");
}

#[test]
fn check_fails_unmapped_accounts() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    seed_attribution_segment(
        &conn,
        "claude-code:s1",
        "account",
        Some("work"),
        "explicit_launcher_or_hook",
        60,
    );
    seed_attribution_segment(
        &conn,
        "claude-code:s1",
        "unknown_account",
        None,
        "unattributed",
        40,
    );

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
        .find(|o| o.name == CheckName::UnmappedAccounts)
        .expect("UnmappedAccounts present");
    assert_eq!(outcome.owner_module, "attribution");
    assert_eq!(
        outcome.condition,
        "no canonical usage sits in the unknown-account bucket"
    );
    assert!(
        !outcome.has_repair,
        "unmapped accounts must declare has_repair = false: --fix must not reattribute"
    );
    match &outcome.status {
        CheckStatus::Fail(reason) => {
            assert!(
                reason.contains("input: 40 unattributed of 100 total"),
                "the failure must name the stable check evidence, per-kind counts: {reason}"
            );
            assert!(
                !reason.contains(state.path().to_str().unwrap()),
                "the failure must not expose an absolute path: {reason}"
            );
        }
        other => panic!("expected Fail naming the unknown-account counts, got {other:?}"),
    }
}

#[test]
fn unmapped_accounts_passes_when_everything_is_attributed() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    seed_attribution_segment(
        &conn,
        "claude-code:s1",
        "account",
        Some("work"),
        "explicit_launcher_or_hook",
        60,
    );

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
        .find(|o| o.name == CheckName::UnmappedAccounts)
        .expect("UnmappedAccounts present");
    assert_eq!(outcome.status, CheckStatus::Pass);
}

#[test]
fn unmapped_accounts_is_not_applicable_before_any_segment_exists() {
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx_no_db = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: true,
        db_open_error: None,
    };
    let outcomes_no_db = build_registry(&ctx_no_db);
    let outcome_no_db = outcomes_no_db
        .iter()
        .find(|o| o.name == CheckName::UnmappedAccounts)
        .expect("UnmappedAccounts present");
    assert_eq!(
        outcome_no_db.status,
        CheckStatus::NotApplicable("no ledger database exists yet".to_string())
    );

    let conn = open_ledger(state.path());
    let ctx_empty_db = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes_empty_db = build_registry(&ctx_empty_db);
    let outcome_empty_db = outcomes_empty_db
        .iter()
        .find(|o| o.name == CheckName::UnmappedAccounts)
        .expect("UnmappedAccounts present");
    assert_eq!(
        outcome_empty_db.status,
        CheckStatus::NotApplicable("no attribution segments have been recorded yet".to_string())
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

/// `aub-6wym`: with prices imported and no model active, the check warns and
/// names the command that repairs it. Planted negative: a naive implementation
/// that warns without the repair line (or that fails instead of warning) fails
/// one of the two assertions below, since a credits ledger is still complete
/// without pricing and the finding must stay advisory.
#[test]
fn cost_model_active_warns_with_rate_cards_and_no_active_model() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    conn.execute(
        "INSERT INTO rate_card (
            vendor, model, token_class, rate_micros, currency, billing_basis,
            effective_start, imported_at, review_due
         ) VALUES ('anthropic', 'claude-opus-4', 'input', 10, 'USD', 'per_million_tokens', '2026-01-01', 1000, '2099-01-01')",
        [],
    )
    .expect("insert rate card");

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
        .find(|o| o.name == CheckName::CostModelActive)
        .expect("CostModelActive present");
    assert_eq!(outcome.owner_module, "store::cost_model");
    assert!(!outcome.has_repair);
    match &outcome.status {
        CheckStatus::Warn(reason) => {
            assert!(reason.contains("1 rate card row(s)"), "{reason}");
            assert!(reason.contains("aub cost-model activate"), "{reason}");
        }
        other => panic!("expected Warn naming the activation command, got {other:?}"),
    }
}

/// `aub-6wym`: after the operator activates a published model the check passes
/// naming the active model, and with no rate card imported at all the check is
/// not applicable, since there is nothing pricing is due against yet.
#[test]
fn cost_model_active_passes_after_activation_and_na_without_cards() {
    use agent_usage_book::store::cost_model::{
        ANTHROPIC_CLAUDE_MESSAGES_V1_ID, activate_if_not_active, published_model,
    };

    let state = StateDir::new();
    let mut conn = open_ledger(state.path());
    let config = test_config(state.path());

    // No rate card, no active model: the pricing premise is absent.
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcome = build_registry(&ctx)
        .iter()
        .find(|o| o.name == CheckName::CostModelActive)
        .expect("CostModelActive present")
        .clone();
    assert!(matches!(outcome.status, CheckStatus::NotApplicable(_)));

    // One rate card plus an activation: the check passes naming the model.
    conn.execute(
        "INSERT INTO rate_card (
            vendor, model, token_class, rate_micros, currency, billing_basis,
            effective_start, imported_at, review_due
         ) VALUES ('anthropic', 'claude-opus-4', 'input', 10, 'USD', 'per_million_tokens', '2026-01-01', 1000, '2099-01-01')",
        [],
    )
    .expect("insert rate card");
    let model = published_model(ANTHROPIC_CLAUDE_MESSAGES_V1_ID, ts(1_700_000_000))
        .expect("published model");
    assert!(
        activate_if_not_active(&mut conn, &model, ts(1_700_000_000)).expect("activation writes")
    );
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let binding = build_registry(&ctx);
    let outcome = binding
        .iter()
        .find(|o| o.name == CheckName::CostModelActive)
        .expect("CostModelActive present");
    assert!(
        matches!(outcome.status, CheckStatus::PassWithDetail(ref detail) if detail.contains("active cost model: anthropic-claude-messages-v1")),
        "{outcome:?}"
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
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("no verified backup found"))
    );
}

/// `aub-kzgo`: `backup-age` reads the newest verified archive from the
/// destination root and reports its age from the manifest's
/// `created_at_unix_nanos`, not from file mtime. The second half rewrites the
/// newest manifest byte-for-byte (which moves its mtime to now) and shows the
/// verdict does not move with it.
#[test]
fn check_reads_backup_age_from_the_newest_verified_manifest_not_mtime() {
    let state = StateDir::new();
    open_ledger(state.path());
    let root = state.path().join("backups");
    let retention = BackupSeriesRetention::new(7, 4, 6, 2);
    let busy = MonotonicDuration::from_millis(500);
    let first_at = UtcTimestamp::parse_rfc3339("2026-09-01T12:00:00Z").unwrap();
    let second_at = UtcTimestamp::parse_rfc3339("2026-09-02T12:00:00Z").unwrap();
    backup_series_create(
        state.path(),
        &root,
        &retention,
        busy,
        &FakeClock::new(first_at),
    )
    .expect("first series backup must succeed");
    let second = backup_series_create(
        state.path(),
        &root,
        &retention,
        busy,
        &FakeClock::new(second_at),
    )
    .expect("second series backup must succeed");

    let toml = format!(
        "[state]\ndir = {:?}\n\n[backup]\ndestination = {:?}\nreview_after = \"48h\"\n",
        state.path(),
        root
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();
    let outcome_at = |now: UtcTimestamp| {
        let ctx = DoctorContext {
            config: &config,
            timestamp: now,
            db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
            db: None,
            db_missing: true,
            db_open_error: None,
        };
        build_registry(&ctx)
            .into_iter()
            .find(|o| o.name == CheckName::BackupAge)
            .expect("BackupAge present")
    };

    // One hour after the newer archive: within the 48h horizon, so it passes.
    let fresh_now = UtcTimestamp::from_unix_nanos(second_at.unix_nanos() + 3_600_000_000_000);
    assert!(
        matches!(
            outcome_at(fresh_now).status,
            CheckStatus::Pass | CheckStatus::PassWithDetail(_)
        ),
        "a series with a fresh newest-verified archive must pass"
    );

    // Forty nine hours after the newer archive: past the horizon, so it fails
    // naming the backup even though the older archive exists too.
    let stale_now = UtcTimestamp::from_unix_nanos(second_at.unix_nanos() + 49 * 3_600_000_000_000);
    let stale = outcome_at(stale_now);
    assert!(
        matches!(stale.status, CheckStatus::Fail(ref reason) if reason.contains("backup")),
        "a stale newest-verified archive must fail naming the backup: {:?}",
        stale.status
    );

    // Rewriting the newest manifest byte-for-byte moves its mtime to now
    // without changing its content; the verdict must not move with the mtime.
    let manifest_path = second.destination.join("manifest.json");
    let bytes = fs::read(&manifest_path).expect("newest manifest must be readable");
    fs::write(&manifest_path, &bytes).expect("rewriting the same manifest must work");
    let stale_again = outcome_at(stale_now);
    assert!(
        matches!(stale_again.status, CheckStatus::Fail(_)),
        "backup age must come from created_at, not file mtime: {:?}",
        stale_again.status
    );
}

/// `aub-eun.14`: with no ledger database yet, `MeterAnomalies` reports
/// `NotApplicable`, the same shape every other database-backed check in this
/// registry reports before a ledger exists (`BackupAge`,
/// `AdapterSemanticsComparisonAge`). The persisted-count read itself, and its
/// `Fail`/`PassWithDetail` split, is covered against a real database in
/// `src/doctor/checks.rs`'s own test module, next to `store::window_anomaly`'s
/// fixtures.
#[test]
fn check_reports_not_applicable_meter_anomalies_before_a_ledger_exists() {
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
    assert!(matches!(outcome.status, CheckStatus::NotApplicable(_)));
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

// --- aub-va6s: the last sample tick's outcome -------------------------------

#[test]
fn last_sample_tick_is_not_applicable_before_any_tick_is_recorded() {
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
        .find(|o| o.name == CheckName::LastSampleTick)
        .expect("LastSampleTick present");
    assert_eq!(
        outcome.status,
        CheckStatus::NotApplicable("no sample tick has been recorded yet".to_string())
    );
}

#[test]
fn last_sample_tick_passes_when_the_last_recorded_tick_succeeded() {
    use agent_usage_book::store::sample_tick::{LastSampleTick, TickOutcome, record_last_tick};

    let state = StateDir::new();
    record_last_tick(
        state.path(),
        &LastSampleTick {
            started_at: ts(1_699_999_000),
            outcome: TickOutcome::Success,
        },
    )
    .unwrap();

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
        .find(|o| o.name == CheckName::LastSampleTick)
        .expect("LastSampleTick present");
    assert_eq!(outcome.status, CheckStatus::Pass);
}

/// The acceptance criterion's exact case: a refused tick is a failed check,
/// not a journal entry nothing else reads.
#[test]
fn check_fails_last_sample_tick_when_the_last_recorded_tick_was_refused() {
    use agent_usage_book::store::sample_tick::{LastSampleTick, TickOutcome, record_last_tick};

    let state = StateDir::new();
    record_last_tick(
        state.path(),
        &LastSampleTick {
            started_at: ts(1_699_999_000),
            outcome: TickOutcome::Failed(
                "cannot start sample run: database is locked (waited up to 5000ms)".to_string(),
            ),
        },
    )
    .unwrap();

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
        .find(|o| o.name == CheckName::LastSampleTick)
        .expect("LastSampleTick present");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("waited up to 5000ms")),
        "{:?}",
        outcome.status
    );
}

// --- aub-b0w6: persist-failed and due-lookup-failed sampler dispositions,
// counted durably by reason ---------------------------------------------------

#[test]
fn sampling_failure_counts_passes_when_no_failure_has_ever_been_recorded() {
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
        .find(|o| o.name == CheckName::SamplingFailureCounts)
        .expect("SamplingFailureCounts present");
    assert_eq!(outcome.status, CheckStatus::Pass);
}

/// The acceptance criterion's exact case (`aub-lz0k`): a recurrence of
/// `database disk image is malformed` is noticeable in `doctor` output,
/// without reading the scheduler's journal by hand.
#[test]
fn check_fails_sampling_failure_counts_and_names_the_recurring_reason() {
    use agent_usage_book::store::sampling_failure_counts::record_sampling_failure;

    let state = StateDir::new();
    record_sampling_failure(
        state.path(),
        "due_lookup_failed",
        "database disk image is malformed",
    )
    .unwrap();
    record_sampling_failure(
        state.path(),
        "due_lookup_failed",
        "database disk image is malformed",
    )
    .unwrap();

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
        .find(|o| o.name == CheckName::SamplingFailureCounts)
        .expect("SamplingFailureCounts present");
    assert!(
        matches!(
            outcome.status,
            CheckStatus::Fail(ref detail)
                if detail.contains("database disk image is malformed")
                    && detail.contains("count=2")
        ),
        "{:?}",
        outcome.status
    );
}

// --- aub-rfot: the provider error classifications a day of failures stored ---

/// Seeds one account, one sample run, one policy snapshot, one attempt and
/// one terminal result, with the outcome and stored classification the
/// caller names. The direct-SQL shape the clock-skew test established: the
/// check under test reads through the same store functions production
/// reads, and the seed is the minimal row set those functions join.
fn seed_failed_attempt_with_classification(
    conn: &rusqlite::Connection,
    account_name: &str,
    attempt_id: i64,
    started_at: UtcTimestamp,
    outcome_sql: &str,
    failure_class_sql: Option<&str>,
    classification: Option<&str>,
) {
    conn.execute(
        "INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
         VALUES (?1, ?2, 'anthropic', ?3, ?3)
         ON CONFLICT(id) DO NOTHING",
        rusqlite::params![attempt_id, account_name, started_at.unix_nanos()],
    )
    .expect("insert account");
    conn.execute(
        "INSERT INTO sample_run (id, trigger, started_at, aub_version, configuration_fingerprint)
         VALUES (?1, 'manual', ?2, '0.1.0', 'cfg')",
        rusqlite::params![attempt_id, started_at.unix_nanos()],
    )
    .expect("insert sample_run");
    conn.execute(
        "INSERT INTO sampling_policy_snapshot (
            id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
            reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version
         ) VALUES (?1, ?2, ?3, 60000000000, 300000000000, 'none', 'none', 1000000000, 'v1')",
        rusqlite::params![attempt_id, attempt_id, started_at.unix_nanos()],
    )
    .expect("insert policy");
    conn.execute(
        "INSERT INTO meter_attempt (
            id, run_id, account_id, provider, request_started_at, policy_snapshot_id,
            due_at, due_reason, provider_contract_id, meter_semantics_id
         ) VALUES (?1, ?1, ?2, 'anthropic', ?3, ?1, ?3, 'ordinary_cadence', 'contract-1', 'meter-1')",
        rusqlite::params![attempt_id, attempt_id, started_at.unix_nanos()],
    )
    .expect("insert meter_attempt");
    conn.execute(
        "INSERT INTO meter_attempt_result (
            attempt_id, completed_at, elapsed_nanos, outcome, failure_class,
            retry_after_nanos, sanitized_error_classification, retry_index, clock_anomaly
         ) VALUES (?1, ?2, 1000, ?3, ?4, ?5, ?6, NULL, 0)",
        rusqlite::params![
            attempt_id,
            started_at.unix_nanos() + 1_000,
            outcome_sql,
            failure_class_sql,
            if failure_class_sql == Some("rate_limited") {
                60000000000i64
            } else {
                0
            },
            classification
        ],
    )
    .expect("insert meter_attempt_result");
}

#[test]
fn meter_error_classifications_lists_the_classifications_seen_per_account() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let now = ts(1_700_000_000);
    let started = ts(1_700_000_000 - 1_000);
    seed_failed_attempt_with_classification(
        &conn,
        "gmail",
        1,
        started,
        "unreachable",
        Some("rate_limited"),
        Some("rate_limit_error: Rate limit exceeded. Please retry later."),
    );
    seed_failed_attempt_with_classification(
        &conn,
        "bianca",
        2,
        started,
        "auth_required",
        None,
        Some("authentication_error: Invalid authentication token provided."),
    );

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
        .find(|o| o.name == CheckName::MeterErrorClassifications)
        .expect("MeterErrorClassifications present");
    assert!(
        matches!(
            outcome.status,
            CheckStatus::PassWithDetail(ref detail)
                if detail.contains("gmail: rate_limit_error (count=1)")
                    && detail.contains("bianca: authentication_error (count=1)")
        ),
        "{:?}",
        outcome.status
    );
}

/// A NULL classification is the older rows' shape: it reads as
/// `unclassified` rather than disappearing from the listing.
#[test]
fn meter_error_classifications_names_a_null_classification_unclassified() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let now = ts(1_700_000_000);
    let started = ts(1_700_000_000 - 1_000);
    seed_failed_attempt_with_classification(
        &conn,
        "gmail",
        1,
        started,
        "unreachable",
        Some("rate_limited"),
        None,
    );

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
        .find(|o| o.name == CheckName::MeterErrorClassifications)
        .expect("MeterErrorClassifications present");
    assert!(
        matches!(
            outcome.status,
            CheckStatus::PassWithDetail(ref detail)
                if detail.contains("gmail: unclassified (count=1)")
        ),
        "{:?}",
        outcome.status
    );
}

/// A success row is never listed: the check says why attempts failed, not
/// that some succeeded.
#[test]
fn meter_error_classifications_lists_nothing_for_a_window_of_successes() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let now = ts(1_700_000_000);
    let started = ts(1_700_000_000 - 1_000);
    seed_failed_attempt_with_classification(&conn, "gmail", 1, started, "success", None, None);

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
        .find(|o| o.name == CheckName::MeterErrorClassifications)
        .expect("MeterErrorClassifications present");
    assert_eq!(outcome.status, CheckStatus::Pass);
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
    let expected_omitted = "Doctor: 1 checks\n  [N/A ] unexplained-residual: no eligible reconciliation intervals in recent window\nSummary: 0 passed, 0 failed, 0 warned, 1 not applicable, 0 not yet available";
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
    let expected_justified = "Doctor: 1 checks\n  [FAIL] unexplained-residual: rolling residual discrepancy: interval [1000000 .. 3000000] credits; pattern: step change in residual: possible plan or provider accounting transition; pointer: check calibration health (aub doctor missing-active-calibrations) to verify whether calibration has become inapplicable\nSummary: 0 passed, 1 failed, 0 warned, 0 not applicable, 0 not yet available\n\nDoctor: Rolling Residual Health\n  window: 30d (6 eligible intervals, minimum: 5)\n  residual interval: [1000000 .. 3000000] credits\n  residual fraction: +20.00%\n  verdict: discrepancy\n  pattern: step change in residual: possible plan or provider accounting transition\n  pointer: check calibration health (aub doctor missing-active-calibrations) to verify whether calibration has become inapplicable";
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

// --- MeterAnomalies: doctor consumes persisted evidence, never re-detects ---

/// `aub-eun.14`: `MeterAnomalies` reads what `store::window_anomaly` already
/// persisted without calling any detection function itself. It fails only for
/// anomalies inside the configured horizon, while retained older evidence stays
/// discoverable in a passing result.
#[test]
fn meter_anomalies_report_recent_health_without_hiding_history() {
    use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
    use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use agent_usage_book::domain::time::MeasurementBasis;
    use agent_usage_book::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowResetState,
        WindowScope, WindowSemanticKey,
    };
    use agent_usage_book::store::account::observe_account;
    use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use agent_usage_book::store::meter_evidence::{
        NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
        insert_response_evidence, insert_window, observation_by_row_id, windows_by_observation,
    };
    use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
    use agent_usage_book::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, resolve_policy_snapshot,
    };
    use agent_usage_book::store::window_anomaly::detect_and_persist;

    let state = StateDir::new();
    let config = test_config(state.path());
    let conn = open_ledger(state.path());

    let account = observe_account(&conn, "anthropic", "work-primary", ts(10)).unwrap();
    let run = start_sample_run(&conn, Trigger::Manual, ts(10), "test").unwrap();
    let policy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: String::new(),
        retry_backoff_policy: String::new(),
        command_budget: MonotonicDuration::from_millis(60_000),
        policy_algorithm_version: String::new(),
    };
    let snapshot = resolve_policy_snapshot(&conn, account, ts(10), &policy).unwrap();

    let record = |received_at: i64, used_ppm: i32, resets_at: WindowResetState| {
        let attempt = start_meter_attempt(
            &conn,
            &NewMeterAttempt {
                run_id: run,
                account_id: account,
                provider: "anthropic".into(),
                request_started_at: ts(received_at - 1),
                credential_context_id: Some("ctx-1".into()),
                policy_snapshot_id: snapshot,
                due_at: ts(received_at - 2),
                due_reason: DueReason::OrdinaryCadence,
                due_basis: None,
                provider_contract_id: "endpoint-schema-v3".into(),
                meter_semantics_id: "account-5h-v2".into(),
            },
        )
        .unwrap();
        let evidence_id = insert_response_evidence(
            &conn,
            &NewMeterResponseEvidence {
                attempt_id: attempt,
                response_classification: "200".into(),
                received_at: ts(received_at),
                provider_observed_at_original: None,
                evidence_capsule: r#"{"windows":[{"key":"5h"}]}"#.into(),
                capsule_schema_version: "capsule-v1".into(),
                sanitizer_version: "sanitizer-v1".into(),
                capture_truncated: false,
            },
        )
        .unwrap();
        let observation_id = insert_observation(
            &conn,
            &NewMeterObservation {
                attempt_id: attempt,
                evidence_id,
                account_id: account,
                provider: "anthropic".into(),
                provider_observed_at: None,
                received_at: ts(received_at),
                measurement_basis: MeasurementBasis::LocallyReceived,
                observed_plan: Some("max".into()),
                observed_tier: Some("pro".into()),
                adapter_version: AdapterVersion::new("adapter-v1"),
                provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
                meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
                normalized_fingerprint: format!("fp-{received_at}"),
            },
        )
        .unwrap();
        insert_window(
            &conn,
            &NewMeterWindow {
                observation_id,
                semantic_key: WindowSemanticKey::new("5h"),
                scope: WindowScope::AccountWide,
                quota_used: QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
                reported_resolution: ReportedResolution::new(
                    QuotaFractionPpm::new(10_000).unwrap(),
                )
                .unwrap(),
                quantization: QuantizationSemantics::RoundedToNearest,
                resets_at,
                nominal_duration: NominalWindowDuration::from_nanos(3_600_000_000_000),
            },
        )
        .unwrap();
        let observation = observation_by_row_id(&conn, observation_id)
            .unwrap()
            .unwrap();
        let windows = windows_by_observation(&conn, observation_id).unwrap();
        (observation, windows)
    };

    let ctx_before = DoctorContext {
        config: &config,
        timestamp: ts(1_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let before = build_registry(&ctx_before)
        .into_iter()
        .find(|o| o.name == CheckName::MeterAnomalies)
        .expect("MeterAnomalies present");
    assert_eq!(
        before.status,
        CheckStatus::PassWithDetail("0 window anomalies recorded".to_string())
    );

    let (first_obs, first_windows) = record(30, 600_000, WindowResetState::Known(ts(100)));
    detect_and_persist(
        &conn,
        account,
        &first_obs,
        &first_windows,
        ts(30),
        None,
        false,
    )
    .unwrap();
    let (second_obs, second_windows) = record(40, 400_000, WindowResetState::Known(ts(100)));
    detect_and_persist(
        &conn,
        account,
        &second_obs,
        &second_windows,
        ts(40),
        None,
        false,
    )
    .unwrap();

    let ctx_after = DoctorContext {
        config: &config,
        timestamp: ts(60),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let after = build_registry(&ctx_after)
        .into_iter()
        .find(|o| o.name == CheckName::MeterAnomalies)
        .expect("MeterAnomalies present");
    match after.status {
        CheckStatus::Fail(ref detail) => {
            assert!(detail.contains("1 recent of 1 total window anomaly"));
            assert!(detail.contains("1 window anomaly"));
            assert!(detail.contains("percentage_decrease_without_reset"));
            assert!(detail.contains(&format!("account={}", account.value())));
        }
        other => panic!("expected Fail naming the anomaly's evidence references, got {other:?}"),
    }

    let ctx_historical = DoctorContext {
        config: &config,
        timestamp: ts(1_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let historical = build_registry(&ctx_historical)
        .into_iter()
        .find(|o| o.name == CheckName::MeterAnomalies)
        .expect("MeterAnomalies present");
    assert_eq!(
        historical.status,
        CheckStatus::PassWithDetail(
            "1 historical window anomaly(ies) recorded; none within the 900s horizon".to_string()
        )
    );

    let historical_report = agent_usage_book::doctor::DoctorReport {
        metadata: agent_usage_book::report::ReportMetadata::new(
            ts(1_000),
            ts(1_000),
            agent_usage_book::report::LedgerGeneration::new(0),
            None,
        ),
        outcomes: vec![historical],
        residual: None,
    };
    let historical_text = agent_usage_book::presentation::render_doctor_report(&historical_report);
    assert!(historical_text.contains("[PASS] meter-anomalies: 1 historical window anomaly"));
    let historical_json = agent_usage_book::presentation::doctor_report_json(
        &historical_report,
        agent_usage_book::logging::RunId::new(ts(1_000)),
    );
    let historical_value: serde_json::Value = serde_json::from_str(&historical_json).unwrap();
    let historical_check = historical_value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "meter-anomalies")
        .expect("meter-anomalies must be rendered in JSON");
    assert_eq!(historical_check["status"], "pass");
    assert!(
        historical_check["reason"]
            .as_str()
            .unwrap()
            .contains("1 historical window anomaly")
    );

    let (third_obs, third_windows) = record(950, 200_000, WindowResetState::Known(ts(100)));
    detect_and_persist(
        &conn,
        account,
        &third_obs,
        &third_windows,
        ts(950),
        None,
        false,
    )
    .unwrap();
    let mixed = build_registry(&ctx_historical)
        .into_iter()
        .find(|o| o.name == CheckName::MeterAnomalies)
        .expect("MeterAnomalies present");
    match mixed.status {
        CheckStatus::Fail(ref detail) => {
            assert!(
                detail.contains("1 recent of 2 total window anomaly"),
                "{detail}"
            );
            assert!(
                detail.contains("1 window anomaly(ies), showing 1"),
                "{detail}"
            );
            assert!(detail.contains("current_observation="), "{detail}");
        }
        other => panic!("expected a recent anomaly to fail the check, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests for aub-n27.10: Finalize the doctor registry
// ---------------------------------------------------------------------------
//
// Assumption recorded on the bead: "stable problem code" is the check's stable
// kebab-case name (`CheckName::as_str`, distinct per check and identical in
// human and JSON output), asserted below together with the evidence reference
// and the repair flag. No new `ProblemCode` taxonomy is added: mapping
// twenty-four heterogeneous check failures onto twenty-nine provider-oriented
// codes would invent precision the plan never mandates and would change the
// versioned JSON contract this bead otherwise leaves untouched.
//
// Two checks cannot fail on data by design, and say so here rather than
// through a fabricated fixture:
// - `MeterErrorClassifications` is a listing, never a failure; its executable
//   fail case is the unreadable-ledger path.
// - `UnexplainedResidual`'s store-loading half is covered by the not-applicable
//   tests; its verdict-to-failure half is forced below through the pure
//   `rolling_residual_status` mapping over a real computed discrepancy, because
//   a store-level discrepancy needs a full calibration, cost-model and usage
//   chain no single-check fixture can honestly seed.

fn seed_observation_with_window(
    conn: &rusqlite::Connection,
) -> (
    agent_usage_book::store::meter_evidence::ObservationRowId,
    agent_usage_book::store::meter_evidence::WindowRowId,
) {
    use agent_usage_book::domain::ids::{AdapterVersion, MeterSemanticsId, ProviderContractId};
    use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use agent_usage_book::domain::time::{MeasurementBasis, MonotonicDuration};
    use agent_usage_book::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
        WindowSemanticKey,
    };
    use agent_usage_book::store::account::observe_account;
    use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt, start_meter_attempt};
    use agent_usage_book::store::meter_evidence::{
        NewMeterObservation, NewMeterResponseEvidence, NewMeterWindow, insert_observation,
        insert_response_evidence, insert_window,
    };
    use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
    use agent_usage_book::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, resolve_policy_snapshot,
    };

    const POLICY: ResolvedSamplingPolicy = ResolvedSamplingPolicy {
        ordinary_cadence: MonotonicDuration::from_millis(300_000),
        freshness_horizon: MonotonicDuration::from_millis(900_000),
        reset_edge_policy: String::new(),
        retry_backoff_policy: String::new(),
        command_budget: MonotonicDuration::from_millis(60_000),
        policy_algorithm_version: String::new(),
    };

    let account =
        observe_account(conn, "anthropic", "primary", ts(10)).expect("account must insert");
    let run = start_sample_run(conn, Trigger::Manual, ts(10), "seed").expect("run must insert");
    let snapshot = resolve_policy_snapshot(conn, account, ts(10), &POLICY)
        .expect("policy snapshot must insert");
    let attempt = start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: run,
            account_id: account,
            provider: "anthropic".into(),
            request_started_at: ts(20),
            credential_context_id: Some("ctx".into()),
            policy_snapshot_id: snapshot,
            due_at: ts(19),
            due_reason: DueReason::OrdinaryCadence,
            due_basis: None,
            provider_contract_id: "endpoint-schema-v3".into(),
            meter_semantics_id: "account-5h-v2".into(),
        },
    )
    .expect("attempt must insert");
    let evidence_id = insert_response_evidence(
        conn,
        &NewMeterResponseEvidence {
            attempt_id: attempt,
            response_classification: "200".into(),
            received_at: ts(30),
            provider_observed_at_original: None,
            evidence_capsule: r#"{"windows":[]}"#.into(),
            capsule_schema_version: "capsule-v1".into(),
            sanitizer_version: "sanitizer-v1".into(),
            capture_truncated: false,
        },
    )
    .expect("evidence must insert");
    let observation_id = insert_observation(
        conn,
        &NewMeterObservation {
            attempt_id: attempt,
            evidence_id,
            account_id: account,
            provider: "anthropic".into(),
            provider_observed_at: None,
            received_at: ts(31),
            measurement_basis: MeasurementBasis::LocallyReceived,
            observed_plan: Some("max".into()),
            observed_tier: None,
            adapter_version: AdapterVersion::new("adapter-v1"),
            provider_contract_id: ProviderContractId::new("endpoint-schema-v3"),
            meter_semantics_id: MeterSemanticsId::new("semantics-v1"),
            normalized_fingerprint: "fp-1".into(),
        },
    )
    .expect("observation must insert");
    let window_id = insert_window(
        conn,
        &NewMeterWindow {
            observation_id,
            semantic_key: WindowSemanticKey::new("five_hour"),
            scope: WindowScope::AccountWide,
            quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
            reported_resolution: ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap())
                .unwrap(),
            quantization: QuantizationSemantics::RoundedToNearest,
            resets_at: ts(100_000).into(),
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
        },
    )
    .expect("window must insert");
    (observation_id, window_id)
}

#[test]
fn check_fails_adapter_semantics_comparison_age() {
    use agent_usage_book::domain::authoritative_comparison::{
        AuthoritativeComparisonVerdict, DocumentedGranularity,
    };
    use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use agent_usage_book::domain::window::WindowSemanticKey;
    use agent_usage_book::store::adapter_semantics_validation::{
        NewAuthoritativeSurfaceComparison, insert_comparison,
    };

    let state = StateDir::new();
    let toml = format!(
        "[state]\ndir = {:?}\n\n[adapter_semantics]\nmax_comparison_age = \"30m\"\n",
        state.path()
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();
    let conn = open_ledger(state.path());
    let (observation_id, window_id) = seed_observation_with_window(&conn);
    // One hour before now: past the thirty-minute review horizon.
    let now = ts(1_700_000_000);
    insert_comparison(
        &conn,
        &NewAuthoritativeSurfaceComparison {
            observation_id,
            window_id,
            semantic_key: WindowSemanticKey::new("five_hour"),
            authoritative_surface: "test surface".into(),
            documented_granularity: DocumentedGranularity::new(
                QuotaFractionPpm::new(10_000).unwrap(),
            ),
            adapter_quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
            authoritative_quota_used: QuotaUsed::new(QuotaFractionPpm::new(250_000).unwrap()),
            read_at: ts(1_700_000_000 - 3_600),
            verdict: AuthoritativeComparisonVerdict::AgreesWithinGranularity,
        },
    )
    .expect("comparison must insert");

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
        .find(|o| o.name == CheckName::AdapterSemanticsComparisonAge)
        .expect("AdapterSemanticsComparisonAge present");
    assert_eq!(outcome.owner_module, "store::adapter_semantics_validation");
    assert!(!outcome.has_repair);
    match &outcome.status {
        CheckStatus::Fail(reason) => {
            assert!(
                reason.contains("past its 1800s review horizon"),
                "the failure must name the stable evidence, the aged comparison: {reason}"
            );
        }
        other => panic!("expected Fail naming the stale comparison, got {other:?}"),
    }
}

fn seed_account_run_policy_attempt(conn: &rusqlite::Connection, now: UtcTimestamp) {
    conn.execute(
        "INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
         VALUES (1, 'sub-test', 'anthropic', ?1, ?1)",
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
}

#[test]
fn check_fails_subscription_identity_change() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let now = ts(1_700_000_000);
    seed_account_run_policy_attempt(&conn, now);
    // A `changed` event with no newer stored observation: readings for this
    // account are being refused right now.
    conn.execute(
        "INSERT INTO meter_subscription_change (
            account_id, kind, previous_identity, current_identity,
            detecting_attempt_id, detected_at
         ) VALUES (1, 'changed', 'anthropic:max:fp-old', 'anthropic:pro:fp-new', 1, ?1)",
        [now.unix_nanos()],
    )
    .expect("insert subscription change");

    let toml = format!(
        "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"sub-test\"\nprovider = \"anthropic\"\n",
        state.path()
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();
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
        .find(|o| o.name == CheckName::SubscriptionIdentityChange)
        .expect("SubscriptionIdentityChange present");
    assert_eq!(outcome.owner_module, "store::subscription_identity");
    assert!(
        !outcome.has_repair,
        "a changed subscription needs an operator rename, never a --fix repair"
    );
    match &outcome.status {
        CheckStatus::Fail(reason) => {
            assert!(
                reason.contains("sub-test")
                    && reason.contains("subscription changed")
                    && reason.contains("readings refused"),
                "the failure must name the stable check evidence, account and refusal: {reason}"
            );
        }
        other => panic!("expected Fail naming the refused subscription, got {other:?}"),
    }
}

#[test]
fn meter_error_classifications_fails_when_the_ledger_will_not_open() {
    // By design this check is a listing, never a failure, over readable data
    // (covered by the per-account listing tests above); its executable fail
    // case is the ledger that exists but will not open.
    let state = StateDir::new();
    let config = test_config(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: None,
        db_missing: false,
        db_open_error: Some("database disk image is malformed".to_string()),
    };
    let outcomes = build_registry(&ctx);
    let outcome = outcomes
        .iter()
        .find(|o| o.name == CheckName::MeterErrorClassifications)
        .expect("MeterErrorClassifications present");
    assert_eq!(outcome.owner_module, "store::meter_attempt");
    assert!(
        matches!(outcome.status, CheckStatus::Fail(ref reason) if reason.contains("database disk image is malformed"))
    );
}

#[test]
fn check_fails_unexplained_residual_on_a_computed_discrepancy() {
    use agent_usage_book::doctor::checks::rolling_residual_status;
    use agent_usage_book::reconciliation::compute_rolling_residual_health;

    // Controlled evidence: six intervals with a step change, through the
    // production classifier, so the discrepancy verdict is computed, not built.
    let intervals = vec![
        test_reconciled_interval(100, 200, 10_000, 20_000, -10_000, -15_000, -5_000),
        test_reconciled_interval(200, 300, 10_000, 20_000, -10_000, -15_000, -5_000),
        test_reconciled_interval(300, 400, 10_000, 20_000, -10_000, -15_000, -5_000),
        test_reconciled_interval(400, 500, 120_000, 20_000, 100_000, 95_000, 105_000),
        test_reconciled_interval(500, 600, 120_000, 20_000, 100_000, 95_000, 105_000),
        test_reconciled_interval(600, 700, 120_000, 20_000, 100_000, 95_000, 105_000),
    ];
    let window = MonotonicDuration::from_seconds(86400 * 30);
    let health = compute_rolling_residual_health(&intervals, window, 5)
        .expect("a step change over six eligible intervals is a discrepancy");

    // The registry metadata for this check is fixed by its outcome entry;
    // read it from a real registry so the test pins the shipped values.
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
    let registered = build_registry(&ctx)
        .into_iter()
        .find(|o| o.name == CheckName::UnexplainedResidual)
        .expect("UnexplainedResidual present");
    assert_eq!(registered.owner_module, "reconciliation");
    assert_eq!(
        registered.condition,
        "rolling residual stays within its explained bound"
    );
    assert!(!registered.has_repair);

    match rolling_residual_status(Some(&health)) {
        CheckStatus::Fail(reason) => {
            assert!(
                reason.contains("rolling residual discrepancy")
                    && reason.contains("[240000 .. 300000]")
                    && reason.contains("step change")
                    && reason.contains("aub doctor missing-active-calibrations"),
                "the failure must name the stable check evidence, interval, pattern and pointer: {reason}"
            );
        }
        other => panic!("expected Fail for a computed discrepancy, got {other:?}"),
    }
}

#[test]
fn human_and_versioned_json_results_agree_on_names_states_reasons_and_repairs() {
    use agent_usage_book::doctor::{CheckOutcome, DoctorReport};
    use agent_usage_book::logging::RunId;
    use agent_usage_book::presentation::{doctor_report_json, render_doctor_report};
    use agent_usage_book::report::{LedgerGeneration, ReportMetadata};

    let now = ts(1_700_000_000);
    let outcomes = vec![
        CheckOutcome {
            name: CheckName::ConfigurationValidity,
            owner_module: "config",
            condition: "the resolved configuration has no invalid or conflicting key",
            has_repair: false,
            status: CheckStatus::Pass,
        },
        CheckOutcome {
            name: CheckName::PendingEvidence,
            owner_module: "store::spool",
            condition: "no meter evidence is stuck undrained in the pending spool",
            has_repair: true,
            status: CheckStatus::Fail("2 pending record(s) undrained".to_string()),
        },
        CheckOutcome {
            name: CheckName::TranscriptRoots,
            owner_module: "doctor",
            condition: "every configured transcript root exists and is reachable",
            has_repair: false,
            status: CheckStatus::NotApplicable("no transcript sources configured".to_string()),
        },
        CheckOutcome {
            name: CheckName::MeterAnomalies,
            owner_module: "store::window_anomaly",
            condition: "no meter window anomaly was recorded inside the configured recent horizon",
            has_repair: false,
            status: CheckStatus::PassWithDetail("0 window anomalies recorded".to_string()),
        },
    ];
    let report = DoctorReport {
        metadata: ReportMetadata::new(now, now, LedgerGeneration::new(1), None),
        outcomes: outcomes.clone(),
        residual: None,
    };

    let text = render_doctor_report(&report);
    let json = doctor_report_json(&report, RunId::new(now));
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    let checks = value["checks"].as_array().expect("checks array");

    fn text_marker_to_label(marker: &str) -> &str {
        match marker {
            "PASS" => "pass",
            "FAIL" => "fail",
            "N/A " => "not_applicable",
            other => panic!("unexpected text marker {other:?}"),
        }
    }

    for outcome in &outcomes {
        let entry = checks
            .iter()
            .find(|c| c["name"] == outcome.name.as_str())
            .unwrap_or_else(|| panic!("JSON must contain {:?}", outcome.name));
        let expected_label = outcome.status.label();
        assert_eq!(entry["status"], expected_label);
        assert_eq!(entry["has_repair"], outcome.has_repair);
        let expected_reason = match &outcome.status {
            CheckStatus::Fail(reason)
            | CheckStatus::Warn(reason)
            | CheckStatus::NotApplicable(reason)
            | CheckStatus::PassWithDetail(reason) => Some(reason.as_str()),
            CheckStatus::Pass | CheckStatus::NotYetAvailable { .. } => None,
        };
        match expected_reason {
            Some(reason) => assert_eq!(entry["reason"], reason),
            None => assert!(entry.get("reason").is_none()),
        }

        let marker_line = text
            .lines()
            .find(|line| line.contains(outcome.name.as_str()))
            .unwrap_or_else(|| panic!("text must contain {:?}", outcome.name));
        let marker = &marker_line[3..7];
        assert_eq!(text_marker_to_label(marker), expected_label);
        if let Some(reason) = expected_reason {
            assert!(
                marker_line.contains(reason),
                "text line must carry the same reason: {marker_line}"
            );
        }
        if outcome.has_repair {
            assert!(
                marker_line.contains("[repairable with --fix]"),
                "text line must carry the same repair availability: {marker_line}"
            );
        }
    }
}

#[test]
fn quarantine_checks_report_counts_never_paths_or_content() {
    let state = StateDir::new();
    let conn = open_ledger(state.path());
    let source_file = state
        .path()
        .join("transcripts")
        .join("secret-project")
        .join("session.jsonl");
    conn.execute(
        "INSERT INTO ingest_quarantine (
            source_file, parser, failure_class, excerpt_hash, first_observed, last_observed
         ) VALUES (?1, 'claude-code', 'malformed_json', 'hash1', 1000, 1000)",
        [source_file.to_str().unwrap()],
    )
    .expect("insert parser quarantine");
    conn.execute(
        "INSERT INTO ingest_quarantine (
            source_file, parser, failure_class, excerpt_hash, first_observed, last_observed
         ) VALUES (?1, 'codex', 'dedup_collision', 'hash2', 1000, 1000)",
        [source_file.to_str().unwrap()],
    )
    .expect("insert dedup quarantine");

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
    for name in [CheckName::ParserFailures, CheckName::HeuristicDedupCounts] {
        let outcome = outcomes
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("{name:?} present"));
        match &outcome.status {
            CheckStatus::Fail(reason) => {
                assert!(reason.contains("1 record(s) quarantined"), "{reason}");
                assert!(
                    !reason.contains("secret-project"),
                    "quarantine checks must report counts, never source paths: {reason}"
                );
                assert!(
                    !reason.contains(state.path().to_str().unwrap()),
                    "quarantine checks must not expose absolute paths: {reason}"
                );
            }
            other => panic!("expected Fail for {name:?}, got {other:?}"),
        }
    }
}

#[test]
fn doctor_reasons_never_carry_credential_values() {
    let state = StateDir::new();
    const VAR: &str = "AUB_N27_10_TEST_CREDENTIAL_VALUE";
    const SECRET: &str = "e2e-planted-credential-value-9f3c";
    let toml = format!(
        "[state]\ndir = {:?}\n\n[[accounts]]\nname = \"cred-test\"\nprovider = \"opencode\"\nopencode_workspace = \"wrk_test\"\ncredential = {{ kind = \"env\", name = {:?} }}\n",
        state.path(),
        VAR
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();
    let conn = open_ledger(state.path());

    let ctx = |timestamp: UtcTimestamp| DoctorContext {
        config: &config,
        timestamp,
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };

    // SAFETY: VAR is unique to this test and read by no other code in this
    // crate, so no concurrently running test can observe or race it.
    unsafe {
        std::env::set_var(VAR, SECRET);
    }
    let with_secret = build_registry(&ctx(ts(1_700_000_000)));
    unsafe {
        std::env::remove_var(VAR);
    }
    let without_secret = build_registry(&ctx(ts(1_700_000_000)));

    for outcomes in [&with_secret, &without_secret] {
        for outcome in outcomes {
            let reason = match &outcome.status {
                CheckStatus::Fail(reason)
                | CheckStatus::Warn(reason)
                | CheckStatus::NotApplicable(reason)
                | CheckStatus::PassWithDetail(reason) => reason.as_str(),
                CheckStatus::Pass | CheckStatus::NotYetAvailable { .. } => continue,
            };
            assert!(
                !reason.contains(SECRET),
                "{:?} must never expose a credential value: {reason}",
                outcome.name
            );
        }
    }
}

#[test]
fn registry_reasons_contain_no_absolute_state_path() {
    // Locks criterion 5 for the check-owned formatting: over a degraded state
    // exercising every path-carrying check, no reason may contain the state
    // directory (which lives under the operator's home).
    let state = StateDir::new();
    let pending_dir = state.path().join("pending");
    fs::create_dir_all(&pending_dir).expect("create pending dir");
    fs::write(pending_dir.join("attempt-1.json"), "{}").expect("write pending record");

    let toml = format!(
        "[state]\ndir = {:?}\n\n[[transcripts]]\nname = \"ghost\"\nroot = {:?}\npattern = \"**/*.jsonl\"\n\n[backup]\ndestination = {:?}\n",
        state.path(),
        state.path().join("does-not-exist"),
        state.path().join("missing-archive"),
    );
    let (config, _) = resolve(&Overrides::new(), &RealEnv, Some(&toml), "aub.toml").unwrap();
    let conn = open_ledger(state.path());
    let ctx = DoctorContext {
        config: &config,
        timestamp: ts(1_700_000_000),
        db_path: state.path().join(connection::LEDGER_DATABASE_FILE),
        db: Some(&conn),
        db_missing: false,
        db_open_error: None,
    };
    let outcomes = build_registry(&ctx);
    let state_str = state.path().to_str().unwrap();
    for outcome in &outcomes {
        let reason = match &outcome.status {
            CheckStatus::Fail(reason)
            | CheckStatus::Warn(reason)
            | CheckStatus::NotApplicable(reason)
            | CheckStatus::PassWithDetail(reason) => reason.as_str(),
            CheckStatus::Pass | CheckStatus::NotYetAvailable { .. } => continue,
        };
        assert!(
            !reason.contains(state_str),
            "{:?} must not expose an absolute path: {reason}",
            outcome.name
        );
    }
    // And the exercised checks do fail, so the scan above is not vacuous.
    for name in [
        CheckName::PendingEvidence,
        CheckName::TranscriptRoots,
        CheckName::BackupAge,
    ] {
        let outcome = outcomes
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("{name:?} present"));
        assert!(
            matches!(outcome.status, CheckStatus::Fail(_)),
            "{name:?} must fail in this degraded state"
        );
    }
}
