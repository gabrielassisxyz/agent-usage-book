//! Provenance graph tests (aub-xus.6, PLAN.md 3.6 and 34.28).
//!
//! For every seeded end-to-end command, every physical quantity field the report
//! model exposes must resolve to a provenance node, matched by typed
//! [`ReportField`] identifier rather than by scanning rendered output for a
//! digit string. A digit search produces false positives on timestamps and
//! counts, and false negatives whenever formatting rounds a value; matching
//! typed identifiers against typed graph keys produces neither.
//!
//! The "seeded end-to-end command" cases below build report models the same
//! way `tests/presentation_explain.rs` seeds them: realistic typed material
//! fed through each report model's own constructor, one per command that
//! carries a physical quantity. `sample`, `ingest`, `backup` and `doctor` are
//! deliberately absent from the registry: their report models carry no
//! quantitative field and construct an empty [`ProvenanceGraph`] by design
//! (`src/report/models.rs`), so they have nothing for this suite to check.
//!
//! Every expected field is derived from the seeded report model's own account,
//! group, task or bucket data, never from the provenance graph the same
//! constructor produced. Deriving the expectation from the graph itself would
//! make every check pass trivially, because a field the constructor forgot to
//! register would then also be a field nobody expects to find.
//!
//! The manifest hash and expansion law are inherited from `aub-rif.11`'s
//! [`ProvenanceManifest`] through [`ProvenanceNode::verify`] and
//! [`canonical_inputs_hash`]; nothing here recomputes or restates them.

use std::collections::BTreeSet;

use agent_usage_book::config::CoverageFloor;
use agent_usage_book::coverage::CoverageReport as EngineCoverageReport;
use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::credits::Credits;
use agent_usage_book::domain::freshness::{Freshness, Observed};
use agent_usage_book::domain::interval::Interval;
use agent_usage_book::domain::provenance::{
    CostModelId, DerivationId, EvidenceId, ProvenanceManifest, QuerySemantics, WindowCalibrationId,
    WitnessId, canonical_inputs_hash,
};
use agent_usage_book::domain::quota::{PercentagePoints, QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{MeasurementBasis, ReceivedAt, UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};
use agent_usage_book::evidence::{
    CoverageCompleteness, Derivation, EvidenceQuality, Provenance, Qualified,
};
use agent_usage_book::logging::LogicalName;
use agent_usage_book::report::coverage::CoverageFailureTally;
use agent_usage_book::report::{
    CalibrateReport, CoverageAccount, CoverageReport, CoverageThreshold, ExportReport,
    IngestSummary, LedgerGeneration, MeterAccount, MeterReadingProvenance, NowReport,
    ProjectionReadState, ProvenanceGraph, ProvenanceNode, ReportField, ReportMetadata, SharePpm,
    SpendDiagnostic, SpendDiagnosticProvenance, SpendGroup, SpendGroupCreditsProvenance,
    SpendGroupProvenance, SpendGroupWindowEquivalentProvenance, SpendReport, StatusReport,
    TaskOverheadBucket, TaskOverheadReport, TaskReport, Unit, ValueArithmetic,
    WindowEquivalentDerivation, WindowEquivalentValue,
};
use agent_usage_book::store::export::ExportKey;

// --- shared fixture material -------------------------------------------------

fn test_metadata() -> ReportMetadata {
    ReportMetadata::new(
        UtcTimestamp::from_unix_nanos(2_000),
        UtcTimestamp::from_unix_nanos(1_000),
        LedgerGeneration::new(7),
        None,
    )
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

fn usage_vector(input: u64, output: u64) -> UsageVector {
    UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        ),
        std::collections::BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    )
}

/// A provenance node from evidence names and an optional cost-model witness.
/// Kept to the material each seeded case actually varies: one witness kind is
/// enough to prove a node is real, and the manifest-verification tests below
/// exercise the hash law rather than the witness set.
fn node(
    evidence: &[&str],
    cost_model: Option<&str>,
    grouping: &str,
    filtering: &str,
    sources: u64,
    observations: u64,
    arithmetic: ValueArithmetic,
) -> ProvenanceNode {
    let members: BTreeSet<EvidenceId> = evidence.iter().map(|s| EvidenceId::new(*s)).collect();
    let witnesses: BTreeSet<WitnessId> = cost_model
        .map(|id| WitnessId::CostModel(CostModelId::new(id)))
        .into_iter()
        .collect();
    ProvenanceNode::new(
        members,
        witnesses,
        QuerySemantics::new(grouping, filtering),
        sources,
        observations,
        arithmetic,
    )
}

