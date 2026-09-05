//! Import of the legacy regression fit as non-activatable calibration history.

use std::collections::BTreeSet;

use agent_usage_book::calibration::health::{
    ApplicabilityContext, CalibrationFacts, HealthInputs, LifecycleState, compute_health,
};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::provenance::{EvidenceId, WindowCalibrationId};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::domain::tokens::TokenKind;
use agent_usage_book::legacy_calibration::read_source;
use agent_usage_book::store::calibration::ConditionNumber;
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::legacy_calibration_import::{LEGACY_INCOMPLETE_COST_MODEL_ID, import};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use rusqlite::Connection;
use test_support::StateDir;

fn open_migrated_ledger(state: &StateDir) -> Connection {
    let path = state.path().join(LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&path, AccessMode::ReadWrite, &policy).expect("scratch ledger must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("scratch ledger must migrate");
    conn
}

fn fit_source(calibration_id: &str, fitted: i64, with_experiment: bool) -> String {
    let experiment = if with_experiment {
        r#","experiment":{"experiment_id":"legacy-fit-evidence-1","method":"ordinary-least-squares","evidence_ids":["legacy:test-obs-1","legacy:test-obs-2"]}"#
    } else {
        ""
    };
    format!(
        r#"{{"format":"legacy-calibration-v1","calibration_id":"{calibration_id}","provider":"anthropic","plan_tier":"default","window":"five_hour","fitted_micros_per_point":{fitted},"fit_timestamp":"2026-07-01T00:00:00Z","provenance":{{"origin":"legacy-regression-fit","note":"pre-rewrite regression"}} {experiment}}}"#
    )
}

fn import_fit(
    conn: &mut Connection,
    source_json: &str,
) -> agent_usage_book::store::legacy_meter_import::ImportSummary {
    let dir = std::env::temp_dir().join(format!(
        "aub-legacy-cal-import-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("source.json");
    std::fs::write(&path, source_json).unwrap();
    let parsed = read_source(&path).expect("fit-evidence source must parse");
    let summary = import(
        conn,
        &parsed,
        "backup-archive-1",
        UtcTimestamp::from_unix_nanos(2_000_000_000),
    )
    .expect("import must succeed");
    std::fs::remove_dir_all(&dir).ok();
    summary
}

#[test]
fn integration_fit_evidence_imports_as_history_and_hardcoded_copy_is_refused() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);

    let summary = import_fit(
        &mut conn,
        &fit_source("legacy-fit-history-1", 480_000, true),
    );
    assert_eq!(summary.imported, 1);
    assert_eq!(summary.unchanged, 0);

    let stored = agent_usage_book::store::calibration::load_result(
        &conn,
        &WindowCalibrationId::new("legacy-fit-history-1"),
    )
    .unwrap()
    .expect("legacy fit must be present as a calibration result");
    assert_eq!(stored.fitted().micros_per_point(), 480_000);
    assert_eq!(
        stored.fit_timestamp(),
        UtcTimestamp::parse_rfc3339("2026-07-01T00:00:00Z").unwrap()
    );

    let events = agent_usage_book::store::calibration::activation_events_for(
        &conn,
        &WindowCalibrationId::new("legacy-fit-history-1"),
    )
    .unwrap();
    assert!(
        events.is_empty(),
        "legacy history must carry no activation event"
    );

    // The identical coefficient without experiment evidence is not evidence.
    let dir = std::env::temp_dir().join(format!("aub-legacy-cal-copy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let copy_path = dir.join("copy.json");
    std::fs::write(
        &copy_path,
        fit_source("legacy-fit-history-1", 480_000, false),
    )
    .unwrap();
    let error = read_source(&copy_path).unwrap_err();
    assert!(
        error.to_string().contains("hardcoded copy"),
        "unexpected: {error}"
    );
    assert!(
        error.to_string().contains("experiment evidence"),
        "unexpected: {error}"
    );
    std::fs::remove_dir_all(&dir).ok();

    // No second history row was created by the refused copy.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_calibration_result",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn integration_repeated_import_is_idempotent() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_json = fit_source("legacy-fit-idem-1", 470_000, true);

    let dir = std::env::temp_dir().join(format!("aub-legacy-cal-idem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("source.json");
    std::fs::write(&path, &source_json).unwrap();
    let parsed = read_source(&path).expect("source must parse");
    let first = import(
        &mut conn,
        &parsed,
        "backup-archive-1",
        UtcTimestamp::from_unix_nanos(2_000_000_000),
    )
    .unwrap();
    assert_eq!(first.imported, 1);
    let second = import(
        &mut conn,
        &parsed,
        "backup-archive-1",
        UtcTimestamp::from_unix_nanos(3_000_000_000),
    )
    .unwrap();
    assert_eq!(second.imported, 0);
    assert_eq!(second.unchanged, 1);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM window_calibration_result",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unit_referenced_cost_model_is_incomplete_naming_cache_write() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    import_fit(&mut conn, &fit_source("legacy-fit-cost-1", 460_000, true));

    let model = agent_usage_book::store::cost_model::load_by_semantic_id(
        &conn,
        &agent_usage_book::domain::provenance::CostModelId::new(LEGACY_INCOMPLETE_COST_MODEL_ID),
    )
    .unwrap()
    .expect("legacy incomplete cost model must exist");
    assert!(
        !model.is_complete(),
        "the legacy cost model must be incomplete"
    );
    assert_eq!(model.missing_token_kinds(), vec![TokenKind::CacheWrite]);
}

#[test]
fn integration_activation_refused_naming_missing_class_from_general_rule() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    import_fit(
        &mut conn,
        &fit_source("legacy-fit-activate-1", 450_000, true),
    );
    import_fit(
        &mut conn,
        &fit_source("legacy-fit-activate-2", 450_000, true),
    );

    // The import splits the two evidence ids into disjoint fitting and
    // validation halves, so presenting those exact halves passes every
    // evidence check and leaves completeness as the only blocker.
    for calibration_id in ["legacy-fit-activate-1", "legacy-fit-activate-2"] {
        let training: BTreeSet<EvidenceId> =
            [EvidenceId::new("legacy:test-obs-1")].into_iter().collect();
        let validation: BTreeSet<EvidenceId> =
            [EvidenceId::new("legacy:test-obs-2")].into_iter().collect();
        let actor =
            agent_usage_book::calibration::activation::ActivationActor::new("operator").unwrap();
        let policy = agent_usage_book::calibration::activation::ActivationPolicy::new(
            "legacy-import-v1",
            Credits::from_micros(100_000),
            ConditionNumber::from_micros(30_000_000),
        )
        .unwrap();
        let verdict = agent_usage_book::calibration::contamination::ContaminationVerdict::clean();
        let request = agent_usage_book::calibration::activation::ActivationRequest {
            actor: &actor,
            policy: &policy,
            training: &training,
            validation: &validation,
            contamination: &verdict,
        };
        let error = agent_usage_book::store::calibration::activate(
            &mut conn,
            &WindowCalibrationId::new(calibration_id),
            UtcTimestamp::from_unix_nanos(4_000_000_000),
            None,
            &request,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("cache_write"),
            "refusal must name the missing token class, got: {message}"
        );
        assert!(
            message.contains(LEGACY_INCOMPLETE_COST_MODEL_ID),
            "refusal must name the incomplete cost model, got: {message}"
        );
        assert!(
            !message.contains(calibration_id),
            "refusal must come from the general completeness rule, not a record-specific check, got: {message}"
        );
    }

    let lifecycle_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM calibration_lifecycle", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(lifecycle_count, 0, "refused activations must write nothing");
}

#[test]
fn integration_repository_selection_refuses_legacy_naming_incomplete_cost_model() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    import_fit(&mut conn, &fit_source("legacy-fit-select-1", 440_000, true));

    let error = agent_usage_book::store::calibration::require_current_applicable(
        &conn,
        &WindowCalibrationId::new("legacy-fit-select-1"),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("cache_write"),
        "repository refusal must name the missing token class, got: {message}"
    );
    assert!(
        message.contains(LEGACY_INCOMPLETE_COST_MODEL_ID),
        "repository refusal must name the incomplete cost model, got: {message}"
    );
}

#[test]
fn unit_history_shows_record_with_non_current_health_and_show_omits_it() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    import_fit(&mut conn, &fit_source("legacy-fit-health-1", 430_000, true));

    let results = agent_usage_book::store::calibration::list_all_results(&conn).unwrap();
    assert!(
        results
            .iter()
            .any(|result| result.id().as_str() == "legacy-fit-health-1"),
        "calibrate history lists every result, including legacy history"
    );

    let calibration = agent_usage_book::store::calibration::load_result(
        &conn,
        &WindowCalibrationId::new("legacy-fit-health-1"),
    )
    .unwrap()
    .unwrap();
    let facts = CalibrationFacts {
        plan_tier: calibration.plan_tier().clone(),
        meter_semantics_id: calibration.meter_semantics_id().clone(),
        billing_semantics_id: calibration.billing_semantics_id().clone(),
    };
    let context = ApplicabilityContext {
        plan_tier: calibration.plan_tier().clone(),
        meter_semantics_id: calibration.meter_semantics_id().clone(),
        billing_semantics_id: calibration.billing_semantics_id().clone(),
    };
    let inputs = HealthInputs {
        calibration: &facts,
        context: &context,
        lifecycle: LifecycleState::NeverActivated,
        cost_model_superseded: false,
        drift: None,
        review_due_at: None,
    };
    let health = compute_health(&inputs, UtcTimestamp::from_unix_nanos(5_000_000_000));
    assert_ne!(
        health,
        agent_usage_book::calibration::health::CalibrationHealth::Current,
        "legacy history must never read as Current"
    );

    // `calibrate show` presents only active calibrations: the legacy scope
    // has no lifecycle event, so the active lookup returns nothing.
    let scope = calibration.scope();
    let active = agent_usage_book::store::calibration::load_active_at(
        &conn,
        &scope,
        UtcTimestamp::from_unix_nanos(5_000_000_000),
    )
    .unwrap();
    assert!(
        active.is_none(),
        "calibrate show must not present never-activated history as active"
    );
}
