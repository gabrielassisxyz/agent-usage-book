//! Contract and property tests for the versioned JSON presentation layer (aub-xus.3).

use std::collections::BTreeMap;
use std::path::Path;

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::freshness::{Freshness, Observed, StaleReason};
use agent_usage_book::domain::interval::Interval;
use agent_usage_book::domain::provenance::DerivationId;
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{MeasurementBasis, ReceivedAt, UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};
use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{
    JsonContractError, JsonEnvelope, Quantity, SCHEMA_VERSION, interval_from_json, interval_json,
    provenance_from_json, provenance_json, spend_json, status_json, validate_envelope_strict,
    validate_spend_report_json, validate_status_report_json,
};
use agent_usage_book::report::{
    IngestSummary, LedgerGeneration, MeterAccount, ReportMetadata, SpendFilter,
    SpendFilterExcluded, SpendFilterOutcome, SpendGroup, SpendGrouping, SpendReport, StatusReport,
};
use proptest::prelude::*;
use test_support::sanitization::matched_patterns;

fn test_metadata() -> ReportMetadata {
    ReportMetadata::new(
        UtcTimestamp::from_unix_nanos(2_000),
        UtcTimestamp::from_unix_nanos(1_000),
        LedgerGeneration::new(7),
        None,
    )
}

fn test_run_id() -> RunId {
    RunId::from_string("run-1000-2000-1".to_string())
}

fn remaining_ppm(ppm: u32) -> QuotaRemaining {
    QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap())
}

fn observed_reading(ppm: u32) -> Observed<QuotaRemaining> {
    Observed::new(
        remaining_ppm(ppm),
        None,
        ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
        MeasurementBasis::ProviderObserved,
    )
}

#[test]
fn contract_status_json_matches_golden_fixture() {
    let report = StatusReport::new(
        test_metadata(),
        vec![
            MeterAccount::new(
                LogicalName::new("primary"),
                Freshness::Fresh {
                    observed: observed_reading(500_000),
                    latest_attempt: AttemptId::new(1),
                },
            ),
            MeterAccount::new(
                LogicalName::new("secondary"),
                Freshness::Stale {
                    last_good: Some(observed_reading(250_000)),
                    latest_attempt: AttemptId::new(2),
                    reason: StaleReason::AgeExceeded,
                },
            ),
            MeterAccount::new(
                LogicalName::new("tertiary"),
                Freshness::AuthRequired {
                    last_good: None,
                    latest_attempt: AttemptId::new(3),
                },
            ),
        ],
        vec![],
        agent_usage_book::report::ProjectionReadState::Read,
    );

    let generated_json = status_json(&report, test_run_id());
    let parsed_generated: serde_json::Value =
        serde_json::from_str(&generated_json).expect("generated status JSON must parse");

    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/presentation/status_v5.json");
    let fixture_content =
        std::fs::read_to_string(&fixture_path).expect("fixture status_v5.json must exist");
    let parsed_fixture: serde_json::Value =
        serde_json::from_str(&fixture_content).expect("fixture must parse as JSON");

    assert_eq!(
        parsed_generated, parsed_fixture,
        "generated status JSON must match golden status_v5.json fixture"
    );

    let parsed_env = validate_status_report_json(&generated_json)
        .expect("status JSON must strictly validate against contract");
    assert_eq!(parsed_env.schema, SCHEMA_VERSION);
    assert_eq!(parsed_env.command, "status");
    assert_eq!(parsed_env.run.as_str(), "run-1000-2000-1");
}

#[test]
fn contract_spend_json_matches_golden_fixture() {
    let since = UtcDate::parse("2026-08-25").unwrap();
    let until = UtcDate::parse("2026-08-26").unwrap();
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(1000),
            OutputTokens::new(500),
            CacheReadTokens::new(200),
            CacheWriteTokens::new(100),
        ),
        BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );
    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![],
        vec![],
        agent_usage_book::domain::provenance::QuerySemantics::new("project", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&manifest);
    let groups = vec![SpendGroup::new(
        LogicalName::new("project-alpha"),
        usage,
        Provenance::new(["claude-code:session-1".to_string()]),
        derivation_id,
    )];
    let ingest = IngestSummary {
        refresh_attempted: false,
        refresh_failure: None,
        files_read: 1,
        files_skipped_before_window: 0,
        unreadable_files: vec![],
        quarantined_by_class: BTreeMap::new(),
        replayed_occurrences: 0,
        collisions: 0,
        without_identity: 0,
        heuristic_identities: 0,
        undated_events: 0,
        events_outside_window: 0,
        events_in_window: 1,
        working_directory_changes: 0,
    };
    let report = SpendReport::new(test_metadata(), since, until, groups, vec![], ingest);

    let generated_json = spend_json(&report, test_run_id());
    let parsed_generated: serde_json::Value =
        serde_json::from_str(&generated_json).expect("generated spend JSON must parse");

    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/presentation/spend_v5.json");
    let fixture_content =
        std::fs::read_to_string(&fixture_path).expect("fixture spend_v5.json must exist");
    let parsed_fixture: serde_json::Value =
        serde_json::from_str(&fixture_content).expect("fixture must parse as JSON");

    assert_eq!(
        parsed_generated, parsed_fixture,
        "generated spend JSON must match golden spend_v5.json fixture"
    );

    let parsed_env = validate_spend_report_json(&generated_json)
        .expect("spend JSON must strictly validate against contract");
    assert_eq!(parsed_env.schema, SCHEMA_VERSION);
    assert_eq!(parsed_env.command, "spend");
    assert_eq!(parsed_env.run.as_str(), "run-1000-2000-1");
}