// --- the registry -------------------------------------------------------------

/// One seeded end-to-end command: its name, the provenance graph its report
/// model produced, and the physical-quantity fields that model's own data says
/// it should carry.
struct SeededCommand {
    name: &'static str,
    graph: ProvenanceGraph,
    expected: Vec<ReportField>,
}

/// The [`ReportField`] a meter reading account resolves to, derived from the
/// account list a `status` or `now` report actually carries.
fn accounts_expected_fields(accounts: &[MeterAccount]) -> Vec<ReportField> {
    accounts
        .iter()
        .map(|account| ReportField::MeterQuotaRemaining {
            account: account.account.clone(),
        })
        .collect()
}

/// Every spend field a report's own group tree says it should carry: one
/// token field per group (recursively, through every nested child), plus a
/// credits or window-equivalent field wherever a group's own data carries
/// that derivation, plus the three report-level diagnostics every spend
/// report attaches.
fn spend_expected_fields(report: &SpendReport) -> Vec<ReportField> {
    fn walk(groups: &[SpendGroup], out: &mut Vec<ReportField>) {
        for group in groups {
            out.push(ReportField::SpendGroupTokens {
                key: group.key.clone(),
            });
            if group.credits.is_some() {
                out.push(ReportField::SpendGroupCredits {
                    key: group.key.clone(),
                });
            }
            if group.window_equivalent.is_some() {
                out.push(ReportField::SpendGroupWindowEquivalent {
                    key: group.key.clone(),
                });
            }
            walk(&group.children, out);
        }
    }
    let mut fields = vec![
        ReportField::SpendCanonicalRecords,
        ReportField::SpendReplayedOccurrences,
        ReportField::SpendHeuristicIdentities,
    ];
    walk(&report.groups, &mut fields);
    fields
}

/// The coverage field per account a coverage report's own account list says it
/// should carry.
fn coverage_expected_fields(report: &CoverageReport) -> Vec<ReportField> {
    report
        .accounts
        .iter()
        .map(|account| ReportField::Coverage {
            account: account.name.clone(),
        })
        .collect()
}

/// The usage and credits fields a task report's own task id says it should
/// carry.
fn task_expected_fields(report: &TaskReport) -> Vec<ReportField> {
    vec![
        ReportField::TaskUsage {
            task_id: report.task_id.clone(),
        },
        ReportField::TaskCredits {
            task_id: report.task_id.clone(),
        },
    ]
}

/// The task-attributed usage field, plus one bucket field per overhead bucket
/// a task-overhead report's own bucket list says it should carry.
fn task_overhead_expected_fields(report: &TaskOverheadReport) -> Vec<ReportField> {
    let mut fields = vec![ReportField::TaskOverheadTaskUsage];
    fields.extend(
        report
            .buckets
            .iter()
            .map(|bucket| ReportField::TaskOverheadBucket {
                reason: bucket.reason.clone(),
            }),
    );
    fields
}

fn status_case() -> SeededCommand {
    let account_name = LogicalName::new("primary");
    let reading_node = node(
        &["meter-obs-001", "meter-obs-002"],
        None,
        "by-account",
        "status",
        2,
        6,
        ValueArithmetic::Direct,
    );
    let accounts = vec![MeterAccount::new(
        account_name.clone(),
        Freshness::Fresh {
            observed: observed_reading(500_000),
            latest_attempt: AttemptId::new(1),
        },
    )];
    let report = StatusReport::new(
        test_metadata(),
        accounts,
        vec![MeterReadingProvenance::new(account_name, reading_node)],
        ProjectionReadState::Read,
    );
    SeededCommand {
        name: "status",
        expected: accounts_expected_fields(&report.accounts),
        graph: report.provenance,
    }
}

fn now_case() -> SeededCommand {
    let account_name = LogicalName::new("primary");
    let reading_node = node(
        &["meter-obs-101"],
        None,
        "by-account",
        "now",
        1,
        1,
        ValueArithmetic::Direct,
    );
    let accounts = vec![MeterAccount::new(
        account_name.clone(),
        Freshness::Fresh {
            observed: observed_reading(400_000),
            latest_attempt: AttemptId::new(2),
        },
    )];
    let report = NowReport::new(
        test_metadata(),
        accounts,
        vec![MeterReadingProvenance::new(account_name, reading_node)],
    );
    SeededCommand {
        name: "now",
        expected: accounts_expected_fields(&report.accounts),
        graph: report.provenance,
    }
}

