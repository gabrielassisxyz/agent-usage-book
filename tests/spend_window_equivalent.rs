//! Contract coverage for calibrated window-equivalent spend output.

use std::collections::BTreeSet;

use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::interval::Interval;
use agent_usage_book::domain::provenance::{
    CostModelId, DerivationId, EvidenceId, QuerySemantics, WindowCalibrationId, WitnessId,
};
use agent_usage_book::domain::quota::PercentagePoints;
use agent_usage_book::domain::time::{UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{
    CoverageCompleteness, Derivation, EstimatorId, EvidenceQuality, Provenance, Qualified,
};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{spend_json_with_explain, validate_spend_report_json};
use agent_usage_book::presentation::render::{ExplainMode, render_spend_report_with_explain};
use agent_usage_book::report::{
    IngestSummary, LedgerGeneration, ProvenanceNode, ReportMetadata, SpendGroup,
    SpendGroupCreditsProvenance, SpendGroupProvenance, SpendGroupWindowEquivalentProvenance,
    SpendGrouping, SpendReport, Unit, ValueArithmetic, WindowEquivalentDerivation,
    WindowEquivalentValue,
};

fn node(arithmetic: ValueArithmetic) -> ProvenanceNode {
    ProvenanceNode::new(
        [EvidenceId::new("event-window-equivalent-1")],
        [
            WitnessId::CostModel(CostModelId::new("cost-model-v1")),
            WitnessId::WindowCalibration(WindowCalibrationId::new("calibration-v1")),
        ],
        QuerySemantics::new("day", "2026-08-25..2026-08-26"),
        1,
        1,
        arithmetic,
    )
}

fn report() -> SpendReport {
    let key = LogicalName::new("day=2026-08-25");
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(100_000),
            OutputTokens::new(20_000),
            CacheReadTokens::new(50_000),
            CacheWriteTokens::new(10_000),
        ),
        Default::default(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    );
    let credits = Derivation::Available(Qualified::new(
        Credits::from_micros(652_500),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
        Provenance::new(["cost-model:cost-model-v1".to_string()]),
    ));
    let interval = Interval::new(
        PercentagePoints::new(100).unwrap(),
        PercentagePoints::new(250).unwrap(),
    )
    .unwrap();
    let window_equivalent = WindowEquivalentDerivation::Available(WindowEquivalentValue {
        interval,
        calibration_id: WindowCalibrationId::new("calibration-v1"),
        coverage: CoverageCompleteness::Complete,
        quality: EvidenceQuality::Estimated {
            methods: BTreeSet::from([EstimatorId::new("window-calibration:calibration-v1")]),
            uncertainty: Some(interval),
        },
        provenance: Provenance::new(["window-calibration:calibration-v1".to_string()]),
    });
    let group = SpendGroup::new(
        key.clone(),
        usage,
        Provenance::new(["transcripts/claude-code/session.jsonl".to_string()]),
        DerivationId::from_manifest(node(ValueArithmetic::Sum).manifest()),
    )
    .with_credits(credits)
    .with_window_equivalent(window_equivalent);
    let metadata = ReportMetadata::new(
        UtcTimestamp::from_unix_nanos(2_000),
        UtcTimestamp::from_unix_nanos(1_000),
        LedgerGeneration::new(7),
        None,
    );
    SpendReport::new(
        metadata,
        UtcDate::parse("2026-08-25").unwrap(),
        UtcDate::parse("2026-08-26").unwrap(),
        vec![group],
        vec![SpendGroupProvenance::new(
            key.clone(),
            node(ValueArithmetic::Sum),
        )],
        IngestSummary::default(),
    )
    .with_grouping(vec![SpendGrouping::Day])
    .with_credit_model(Some(CostModelId::new("cost-model-v1")))
    .with_window_equivalent_window(Some("five_hour".to_string()))
    .with_credit_provenance(vec![SpendGroupCreditsProvenance::new(
        key.clone(),
        node(ValueArithmetic::Converted {
            from: Unit::Tokens,
            to: Unit::Credits,
        }),
    )])
    .with_window_equivalent_provenance(vec![SpendGroupWindowEquivalentProvenance::new(
        key,
        node(ValueArithmetic::Converted {
            from: Unit::Credits,
            to: Unit::PercentagePoints,
        }),
    )])
}

#[test]
fn human_and_json_window_equivalent_contracts_retain_the_same_interval_and_witness() {
    let report = report();
    let human = render_spend_report_with_explain(&report, ExplainMode::Summary);
    assert!(human.contains("converted to window-equivalent percentage points for five_hour"));
    assert!(
        human.contains("0.65 credits"),
        "credit dimension must remain: {human}"
    );
    assert!(
        human.contains("input 100000 tokens"),
        "token dimension must remain: {human}"
    );
    assert!(
        human.contains("window equivalent [0.0100, 0.0250] percentage points")
            && human.contains("calibration calibration-v1"),
        "human interval and calibration must be visible: {human}"
    );
    assert!(
        human.contains("window calibration: calibration-v1"),
        "explanation must name the calibration witness: {human}"
    );

    let json = spend_json_with_explain(
        &report,
        RunId::from_string("run-window-equivalent".to_string()),
        ExplainMode::Summary,
    );
    validate_spend_report_json(&json).expect("window-equivalent JSON must validate");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let window = &parsed["groups"][0]["window_equivalent"];
    assert_eq!(window["lower"], "100");
    assert_eq!(window["upper"], "250");
    assert_eq!(window["unit"], "percentage_points");
    assert_eq!(window["calibration_id"], "calibration-v1");
    assert_eq!(window["coverage"], "complete");
    assert_eq!(window["evidence_quality"], "estimated");
    assert_eq!(parsed["groups"][0]["credits"]["unit"], "credits");
    assert_eq!(parsed["groups"][0]["tokens"]["input"]["unit"], "tokens");
    assert!(
        window["provenance"]
            .to_string()
            .contains("window-calibration:calibration-v1")
    );
}
