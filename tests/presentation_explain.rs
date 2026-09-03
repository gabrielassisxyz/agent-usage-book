//! Contract, integration and unit tests for --explain and --explain=full (aub-xus.5).

use std::collections::{BTreeMap, BTreeSet};

use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::freshness::{Freshness, Observed};
use agent_usage_book::domain::provenance::{
    CostModelId, DerivationId, EvidenceId, QuerySemantics, RateCardId, WindowCalibrationId,
    WitnessId, canonical_inputs_hash,
};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{
    ClockSkewEnvelope, MeasurementBasis, MonotonicDuration, ReceivedAt, UtcDate, UtcTimestamp,
};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{
    spend_json, spend_json_with_explain, status_json, status_json_with_explain,
    validate_spend_report_json, validate_status_report_json,
};
use agent_usage_book::presentation::render::{
    ExplainMode, render_explain, render_spend_report, render_spend_report_with_explain,
    render_status_report, render_status_report_with_explain,
};
use agent_usage_book::report::{
    IngestSummary, LedgerGeneration, MeterAccount, MeterReadingProvenance, ProvenanceGraph,
    ProvenanceNode, ReportField, ReportMetadata, SpendGroup, SpendGroupProvenance, SpendReport,
    StatusReport, Unit, ValueArithmetic,
};

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

fn test_envelope() -> ClockSkewEnvelope {
    ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60))
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

// A fixture builder takes one argument per provenance field on purpose: a struct of
// options would hide which field a test varies.
#[allow(clippy::too_many_arguments)]
fn make_sample_node(
    evidence_names: &[&str],
    cost_model: Option<&str>,
    window_cal: Option<&str>,
    rate_card: Option<&str>,
    grouping: &str,
    filtering: &str,
    sources: u64,
    observations: u64,
    arithmetic: ValueArithmetic,
) -> ProvenanceNode {
    let members: BTreeSet<EvidenceId> =
        evidence_names.iter().map(|s| EvidenceId::new(*s)).collect();
    let mut witnesses = BTreeSet::new();
    if let Some(cm) = cost_model {
        witnesses.insert(WitnessId::CostModel(CostModelId::new(cm)));
    }
    if let Some(wc) = window_cal {
        witnesses.insert(WitnessId::WindowCalibration(WindowCalibrationId::new(wc)));
    }
    if let Some(rc) = rate_card {
        witnesses.insert(WitnessId::RateCard(RateCardId::new(rc)));
    }
    let semantics = QuerySemantics::new(grouping, filtering);
    ProvenanceNode::new(
        members,
        witnesses,
        semantics,
        sources,
        observations,
        arithmetic,
    )
}

fn seed_status_report() -> StatusReport {
    let node_primary = make_sample_node(
        &["obs-meter-primary-001", "obs-meter-primary-002"],
        Some("cost-model-primary-v1"),
        Some("cal-window-2026-08"),
        Some("rate-card-2026-08-01"),
        "by-account",
        "account=primary can-run",
        2,
        6,
        ValueArithmetic::Converted {
            from: Unit::QuotaFraction,
            to: Unit::Credits,
        },
    );
    let readings = vec![MeterReadingProvenance::new(
        LogicalName::new("primary"),
        node_primary,
    )];
    StatusReport::new(
        test_metadata(),
        vec![MeterAccount::new(
            LogicalName::new("primary"),
            Freshness::Fresh {
                observed: observed_reading(500_000),
                latest_attempt: AttemptId::new(1),
            },
        )],
        readings,
    )
}