fn spend_case() -> SeededCommand {
    let parent_key = LogicalName::new("2026-08-26 claude-code");
    let child_key = LogicalName::new("2026-08-26 claude-code project=aub");

    let parent_node = node(
        &["tx-event-001", "tx-event-002"],
        Some("cost-model-anthropic-messages-v1"),
        "day",
        "2026-08-26..2026-08-27",
        2,
        8,
        ValueArithmetic::Sum,
    );
    let child_node = node(
        &["tx-event-003"],
        Some("cost-model-anthropic-messages-v1"),
        "day,project",
        "2026-08-26..2026-08-27",
        1,
        3,
        ValueArithmetic::Sum,
    );
    let credits_node = node(
        &["tx-event-001", "tx-event-002"],
        Some("cost-model-anthropic-messages-v1"),
        "day",
        "credits",
        2,
        8,
        ValueArithmetic::Converted {
            from: Unit::Tokens,
            to: Unit::Credits,
        },
    );
    let window_equivalent_node = node(
        &["tx-event-001", "tx-event-002"],
        None,
        "day",
        "window-equivalent",
        2,
        8,
        ValueArithmetic::Converted {
            from: Unit::Credits,
            to: Unit::PercentagePoints,
        },
    );
    let canonical_node = node(
        &["tx-event-001", "tx-event-002", "tx-event-003"],
        None,
        "day",
        "canonical-records",
        1,
        3,
        ValueArithmetic::Count,
    );
    let replayed_node = node(
        &[],
        None,
        "day",
        "replayed-occurrences",
        1,
        0,
        ValueArithmetic::Count,
    );
    let heuristic_node = node(
        &[],
        None,
        "day",
        "heuristic-identities",
        1,
        0,
        ValueArithmetic::Count,
    );

    let child = SpendGroup::new(
        child_key.clone(),
        usage_vector(20, 10),
        Provenance::new(["transcripts/claude-code/child.jsonl".to_string()]),
        DerivationId::from_manifest(child_node.manifest()),
    );
    let parent = SpendGroup::new(
        parent_key.clone(),
        usage_vector(100, 50),
        Provenance::new(["transcripts/claude-code/session-1.jsonl".to_string()]),
        DerivationId::from_manifest(parent_node.manifest()),
    )
    .with_children(vec![child])
    .with_credits(Derivation::Available(Qualified::new(
        Credits::from_micros(42_000_000),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
        Provenance::new(["cost-model-anthropic-messages-v1".to_string()]),
    )))
    .with_window_equivalent(WindowEquivalentDerivation::Available(
        WindowEquivalentValue {
            interval: Interval::new(
                PercentagePoints::new(10).unwrap(),
                PercentagePoints::new(20).unwrap(),
            )
            .unwrap(),
            calibration_id: WindowCalibrationId::new("cal-window-2026-08"),
            coverage: CoverageCompleteness::Complete,
            quality: EvidenceQuality::Measured,
            provenance: Provenance::new(["cal-window-2026-08".to_string()]),
        },
    ));

    let report = SpendReport::new(
        test_metadata(),
        UtcDate::parse("2026-08-26").unwrap(),
        UtcDate::parse("2026-08-27").unwrap(),
        vec![parent],
        vec![
            SpendGroupProvenance::new(parent_key.clone(), parent_node),
            SpendGroupProvenance::new(child_key, child_node),
        ],
        IngestSummary {
            refresh_attempted: false,
            refresh_failure: None,
            files_read: 2,
            files_skipped_before_window: 0,
            unreadable_files: vec![],
            quarantined_by_class: std::collections::BTreeMap::new(),
            replayed_occurrences: 0,
            collisions: 0,
            without_identity: 0,
            heuristic_identities: 0,
            undated_events: 0,
            events_outside_window: 0,
            events_in_window: 3,
        },
    )
    .with_credit_provenance(vec![SpendGroupCreditsProvenance::new(
        parent_key.clone(),
        credits_node,
    )])
    .with_window_equivalent_provenance(vec![SpendGroupWindowEquivalentProvenance::new(
        parent_key,
        window_equivalent_node,
    )])
    .with_diagnostics(vec![
        SpendDiagnosticProvenance {
            diagnostic: SpendDiagnostic::CanonicalRecords,
            node: canonical_node,
        },
        SpendDiagnosticProvenance {
            diagnostic: SpendDiagnostic::ReplayedOccurrences,
            node: replayed_node,
        },
        SpendDiagnosticProvenance {
            diagnostic: SpendDiagnostic::HeuristicIdentities,
            node: heuristic_node,
        },
    ]);

    SeededCommand {
        name: "spend",
        expected: spend_expected_fields(&report),
        graph: report.provenance,
    }
}

