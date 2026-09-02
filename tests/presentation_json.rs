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
    IngestSummary, LedgerGeneration, MeterAccount, ReportMetadata, SpendGroup, SpendReport,
    StatusReport,
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
    );

    let generated_json = status_json(&report, test_run_id());
    let parsed_generated: serde_json::Value =
        serde_json::from_str(&generated_json).expect("generated status JSON must parse");

    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/presentation/status_v1.json");
    let fixture_content =
        std::fs::read_to_string(&fixture_path).expect("fixture status_v1.json must exist");
    let parsed_fixture: serde_json::Value =
        serde_json::from_str(&fixture_content).expect("fixture must parse as JSON");

    assert_eq!(
        parsed_generated, parsed_fixture,
        "generated status JSON must match golden status_v1.json fixture"
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
        files_read: 1,
        files_skipped_before_window: 0,
        unreadable_files: vec![],
        quarantined_by_class: BTreeMap::new(),
        replayed_occurrences: 0,
        collisions: 0,
        without_identity: 0,
        undated_events: 0,
        events_outside_window: 0,
        events_in_window: 1,
    };
    let report = SpendReport::new(test_metadata(), since, until, groups, vec![], ingest);

    let generated_json = spend_json(&report, test_run_id());
    let parsed_generated: serde_json::Value =
        serde_json::from_str(&generated_json).expect("generated spend JSON must parse");

    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/presentation/spend_v1.json");
    let fixture_content =
        std::fs::read_to_string(&fixture_path).expect("fixture spend_v1.json must exist");
    let parsed_fixture: serde_json::Value =
        serde_json::from_str(&fixture_content).expect("fixture must parse as JSON");

    assert_eq!(
        parsed_generated, parsed_fixture,
        "generated spend JSON must match golden spend_v1.json fixture"
    );

    let parsed_env = validate_spend_report_json(&generated_json)
        .expect("spend JSON must strictly validate against contract");
    assert_eq!(parsed_env.schema, SCHEMA_VERSION);
    assert_eq!(parsed_env.command, "spend");
    assert_eq!(parsed_env.run.as_str(), "run-1000-2000-1");
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
        .insert("schema".to_string(), serde_json::json!(2));

    let modified_json = serde_json::to_string(&parsed).unwrap();
    let err = validate_envelope_strict(&modified_json)
        .expect_err("bumping schema version without contract update must fail validation");
    assert_eq!(
        err,
        JsonContractError::SchemaVersionMismatch {
            expected: 1,
            actual: 2
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