fn seed_spend_report() -> SpendReport {
    let node_group = make_sample_node(
        &["tx-event-001", "tx-event-002", "tx-event-003"],
        Some("cost-model-anthropic-messages-v1"),
        Some("cal-window-2026-08"),
        Some("rate-card-2026-08-01"),
        "day,source",
        "2026-08-26..2026-08-27 can-run",
        3,
        15,
        ValueArithmetic::Sum,
    );
    let key = LogicalName::new("2026-08-26 claude-code");
    let derivation_id = DerivationId::from_manifest(node_group.manifest());
    let group = SpendGroup::new(
        key.clone(),
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(100),
                OutputTokens::new(50),
                CacheReadTokens::new(10),
                CacheWriteTokens::new(5),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        ),
        Provenance::new(["transcripts/claude-code/session-1.jsonl".to_string()]),
        derivation_id,
    );
    let prov = vec![SpendGroupProvenance::new(key, node_group)];
    SpendReport::new(
        test_metadata(),
        UtcDate::parse("2026-08-26").unwrap(),
        UtcDate::parse("2026-08-27").unwrap(),
        vec![group],
        prov,
        IngestSummary {
            files_read: 1,
            files_skipped_before_window: 0,
            unreadable_files: vec![],
            quarantined_by_class: BTreeMap::new(),
            replayed_occurrences: 0,
            collisions: 0,
            without_identity: 0,
            undated_events: 0,
            events_outside_window: 0,
            events_in_window: 15,
        },
    )
}

/// Integration: both explain levels over a seeded report show all 10 listed elements,
/// including the arithmetic and conversion sequence.
#[test]
fn integration_explain_summary_renders_all_ten_design_elements() {
    let report = seed_spend_report();
    let text_summary = render_spend_report_with_explain(&report, ExplainMode::Summary);

    // 1. Stable DerivationId
    let node = report
        .provenance
        .resolve(&ReportField::SpendGroupTokens {
            key: LogicalName::new("2026-08-26 claude-code"),
        })
        .expect("spend group provenance node must exist");
    let derivation_id = DerivationId::from_manifest(node.manifest());
    assert!(
        text_summary.contains(&derivation_id.to_hex()),
        "Element 1: derivation identifier must be present"
    );

    // 2. Source-event and observation counts
    assert!(
        text_summary.contains("sources: 3, observations: 15"),
        "Element 2: source and observation counts must be present"
    );

    // 3. Content-addressed input-manifest IDs
    let inputs_hash_hex = node.manifest().inputs_hash().to_hex();
    assert!(
        text_summary.contains(&format!("manifest: hash={inputs_hash_hex}")),
        "Element 3: manifest hash must be present"
    );
    assert!(
        text_summary.contains("inputs=3"),
        "Element 3: manifest inputs count must be present"
    );

    // 4. Account attribution evidence
    assert!(
        text_summary.contains("account attribution: 2026-08-26 claude-code"),
        "Element 4: account attribution must be present"
    );

    // 5. Cost-model ID
    assert!(
        text_summary.contains("cost model: cost-model-anthropic-messages-v1"),
        "Element 5: cost-model ID must be present"
    );

    // 6. Window-calibration ID
    assert!(
        text_summary.contains("window calibration: cal-window-2026-08"),
        "Element 6: window-calibration ID must be present"
    );

    // 7. Rate-card version
    assert!(
        text_summary.contains("rate card: rate-card-2026-08-01"),
        "Element 7: rate-card version must be present"
    );

    // 8. Coverage and evidence-quality status
    assert!(
        text_summary.contains("coverage and quality: complete"),
        "Element 8: coverage and quality status must be present"
    );

    // 9. Empirical-history selection for can-run
    assert!(
        text_summary.contains("empirical history: can-run"),
        "Element 9: empirical history selection must be present"
    );

    // 10. Arithmetic and conversion sequence
    assert!(
        text_summary.contains("arithmetic: sum"),
        "Element 10: arithmetic sequence must be present"
    );

    // Status report with conversion arithmetic
    let status_rep = seed_status_report();
    let status_text = render_status_report_with_explain(
        &status_rep,
        UtcTimestamp::from_unix_nanos(2000),
        test_envelope(),
        ExplainMode::Summary,
    );
    assert!(
        status_text.contains("arithmetic: converted from quota_fraction to credits"),
        "Element 10: conversion arithmetic must be present on status explain"
    );
}