fn coverage_case() -> SeededCommand {
    let account_node = node(
        &["coverage-window-001"],
        None,
        "coverage",
        "interval",
        1,
        1,
        ValueArithmetic::Count,
    );
    let account = CoverageAccount {
        name: LogicalName::new("research"),
        engine: EngineCoverageReport {
            expected_opportunities: None,
            attempted_opportunities: 0,
            successful_observations: 0,
            started_without_terminal_result: 0,
            attempt_coverage: None,
            measurement_coverage: None,
            longest_no_attempt_gap: None,
            longest_no_observation_gap: None,
            reset_spanning_gaps: Vec::new(),
            most_recent_timer_run: None,
            most_recent_successful_observation: None,
            severe: false,
        },
        failures: CoverageFailureTally::default(),
        resets_in_gaps: Vec::new(),
        legacy_evidence_present: false,
        configured: true,
        provenance: account_node,
    };
    let report = CoverageReport::new(
        test_metadata(),
        UtcTimestamp::from_unix_nanos(0),
        UtcTimestamp::from_unix_nanos(1),
        false,
        CoverageThreshold {
            attempt_floor: CoverageFloor::new(0.98).unwrap(),
            measurement_floor: CoverageFloor::new(0.95).unwrap(),
            met: true,
            breaches: Vec::new(),
        },
        vec![account],
    );
    SeededCommand {
        name: "coverage",
        expected: coverage_expected_fields(&report),
        graph: report.provenance,
    }
}

fn export_case() -> SeededCommand {
    let rows_node = node(
        &["export-row-001"],
        None,
        "session",
        "all",
        1,
        1,
        ValueArithmetic::Count,
    );
    let report = ExportReport::new(
        test_metadata(),
        ExportKey::Session,
        false,
        vec![],
        0,
        rows_node,
    );
    SeededCommand {
        name: "export",
        expected: vec![ReportField::ExportRows],
        graph: report.provenance,
    }
}

fn calibrate_case() -> SeededCommand {
    let tokens_node = node(
        &["calib-obs-001", "calib-obs-002"],
        None,
        "single-source",
        "window=2026-08",
        1,
        2,
        ValueArithmetic::Sum,
    );
    let derivation = Derivation::Available(Qualified::new(
        TokenCount::new(12_000),
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
        Provenance::new(["calibration-run-2026-08".to_string()]),
    ));
    let report = CalibrateReport::new(test_metadata(), derivation, tokens_node);
    SeededCommand {
        name: "calibrate",
        expected: vec![ReportField::CalibrationTokens],
        graph: report.provenance,
    }
}

fn task_case() -> SeededCommand {
    let task_id = LogicalName::new("beads-a:aub-xus.6");
    let usage_node = node(
        &["task-session-001"],
        None,
        "task",
        "all",
        1,
        1,
        ValueArithmetic::Sum,
    );
    let credits_node = node(
        &["task-session-001"],
        Some("cost-model-anthropic-messages-v1"),
        "task",
        "credits",
        1,
        1,
        ValueArithmetic::Converted {
            from: Unit::Tokens,
            to: Unit::Credits,
        },
    );
    let report = TaskReport::new(
        test_metadata(),
        task_id,
        None,
        usage_vector(200, 80),
        Derivation::Available(Qualified::new(
            Credits::from_micros(9_000_000),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
            Provenance::new(["cost-model-anthropic-messages-v1".to_string()]),
        )),
        vec![],
        usage_node,
        credits_node,
    );
    SeededCommand {
        name: "task",
        expected: task_expected_fields(&report),
        graph: report.provenance,
    }
}

