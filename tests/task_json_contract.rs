//! The `aub task` JSON contract tests (`aub-eu7.4`): each of the three
//! subcommands' documents validates against its own versioned envelope.

use agent_usage_book::attribution::{TaskIdentityState, TaskKind, TaskKindOrigin};
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::ids::TaskId as DomainTaskId;
use agent_usage_book::domain::ids::{NativeRunId, SourceNamespace};
use agent_usage_book::domain::provenance::QuerySemantics;
use agent_usage_book::domain::time::UtcTimestamp;
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{
    CoverageCompleteness, Derivation, EvidenceQuality, Provenance, RequiredFact,
};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{
    task_ingest_json, task_overhead_json_with_explain, task_report_json_with_explain,
    validate_task_ingest_json, validate_task_overhead_json, validate_task_report_json,
};
use agent_usage_book::presentation::render::ExplainMode;
use agent_usage_book::report::provenance::{ProvenanceNode, ValueArithmetic};
use agent_usage_book::report::{
    IngestionGeneration, LedgerGeneration, ReportMetadata, SharePpm, TaskIdentityRow,
    TaskIngestReport, TaskOverheadBucket, TaskOverheadReport, TaskReport, TaskSessionUsage,
};

fn run() -> RunId {
    RunId::new(UtcTimestamp::from_unix_nanos(2_000))
}

fn metadata() -> ReportMetadata {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    ReportMetadata::new(
        now,
        now,
        LedgerGeneration::new(9),
        Some(IngestionGeneration::new(4)),
    )
}

fn usage(input: u64) -> UsageVector {
    UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        ),
        Default::default(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    )
}

fn node() -> ProvenanceNode {
    ProvenanceNode::new(
        [] as [agent_usage_book::domain::provenance::EvidenceId; 0],
        [] as [agent_usage_book::domain::provenance::WitnessId; 0],
        QuerySemantics::new("task", "beads-a:aub-1"),
        1,
        1,
        ValueArithmetic::Sum,
    )
}

/// A resolved task-kind identity, an unresolved one, and the absent-evidence
/// case all serialize distinctly and all validate: `task_kind` must not
/// collapse "the tracker never mentioned this task" (`None`) with "the
/// tracker's evidence was ambiguous" (`Conflict`).
#[test]
fn every_task_kind_state_serializes_distinctly_and_validates() {
    let cases: Vec<(Option<TaskIdentityRow>, &str)> = vec![
        (None, "null"),
        (
            Some(TaskIdentityRow {
                task_id: DomainTaskId::new(
                    SourceNamespace::new("beads-a"),
                    agent_usage_book::domain::ids::NativeTaskId::new("aub-1"),
                ),
                state: TaskIdentityState::Resolved,
                kind: Some(TaskKind::Bug),
                winner: Some(TaskKindOrigin::TrackerField("issue_type".to_string())),
                evidence: "tracker_field:issue_type=bug".to_string(),
                normalization_version: 1,
                size_state: TaskIdentityState::Unknown,
                size: None,
                size_evidence: String::new(),
                difficulty_state: TaskIdentityState::Unknown,
                difficulty: None,
                difficulty_evidence: String::new(),
            }),
            "resolved",
        ),
        (
            Some(TaskIdentityRow {
                task_id: DomainTaskId::new(
                    SourceNamespace::new("beads-a"),
                    agent_usage_book::domain::ids::NativeTaskId::new("aub-2"),
                ),
                state: TaskIdentityState::Conflict,
                kind: None,
                winner: None,
                evidence: "tracker_label:alpha=alpha;tracker_label:beta=beta".to_string(),
                normalization_version: 1,
                size_state: TaskIdentityState::Unknown,
                size: None,
                size_evidence: String::new(),
                difficulty_state: TaskIdentityState::Unknown,
                difficulty: None,
                difficulty_evidence: String::new(),
            }),
            "conflict",
        ),
    ];

    for (task_kind, expected_state) in cases {
        let report = TaskReport::new(
            metadata(),
            LogicalName::new("beads-a:aub-1"),
            task_kind,
            usage(7),
            Derivation::unavailable(
                [RequiredFact::new("active cost model")],
                Provenance::new(["cost-model:unavailable".to_string()]),
            )
            .unwrap(),
            vec![TaskSessionUsage {
                session: LogicalName::new("fixture:s1"),
                run: Some(NativeRunId::new("run-1")),
                usage: usage(7),
            }],
            node(),
            node(),
        );
        let document = task_report_json_with_explain(&report, run(), ExplainMode::Off);
        validate_task_report_json(&document).expect("the task report document must validate");

        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        assert_eq!(parsed["command"], "task-report");
        assert_eq!(parsed["task_id"], "beads-a:aub-1");
        assert_eq!(parsed["sessions"][0]["session"], "fixture:s1");
        assert_eq!(parsed["sessions"][0]["run"], "run-1");
        let state = if task_kind_is_none(&parsed) {
            "null".to_string()
        } else {
            parsed["task_kind"]["state"].as_str().unwrap().to_string()
        };
        assert_eq!(state, expected_state);
    }
}