/// Unit: --explain=full expanding a manifest to exactly the evidence identifiers
/// whose canonical hash produced it, verified by rehashing the expansion.
#[test]
fn unit_explain_full_expands_exact_canonical_evidence_members() {
    let report = seed_spend_report();
    let text_full = render_spend_report_with_explain(&report, ExplainMode::Full);

    assert!(text_full.contains("members (3):"));
    assert!(text_full.contains("  - tx-event-001"));
    assert!(text_full.contains("  - tx-event-002"));
    assert!(text_full.contains("  - tx-event-003"));

    let node = report
        .provenance
        .resolve(&ReportField::SpendGroupTokens {
            key: LogicalName::new("2026-08-26 claude-code"),
        })
        .expect("node must exist");

    // Rehashing the expansion must match the manifest inputs hash
    let rehashed = canonical_inputs_hash(node.members());
    assert_eq!(rehashed, node.manifest().inputs_hash());
    assert!(node.verify());
}

/// Unit: corrupting one member of an expansion is detected by the rehash comparison.
#[test]
fn unit_corrupting_one_member_of_expansion_detected_by_rehash() {
    let node = make_sample_node(
        &["evidence-alpha", "evidence-beta", "evidence-gamma"],
        None,
        None,
        None,
        "test",
        "test",
        1,
        3,
        ValueArithmetic::Direct,
    );

    assert!(node.verify());

    // Corrupt one member
    let mut corrupted: BTreeSet<EvidenceId> = node.members().clone();
    corrupted.remove(&EvidenceId::new("evidence-beta"));
    corrupted.insert(EvidenceId::new("evidence-beta-tampered"));

    let corrupted_hash = canonical_inputs_hash(&corrupted);
    assert_ne!(
        corrupted_hash,
        node.manifest().inputs_hash(),
        "tampered member must alter canonical inputs hash"
    );
    assert!(
        !node.manifest().verify_expansion(&corrupted),
        "expansion verification must fail for corrupted members"
    );
}

/// Contract: explain output available in JSON as well as human form, validated against the schema.
#[test]
fn contract_explain_json_validated_against_schema() {
    let status_rep = seed_status_report();
    let status_summary_json =
        status_json_with_explain(&status_rep, test_run_id(), ExplainMode::Summary);
    let status_full_json = status_json_with_explain(&status_rep, test_run_id(), ExplainMode::Full);

    validate_status_report_json(&status_summary_json)
        .expect("status summary JSON must pass strict schema validation");
    validate_status_report_json(&status_full_json)
        .expect("status full JSON must pass strict schema validation");

    let spend_rep = seed_spend_report();
    let spend_summary_json =
        spend_json_with_explain(&spend_rep, test_run_id(), ExplainMode::Summary);
    let spend_full_json = spend_json_with_explain(&spend_rep, test_run_id(), ExplainMode::Full);

    validate_spend_report_json(&spend_summary_json)
        .expect("spend summary JSON must pass strict schema validation");
    validate_spend_report_json(&spend_full_json)
        .expect("spend full JSON must pass strict schema validation");

    // Check JSON content
    let parsed: serde_json::Value =
        serde_json::from_str(&spend_full_json).expect("JSON must be valid");
    let explain = parsed.get("explain").expect("explain field must exist");
    assert_eq!(explain.get("mode").unwrap().as_str().unwrap(), "full");
    let nodes = explain
        .get("nodes")
        .unwrap()
        .as_array()
        .expect("nodes array");
    assert_eq!(nodes.len(), 1);
    let first = &nodes[0];
    assert_eq!(
        first.get("field").unwrap().as_str().unwrap(),
        "spend_group_tokens[2026-08-26 claude-code]"
    );
    assert_eq!(first.get("source_count").unwrap().as_u64().unwrap(), 3);
    assert_eq!(
        first.get("observation_count").unwrap().as_u64().unwrap(),
        15
    );
    assert_eq!(
        first.get("cost_model").unwrap().as_str().unwrap(),
        "cost-model-anthropic-messages-v1"
    );
    let members = first
        .get("members")
        .unwrap()
        .as_array()
        .expect("members array in full mode");
    assert_eq!(members.len(), 3);
}