fn task_overhead_case() -> SeededCommand {
    let task_usage_node = node(
        &["overhead-session-001"],
        None,
        "task-overhead",
        "task-usage",
        1,
        1,
        ValueArithmetic::Sum,
    );
    let bucket_node = node(
        &["overhead-session-002"],
        None,
        "task-overhead",
        "bucket=contended",
        1,
        1,
        ValueArithmetic::Sum,
    );
    let bucket = TaskOverheadBucket {
        reason: LogicalName::new("contended"),
        usage: usage_vector(30, 10),
        share: SharePpm::of(30, 100),
    };
    let report = TaskOverheadReport::new(
        test_metadata(),
        UtcDate::parse("2026-08-26").unwrap(),
        UtcDate::parse("2026-08-27").unwrap(),
        usage_vector(300, 120),
        task_usage_node,
        vec![bucket],
        vec![(LogicalName::new("contended"), bucket_node)],
    );
    SeededCommand {
        name: "task-overhead",
        expected: task_overhead_expected_fields(&report),
        graph: report.provenance,
    }
}

/// Every seeded end-to-end command that exposes a physical quantity.
/// `sample`, `ingest`, `backup` and `doctor` are absent on purpose: see the
/// module doc comment.
fn seeded_commands() -> Vec<SeededCommand> {
    vec![
        status_case(),
        now_case(),
        spend_case(),
        coverage_case(),
        export_case(),
        calibrate_case(),
        task_case(),
        task_overhead_case(),
    ]
}

/// The stable "kind" of a [`ReportField`], ignoring its parameterized key.
///
/// This match is exhaustive over `ReportField`: a variant added anywhere in
/// the crate fails to compile here until an arm is added for it, which is
/// what turns "a seeded command added without provenance-suite registration"
/// into a build failure rather than a silently passing gap.
fn field_kind(field: &ReportField) -> &'static str {
    match field {
        ReportField::MeterQuotaRemaining { .. } => "meter_quota_remaining",
        ReportField::SpendGroupTokens { .. } => "spend_group_tokens",
        ReportField::SpendGroupCredits { .. } => "spend_group_credits",
        ReportField::SpendGroupWindowEquivalent { .. } => "spend_group_window_equivalent",
        ReportField::SpendCanonicalRecords => "spend_canonical_records",
        ReportField::SpendReplayedOccurrences => "spend_replayed_occurrences",
        ReportField::SpendHeuristicIdentities => "spend_heuristic_identities",
        ReportField::Coverage { .. } => "coverage",
        ReportField::ExportRows => "export_rows",
        ReportField::CalibrationTokens => "calibration_tokens",
        ReportField::TaskUsage { .. } => "task_usage",
        ReportField::TaskCredits { .. } => "task_credits",
        ReportField::TaskOverheadBucket { .. } => "task_overhead_bucket",
        ReportField::TaskOverheadTaskUsage => "task_overhead_task_usage",
    }
}

const ALL_REPORT_FIELD_KINDS: [&str; 14] = [
    "meter_quota_remaining",
    "spend_group_tokens",
    "spend_group_credits",
    "spend_group_window_equivalent",
    "spend_canonical_records",
    "spend_replayed_occurrences",
    "spend_heuristic_identities",
    "coverage",
    "export_rows",
    "calibration_tokens",
    "task_usage",
    "task_credits",
    "task_overhead_bucket",
    "task_overhead_task_usage",
];

// --- tests ----------------------------------------------------------------

/// Integration: every seeded end-to-end command's physical quantity fields
/// resolve to a provenance node by typed [`ReportField`] identifier, and every
/// resolved node verifies against its own manifest.
#[test]
fn every_seeded_command_field_resolves_by_typed_identifier() {
    for case in seeded_commands() {
        assert!(
            !case.expected.is_empty(),
            "{}: seeded no physical-quantity fields to check",
            case.name
        );
        for field in &case.expected {
            let node = case.graph.resolve(field).unwrap_or_else(|| {
                panic!(
                    "{}: field {} has no provenance node",
                    case.name,
                    field.label()
                )
            });
            assert!(
                node.verify(),
                "{}: field {} node fails its own manifest expansion",
                case.name,
                field.label()
            );
        }
    }
}