/// The golden for a filtered, harness-grouped report: the `grouping` value
/// `harness` and the `filters[]` array the schema bump to v5 added (aub-satk).
/// The filtered report carries the filter's exclusion record under `filters[]`,
/// and the validation pass proves the array is a schema'd field rather than an
/// ad-hoc key.
#[test]
fn contract_spend_filtered_harness_json_matches_golden_fixture() {
    let since = UtcDate::parse("2026-09-10").unwrap();
    let until = UtcDate::parse("2026-09-11").unwrap();
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(400),
            OutputTokens::new(60),
            CacheReadTokens::new(20),
            CacheWriteTokens::new(0),
        ),
        BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );
    let manifest = agent_usage_book::domain::provenance::ProvenanceManifest::new(
        vec![],
        vec![],
        agent_usage_book::domain::provenance::QuerySemantics::new("harness", "none"),
    );
    let derivation_id = DerivationId::from_manifest(&manifest);
    let groups = vec![SpendGroup::new(
        LogicalName::new("harness=codex"),
        usage,
        Provenance::new(["codex:session-9".to_string()]),
        derivation_id,
    )];
    let ingest = IngestSummary {
        refresh_attempted: false,
        refresh_failure: None,
        files_read: 0,
        files_skipped_before_window: 0,
        unreadable_files: vec![],
        quarantined_by_class: BTreeMap::new(),
        replayed_occurrences: 0,
        collisions: 0,
        without_identity: 0,
        heuristic_identities: 0,
        undated_events: 0,
        events_outside_window: 0,
        events_in_window: 3,
        working_directory_changes: 2,
    };
    let filters = vec![SpendFilterOutcome::new(
        SpendFilter {
            flag: "--harness",
            dimension: SpendGrouping::Harness,
            values: ["codex".to_string()].into_iter().collect(),
        },
        SpendFilterExcluded {
            sessions: 2,
            events: 2,
            unknown_sessions: 0,
            unknown_events: 0,
        },
    )];
    let report = SpendReport::new(test_metadata(), since, until, groups, vec![], ingest)
        .with_grouping(vec![SpendGrouping::Harness])
        .with_filters(filters);

    let generated_json = spend_json(&report, test_run_id());
    let parsed_generated: serde_json::Value =
        serde_json::from_str(&generated_json).expect("generated spend JSON must parse");

    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/presentation/spend_filtered_harness_v5.json");
    let fixture_content = std::fs::read_to_string(&fixture_path)
        .expect("fixture spend_filtered_harness_v5.json must exist");
    let parsed_fixture: serde_json::Value =
        serde_json::from_str(&fixture_content).expect("fixture must parse as JSON");

    assert_eq!(
        parsed_generated, parsed_fixture,
        "generated filtered spend JSON must match golden spend_filtered_harness_v5.json fixture"
    );

    let parsed_env = validate_spend_report_json(&generated_json)
        .expect("spend JSON must strictly validate against contract");
    assert_eq!(parsed_env.schema, SCHEMA_VERSION);
    assert_eq!(parsed_env.command, "spend");
    // The contract covers both new grouping values and the filters array.
    let grouping: Vec<String> = parsed_generated["grouping"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect();
    assert!(grouping.contains(&"harness".to_string()));
    assert_eq!(
        parsed_generated["filters"].as_array().unwrap().len(),
        1,
        "the filtered report carries its exclusion record under filters[]"
    );
}