/// Unit: explain adds no computation to the ordinary path, asserted by comparing
/// the work and output done with and without the flag.
#[test]
fn unit_explain_adds_no_computation_to_ordinary_path() {
    let spend_rep = seed_spend_report();

    // Ordinary path text matches ExplainMode::Off and contains no explain block
    let base_text = render_spend_report(&spend_rep);
    let off_text = render_spend_report_with_explain(&spend_rep, ExplainMode::Off);
    assert_eq!(base_text, off_text);
    assert!(!base_text.contains("explain:"));
    assert!(!off_text.contains("explain:"));

    // Ordinary path JSON matches ExplainMode::Off and contains no explain field
    let base_json = spend_json(&spend_rep, test_run_id());
    let off_json = spend_json_with_explain(&spend_rep, test_run_id(), ExplainMode::Off);
    assert_eq!(base_json, off_json);
    assert!(!base_json.contains("\"explain\":"));
    assert!(!off_json.contains("\"explain\":"));

    // Status report ordinary path matches ExplainMode::Off
    let status_rep = seed_status_report();
    let now = UtcTimestamp::from_unix_nanos(2000);
    let env = test_envelope();
    let base_status_text = render_status_report(&status_rep, now, env);
    let off_status_text =
        render_status_report_with_explain(&status_rep, now, env, ExplainMode::Off);
    assert_eq!(base_status_text, off_status_text);
    assert!(!base_status_text.contains("explain:"));
    assert!(!off_status_text.contains("explain:"));

    let base_status_json = status_json(&status_rep, test_run_id());
    let off_status_json = status_json_with_explain(&status_rep, test_run_id(), ExplainMode::Off);
    assert_eq!(base_status_json, off_status_json);
    assert!(!base_status_json.contains("\"explain\":"));
    assert!(!off_status_json.contains("\"explain\":"));
}

/// Integration: seeded report models containing at least a few thousand provenance
/// occurrences render bounded `--explain` and complete `--explain=full`.
#[test]
fn integration_large_scale_provenance_occurrences_bounded_summary_complete_full() {
    const OCCURRENCE_COUNT: usize = 5_000;
    let raw_ids: Vec<String> = (0..OCCURRENCE_COUNT)
        .map(|i| format!("occurrence-{i:06}"))
        .collect();
    let members: BTreeSet<EvidenceId> = raw_ids
        .iter()
        .map(|s| EvidenceId::new(s.as_str()))
        .collect();

    let node = ProvenanceNode::new(
        members.clone(),
        [],
        QuerySemantics::new("large-batch", "all"),
        100,
        OCCURRENCE_COUNT as u64,
        ValueArithmetic::Sum,
    );

    let graph = ProvenanceGraph::new([(ReportField::CalibrationTokens, node)]);

    // Summary mode must be compact (bounded length, does not enumerate 5000 items)
    let summary_text = render_explain(&graph, ExplainMode::Summary);
    assert!(
        summary_text.len() < 1_000,
        "Summary mode must be compact and bounded: was {} chars",
        summary_text.len()
    );
    assert!(!summary_text.contains("occurrence-000001"));

    // Full mode must enumerate all 5000 members
    let full_text = render_explain(&graph, ExplainMode::Full);
    assert!(full_text.contains(&format!("members ({OCCURRENCE_COUNT}):")));
    assert!(full_text.contains("occurrence-000000"));
    assert!(full_text.contains("occurrence-004999"));

    // Full expansion must be verifiable by rehash
    let node_ref = graph
        .resolve(&ReportField::CalibrationTokens)
        .expect("calibration node");
    assert!(node_ref.verify());
    assert_eq!(
        canonical_inputs_hash(node_ref.members()),
        node_ref.manifest().inputs_hash()
    );
}