fn task_kind_is_none(parsed: &serde_json::Value) -> bool {
    parsed["task_kind"].is_null()
}

/// A credit derivation that is available serializes and validates exactly
/// like an unavailable one: "where a complete cost model exists" still
/// produces one document either way.
#[test]
fn an_available_credit_derivation_validates_too() {
    let credits = Derivation::Available(agent_usage_book::evidence::Qualified::new(
        Credits::from_micros(1_500_000),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
        Provenance::new(["cost-model:active".to_string()]),
    ));
    let report = TaskReport::new(
        metadata(),
        LogicalName::new("beads-a:aub-1"),
        None,
        usage(7),
        credits,
        vec![],
        node(),
        node(),
    );
    let document = task_report_json_with_explain(&report, run(), ExplainMode::Off);
    validate_task_report_json(&document).expect("an available credit derivation must validate");
    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    assert!(parsed["credits"].get("status").is_none());
}

/// `--explain` attaches the provenance block and the document still
/// validates; the planted negative is the same report without `--explain`,
/// where the key is absent rather than present-and-empty.
#[test]
fn task_report_explain_attaches_and_still_validates() {
    let report = TaskReport::new(
        metadata(),
        LogicalName::new("beads-a:aub-1"),
        None,
        usage(7),
        Derivation::unavailable(
            [RequiredFact::new("active cost model")],
            Provenance::new(["cost-model:unavailable".to_string()]),
        )
        .unwrap(),
        vec![],
        node(),
        node(),
    );
    let off = task_report_json_with_explain(&report, run(), ExplainMode::Off);
    assert!(!serde_json::from_str::<serde_json::Value>(&off).unwrap()["explain"].is_object());
    let summary = task_report_json_with_explain(&report, run(), ExplainMode::Summary);
    validate_task_report_json(&summary).expect("the explain document must still validate");
    let parsed: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert!(parsed["explain"].is_object());
}

/// `aub task overhead`'s document names its window and validates, with every
/// bucket carrying its magnitude and share.
#[test]
fn task_overhead_document_validates_with_buckets_and_shares() {
    let bucket = TaskOverheadBucket {
        reason: LogicalName::new("before_first_claim"),
        usage: usage(3),
        share: SharePpm::of(3, 3),
    };
    let report = TaskOverheadReport::new(
        metadata(),
        agent_usage_book::domain::time::UtcDate::parse("2026-08-25").unwrap(),
        agent_usage_book::domain::time::UtcDate::parse("2026-08-26").unwrap(),
        usage(7),
        node(),
        vec![bucket],
        vec![(LogicalName::new("before_first_claim"), node())],
    );
    let document = task_overhead_json_with_explain(&report, run(), ExplainMode::Off);
    validate_task_overhead_json(&document).expect("the task overhead document must validate");

    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    assert_eq!(parsed["command"], "task-overhead");
    assert_eq!(parsed["window"]["since"], "2026-08-25");
    assert_eq!(parsed["buckets"][0]["reason"], "before_first_claim");
    assert_eq!(parsed["buckets"][0]["share"]["value"], "1000000");
    assert_eq!(parsed["buckets"][0]["share"]["unit"], "ppm");
}

/// An unexpected key at the root is refused: the strict-key contract catches
/// a shape change that was not accompanied by a validator update, the same
/// property `validate_spend_report_json` enforces.
#[test]
fn an_unexpected_key_is_refused() {
    let report = TaskOverheadReport::new(
        metadata(),
        agent_usage_book::domain::time::UtcDate::parse("2026-08-25").unwrap(),
        agent_usage_book::domain::time::UtcDate::parse("2026-08-26").unwrap(),
        usage(0),
        node(),
        vec![],
        vec![],
    );
    let document = task_overhead_json_with_explain(&report, run(), ExplainMode::Off);
    let mut value: serde_json::Value = serde_json::from_str(&document).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("unexpected_field".to_string(), serde_json::json!(true));
    let corrupted = serde_json::to_string(&value).unwrap();
    assert!(validate_task_overhead_json(&corrupted).is_err());
}

/// `aub task ingest`'s document reports the same four counts the text
/// rendering does, and validates.
#[test]
fn task_ingest_document_validates() {
    let summary = TaskIngestReport {
        events_inserted: 3,
        events_already_present: 1,
        quarantines_inserted: 1,
        quarantines_already_present: 0,
    };
    let document = task_ingest_json(&summary, run(), metadata());
    validate_task_ingest_json(&document).expect("the task ingest document must validate");
    let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
    assert_eq!(parsed["command"], "task-ingest");
    assert_eq!(parsed["events_inserted"], 3);
    assert_eq!(parsed["events_already_present"], 1);
    assert_eq!(parsed["quarantines_inserted"], 1);
    assert_eq!(parsed["quarantines_already_present"], 0);
}