/// The grouping values `harness` and `model` serialize under the envelope's
/// `grouping` array, and `validate_spend_report_json` accepts them (aub-satk).
#[test]
fn contract_grouping_accepts_harness_and_model_values() {
    let report = SpendReport::new(
        test_metadata(),
        UtcDate::parse("2026-09-10").unwrap(),
        UtcDate::parse("2026-09-11").unwrap(),
        Vec::new(),
        Vec::new(),
        IngestSummary::default(),
    )
    .with_grouping(vec![SpendGrouping::Harness, SpendGrouping::Model]);
    let json = spend_json(&report, test_run_id());
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed["grouping"],
        serde_json::json!(["harness", "model"]),
        "the two new dimensions serialize by their dimension names"
    );
    validate_spend_report_json(&json).expect("the harness and model grouping values must validate");
}

#[test]
fn contract_adding_field_to_envelope_without_version_bump_fails() {
    let envelope = JsonEnvelope::new("status", test_run_id(), test_metadata());
    let raw = envelope.to_json();
    let mut parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed
        .as_object_mut()
        .unwrap()
        .insert("unversioned_field".to_string(), serde_json::json!("value"));

    let modified_json = serde_json::to_string(&parsed).unwrap();
    let err = validate_envelope_strict(&modified_json)
        .expect_err("adding an unversioned field must fail strict envelope validation");
    assert_eq!(
        err,
        JsonContractError::UnexpectedField("unversioned_field".to_string())
    );
}

#[test]
fn contract_bumping_version_without_schema_update_fails() {
    let envelope = JsonEnvelope::new("status", test_run_id(), test_metadata());
    let raw = envelope.to_json();
    let mut parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed
        .as_object_mut()
        .unwrap()
        .insert("schema".to_string(), serde_json::json!(SCHEMA_VERSION + 1));

    let modified_json = serde_json::to_string(&parsed).unwrap();
    let err = validate_envelope_strict(&modified_json)
        .expect_err("bumping schema version without contract update must fail validation");
    assert_eq!(
        err,
        JsonContractError::SchemaVersionMismatch {
            expected: SCHEMA_VERSION,
            actual: SCHEMA_VERSION + 1
        }
    );
}

#[test]
fn contract_diagnostic_event_shares_run_id_with_envelope() {
    let timestamp = UtcTimestamp::from_unix_nanos(1_234_567_890);
    let run = RunId::new(timestamp);
    let envelope = JsonEnvelope::new("status", run.clone(), test_metadata());

    let env_json = envelope.to_json();
    let (parsed_env, _) = JsonEnvelope::parse(&env_json).expect("envelope must parse successfully");

    assert_eq!(parsed_env.run.as_str(), run.as_str());
    assert_eq!(
        envelope.run().as_str(),
        run.as_str(),
        "envelope run identifier must match diagnostic run identifier"
    );
}

#[test]
fn sanitization_scan_finds_no_forbidden_patterns_in_presentation_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/presentation");
    assert!(dir.is_dir(), "presentation fixtures directory must exist");
    for entry in std::fs::read_dir(&dir).expect("read_dir presentation fixtures") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
            let hits = matched_patterns(&content);
            assert!(
                hits.is_empty(),
                "fixture {} contains forbidden sensitive patterns: {hits:?}",
                path.display()
            );
        }
    }
}

proptest! {
    #[test]
    fn proptest_quantity_round_trip(
        num in any::<u64>(),
        unit in "[a-z]{1,8}"
    ) {
        let leaked_unit: &'static str = Box::leak(unit.into_boxed_str());
        let val_str = num.to_string();
        let q = Quantity::new(val_str.clone(), leaked_unit);
        let json = q.to_json();
        let parsed = Quantity::from_json(&json).expect("quantity must parse from json");
        prop_assert_eq!(parsed.value(), &val_str);
        prop_assert_eq!(parsed.unit(), leaked_unit);
    }

    #[test]
    fn proptest_interval_exact_round_trip(
        a in any::<u64>(),
        b in any::<u64>()
    ) {
        let lower_val = a.min(b);
        let upper_val = a.max(b);
        let lower = TokenCount::new(lower_val);
        let upper = TokenCount::new(upper_val);
        let interval = Interval::new(lower, upper).unwrap();

        let json = interval_json(&interval);
        let parsed_val: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            parsed_val.get("lower").unwrap().as_str().unwrap(),
            &lower_val.to_string()
        );
        prop_assert_eq!(
            parsed_val.get("upper").unwrap().as_str().unwrap(),
            &upper_val.to_string()
        );
        prop_assert_eq!(
            parsed_val.get("unit").unwrap().as_str().unwrap(),
            "tokens"
        );

        let round_trip: Interval<TokenCount> = interval_from_json(&json).unwrap();
        prop_assert_eq!(round_trip, interval);
    }

    #[test]
    fn proptest_provenance_round_trip(
        sources in prop::collection::vec("[a-zA-Z0-9_-]{1,20}", 0..10)
    ) {
        let provenance = Provenance::new(sources.clone());
        let json = provenance_json(&provenance);
        let round_trip = provenance_from_json(&json).unwrap();
        prop_assert_eq!(round_trip.sources(), provenance.sources());
    }
}