/// Integration: the registry covers every [`ReportField`] kind the enum
/// defines. Combined with `field_kind`'s exhaustive match, a report field
/// added anywhere without a seeded case exercising it fails here.
#[test]
fn registry_covers_every_report_field_kind() {
    let mut covered = BTreeSet::new();
    for case in seeded_commands() {
        for field in &case.expected {
            covered.insert(field_kind(field));
        }
    }
    let expected: BTreeSet<&str> = ALL_REPORT_FIELD_KINDS.into_iter().collect();
    assert_eq!(
        covered, expected,
        "a ReportField kind is not exercised by any seeded command, or a seeded command \
         claims a kind absent from the enumeration"
    );
}

/// Integration: every seeded node's canonical member set rehashes to exactly
/// its own manifest hash, using the provenance types' own hash function
/// rather than a second implementation of the law.
#[test]
fn every_seeded_command_node_expands_to_exactly_its_manifest_hash() {
    for case in seeded_commands() {
        for (field, node) in case.graph.iter() {
            assert_eq!(
                canonical_inputs_hash(node.members()),
                node.manifest().inputs_hash(),
                "{}: field {} does not rehash to its own manifest",
                case.name,
                field.label()
            );
            assert!(node.verify());
        }
    }
}

/// Integration: corrupting one member of a real seeded command's manifest is
/// caught by [`ProvenanceManifest::verify_expansion`], the same law
/// `report/provenance.rs` and `domain/provenance.rs` already prove in
/// isolation; this exercises it against material an actual command emitted.
#[test]
fn corrupting_a_seeded_commands_manifest_member_is_detected() {
    let case = seeded_commands()
        .into_iter()
        .find(|case| case.name == "spend")
        .expect("the spend case must be registered");
    let (field, node) = case
        .graph
        .iter()
        .find(|(_, node)| !node.members().is_empty())
        .expect("at least one spend node must carry members");

    let mut corrupted = node.members().clone();
    let sacrificed = corrupted
        .iter()
        .next()
        .cloned()
        .expect("members must be non-empty by construction above");
    corrupted.remove(&sacrificed);
    corrupted.insert(EvidenceId::new("tampered-member"));

    assert!(
        !node.manifest().verify_expansion(&corrupted),
        "{}: a corrupted member set must not verify against the manifest hash",
        field.label()
    );
    // The manifest built directly from the corrupted set also disagrees with
    // the original: corruption is detectable both ways, never only one.
    let corrupted_manifest = ProvenanceManifest::new(corrupted, [], QuerySemantics::new("x", "x"));
    assert_ne!(
        corrupted_manifest.inputs_hash(),
        node.manifest().inputs_hash()
    );
}

/// Integration: a report missing a physical quantity's provenance node fails
/// this suite's check rather than passing silently. Built the same way the
/// positive cases are, minus the one provenance registration, so the only
/// difference from a passing case is the deliberate omission.
#[test]
fn a_field_missing_its_provenance_node_makes_the_check_fail() {
    let account_name = LogicalName::new("omitted");
    let accounts = vec![MeterAccount::new(
        account_name,
        Freshness::Fresh {
            observed: observed_reading(500_000),
            latest_attempt: AttemptId::new(1),
        },
    )];
    // No `MeterReadingProvenance` supplied: the constructor assembles an
    // empty graph even though the account list still names a reading.
    let report = StatusReport::new(test_metadata(), accounts, vec![], ProjectionReadState::Read);
    let expected = accounts_expected_fields(&report.accounts);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for field in &expected {
            let node = report
                .provenance
                .resolve(field)
                .unwrap_or_else(|| panic!("missing provenance node for {}", field.label()));
            assert!(node.verify());
        }
    }));
    assert!(
        result.is_err(),
        "a report missing a provenance node for one of its own quantity fields must fail the check"
    );
}

/// Unit: this suite's own source contains neither an ASCII-digit predicate nor
/// a regular expression, which is what a digit-string search over rendered
/// output would require. Both forbidden fragments are assembled from parts so
/// neither appears verbatim in this file; a literal copy here would make this
/// very check trip on itself.
#[test]
fn the_suite_matches_by_typed_field_identifier_not_digit_string_search() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/provenance_graph.rs"
    ))
    .expect("this test's own source must be readable");
    let forbidden = [["is_ascii", "_digit"].concat(), ["Reg", "ex::new"].concat()];
    for pattern in forbidden {
        assert!(
            !source.contains(&pattern),
            "found {pattern:?} in this file: matching must be by typed ReportField \
             identifier against the provenance graph, never a digit-string search over \
             rendered output"
        );
    }
}