fn per_kind_result_view() -> agent_usage_book::report::CalibratePerKindResultView {
    use agent_usage_book::report::{CalibrateKindCoefficientView, CalibratePerKindResultView};
    let coefficient = |kind: &str, estimate: i64, std_error: i64| CalibrateKindCoefficientView {
        kind_label: kind.to_string(),
        estimate_micro_ppm_per_token: estimate,
        std_error_micro_ppm_per_token: std_error,
        interval_low_micro_ppm_per_token: estimate - 2 * std_error,
        interval_high_micro_ppm_per_token: estimate + 2 * std_error,
    };
    CalibratePerKindResultView {
        calibration_id: "promoted-mvcand-exp-1-00000000000000ab".to_string(),
        candidate_id: "mvcand-exp-1-00000000000000ab".to_string(),
        experiment_id: "exp-1".to_string(),
        provider: "anthropic".to_string(),
        plan_tier: "pro".to_string(),
        window_semantic_key: "five_hour".to_string(),
        coefficients: vec![
            coefficient("input", 672_000, 22_500),
            coefficient("output", 4_484_000, 90_000),
            coefficient("cache_read", -3_000, 2_000),
            coefficient("cache_write", 701_000, 30_000),
        ],
        condition_number_micros: 7_250_000,
        condition_number_threshold_micros: 30_000_000,
        fit_residual_ppm: 2_196,
        held_out_residual_ppm: 3_140,
        validation_observations: 6,
        sample_count: 16,
        statistical_method: "ols-through-origin".to_string(),
        statistical_parameters: "{\"ridge\":0}".to_string(),
        phase_design: "controlled-run=exp-1;kinds=input,output,cache_read,cache_write".to_string(),
        validation_method: "held-out-block-residual".to_string(),
        validation_version: "v1".to_string(),
        inputs_digest_hex: "00000000000000ab".to_string(),
        inputs_count: 40,
        fitting_evidence_digest_hex: "00000000000000cd".to_string(),
        validation_evidence_digest_hex: "00000000000000ef".to_string(),
        fit_timestamp_nanos: 1_500,
        activation_policy_version: "promote-v1".to_string(),
        aub_version: "0.1.0".to_string(),
        source_revision: "abc1234".to_string(),
    }
}

fn assert_matches_fixture(generated_json: &str, fixture: &str) {
    let parsed_generated: serde_json::Value =
        serde_json::from_str(generated_json).expect("generated JSON must parse");
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/presentation")
        .join(fixture);
    let fixture_content = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("fixture {fixture} must exist: {e}"));
    let parsed_fixture: serde_json::Value =
        serde_json::from_str(&fixture_content).expect("fixture must parse as JSON");
    assert_eq!(
        parsed_generated, parsed_fixture,
        "generated JSON must match golden {fixture}"
    );
}

/// `calibrate promote --format json` for a joint candidate: one coefficient
/// per kind with its interval, the condition number and both residuals in
/// their own units, and no credits-per-point `fitted` field.
#[test]
fn contract_calibrate_per_kind_promote_json_matches_golden_fixture() {
    let report = agent_usage_book::report::CalibratePerKindPromoteReport {
        metadata: test_metadata(),
        result: per_kind_result_view(),
    };
    let json = agent_usage_book::presentation::json::calibrate_per_kind_promote_json(
        &report,
        test_run_id(),
    );
    assert_matches_fixture(&json, "calibrate_per_kind_promote_v5.json");
}

/// `calibrate show --format json` with an active per-kind calibration: the
/// scalar `entries` stay empty and the per-kind entry carries its result,
/// health, activation state and lifecycle events.
#[test]
fn contract_calibrate_per_kind_show_json_matches_golden_fixture() {
    let report = agent_usage_book::report::CalibrateShowReport {
        metadata: test_metadata(),
        entries: Vec::new(),
        per_kind_entries: vec![agent_usage_book::report::CalibratePerKindEntry {
            result: per_kind_result_view(),
            health_label: "current".to_string(),
            is_active: true,
            events: vec![agent_usage_book::report::CalibrateLifecycleEventView {
                kind_label: "supersession".to_string(),
                event_at_nanos: 1_900,
                actor: "operator".to_string(),
                activation_policy_version: "promote-v1".to_string(),
                supersedes: Some("promoted-cand-1".to_string()),
            }],
        }],
    };
    let json = agent_usage_book::presentation::json::calibrate_show_json(&report, test_run_id());
    assert_matches_fixture(&json, "calibrate_per_kind_show_v5.json");
}
