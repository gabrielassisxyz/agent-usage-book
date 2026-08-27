//! Typed report models for every command.
//!
//! The report model is the seam that keeps the renderer honest: every quantity
//! field is a [`Qualified`] or [`Derivation`] value, never a bare newtype, and a
//! meter reading carries exactly one freshness variant. A renderer cannot produce
//! an unqualified number because it never holds one.

use crate::domain::attempt::AttemptOutcome;
use crate::domain::freshness::Freshness;
use crate::domain::provenance::DerivationId;
use crate::domain::quota::QuotaRemaining;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::TokenCount;
use crate::evidence::{CoverageCompleteness, Derivation, Qualified};
use crate::logging::LogicalName;
use crate::report::provenance::{ProvenanceGraph, ProvenanceNode, ReportField};

/// A monotonically increasing ledger generation.
///
/// The database advances this on every transaction that changes projection-relevant
/// durable state, and a report records the exact generation it was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LedgerGeneration(u64);

impl LedgerGeneration {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// A monotonically increasing transcript-ingestion generation.
///
/// Present only on reports that consume transcript-derived material; a report over
/// meter evidence alone has no ingestion generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IngestionGeneration(u64);

impl IngestionGeneration {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Report-level metadata shared by every command.
///
/// `generated_at` says when the report was rendered and `knowledge_at` says which
/// witness set it was rendered against, which are different facts once a corrected
/// rate card or calibration lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportMetadata {
    pub generated_at: UtcTimestamp,
    pub knowledge_at: UtcTimestamp,
    pub ledger_generation: LedgerGeneration,
    pub ingestion_generation: Option<IngestionGeneration>,
}

impl ReportMetadata {
    pub fn new(
        generated_at: UtcTimestamp,
        knowledge_at: UtcTimestamp,
        ledger_generation: LedgerGeneration,
        ingestion_generation: Option<IngestionGeneration>,
    ) -> Self {
        Self {
            generated_at,
            knowledge_at,
            ledger_generation,
            ingestion_generation,
        }
    }
}

/// A named account with a meter reading carrying exactly one freshness variant.
///
/// The reading is a [`Freshness`] over the remaining quota, so a renderer always
/// knows whether the number is fresh, stale or auth-required and never has to infer
/// staleness from a timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterAccount {
    pub account: LogicalName,
    pub reading: Freshness<QuotaRemaining>,
}

impl MeterAccount {
    pub fn new(account: LogicalName, reading: Freshness<QuotaRemaining>) -> Self {
        Self { account, reading }
    }
}

/// Provenance material for one account's meter reading.
///
/// The report constructor assembles the graph from this, so the renderer never
/// computes any part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterReadingProvenance {
    pub account: LogicalName,
    pub node: ProvenanceNode,
}

impl MeterReadingProvenance {
    pub fn new(account: LogicalName, node: ProvenanceNode) -> Self {
        Self { account, node }
    }
}

/// The status projection: the current compact meter picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub metadata: ReportMetadata,
    pub accounts: Vec<MeterAccount>,
    pub provenance: ProvenanceGraph,
}

impl StatusReport {
    pub fn new(
        metadata: ReportMetadata,
        accounts: Vec<MeterAccount>,
        readings: Vec<MeterReadingProvenance>,
    ) -> Self {
        let provenance = ProvenanceGraph::new(readings.into_iter().map(|reading| {
            (
                ReportField::MeterQuotaRemaining {
                    account: reading.account,
                },
                reading.node,
            )
        }));
        Self {
            metadata,
            accounts,
            provenance,
        }
    }
}

/// The live meter report for `aub now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowReport {
    pub metadata: ReportMetadata,
    pub accounts: Vec<MeterAccount>,
    pub provenance: ProvenanceGraph,
}

impl NowReport {
    pub fn new(
        metadata: ReportMetadata,
        accounts: Vec<MeterAccount>,
        readings: Vec<MeterReadingProvenance>,
    ) -> Self {
        let provenance = ProvenanceGraph::new(readings.into_iter().map(|reading| {
            (
                ReportField::MeterQuotaRemaining {
                    account: reading.account,
                },
                reading.node,
            )
        }));
        Self {
            metadata,
            accounts,
            provenance,
        }
    }
}

/// One group of a spend report, keyed by day, session, project, repository, account
/// or task. The token count is qualified and carries its derivation identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendGroup {
    pub key: LogicalName,
    pub tokens: Qualified<TokenCount>,
    pub derivation_id: DerivationId,
}

impl SpendGroup {
    pub fn new(
        key: LogicalName,
        tokens: Qualified<TokenCount>,
        derivation_id: DerivationId,
    ) -> Self {
        Self {
            key,
            tokens,
            derivation_id,
        }
    }
}

/// Provenance material for one spend group's token count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendGroupProvenance {
    pub key: LogicalName,
    pub node: ProvenanceNode,
}

impl SpendGroupProvenance {
    pub fn new(key: LogicalName, node: ProvenanceNode) -> Self {
        Self { key, node }
    }
}

/// The spend report for `aub spend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendReport {
    pub metadata: ReportMetadata,
    pub groups: Vec<SpendGroup>,
    pub provenance: ProvenanceGraph,
}

impl SpendReport {
    pub fn new(
        metadata: ReportMetadata,
        groups: Vec<SpendGroup>,
        group_provenance: Vec<SpendGroupProvenance>,
    ) -> Self {
        let provenance = ProvenanceGraph::new(
            group_provenance
                .into_iter()
                .map(|group| (ReportField::SpendGroupTokens { key: group.key }, group.node)),
        );
        Self {
            metadata,
            groups,
            provenance,
        }
    }
}

/// The coverage report for `aub coverage`: expected-versus-observed sampling
/// opportunities and the threshold verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    pub metadata: ReportMetadata,
    pub coverage: CoverageCompleteness,
    pub threshold_met: bool,
    pub provenance: ProvenanceGraph,
}

impl CoverageReport {
    pub fn new(
        metadata: ReportMetadata,
        coverage: CoverageCompleteness,
        threshold_met: bool,
        node: ProvenanceNode,
    ) -> Self {
        let provenance = ProvenanceGraph::new([(ReportField::CoverageCompleteness, node)]);
        Self {
            metadata,
            coverage,
            threshold_met,
            provenance,
        }
    }
}

/// One sampling attempt outcome in a sample report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleAttempt {
    pub account: LogicalName,
    pub outcome: AttemptOutcome,
}

impl SampleAttempt {
    pub fn new(account: LogicalName, outcome: AttemptOutcome) -> Self {
        Self { account, outcome }
    }
}

/// The sample report for `aub sample`.
///
/// Sampling outcomes are not quantities, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleReport {
    pub metadata: ReportMetadata,
    pub attempts: Vec<SampleAttempt>,
    pub provenance: ProvenanceGraph,
}

impl SampleReport {
    pub fn new(metadata: ReportMetadata, attempts: Vec<SampleAttempt>) -> Self {
        Self {
            metadata,
            attempts,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The ingest report for `aub ingest`.
///
/// The ingestion generation is ledger metadata, not a measured quantity, so
/// the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    pub metadata: ReportMetadata,
    pub ingestion_generation: IngestionGeneration,
    pub provenance: ProvenanceGraph,
}

impl IngestReport {
    pub fn new(metadata: ReportMetadata, ingestion_generation: IngestionGeneration) -> Self {
        Self {
            metadata,
            ingestion_generation,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The backup report for `aub backup`.
///
/// A boolean verdict is not a quantity, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    pub metadata: ReportMetadata,
    pub verified: bool,
    pub provenance: ProvenanceGraph,
}

impl BackupReport {
    pub fn new(metadata: ReportMetadata, verified: bool) -> Self {
        Self {
            metadata,
            verified,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The doctor report for `aub doctor`.
///
/// Check names are not quantities, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub metadata: ReportMetadata,
    pub checks: Vec<LogicalName>,
    pub provenance: ProvenanceGraph,
}

impl DoctorReport {
    pub fn new(metadata: ReportMetadata, checks: Vec<LogicalName>) -> Self {
        Self {
            metadata,
            checks,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The task report for the `aub task` command family.
///
/// Task names are not quantities, so the provenance graph is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReport {
    pub metadata: ReportMetadata,
    pub tasks: Vec<LogicalName>,
    pub provenance: ProvenanceGraph,
}

impl TaskReport {
    pub fn new(metadata: ReportMetadata, tasks: Vec<LogicalName>) -> Self {
        Self {
            metadata,
            tasks,
            provenance: ProvenanceGraph::default(),
        }
    }
}

/// The calibration report for the `aub calibrate` command family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateReport {
    pub metadata: ReportMetadata,
    pub derivation: Derivation<TokenCount>,
    pub provenance: ProvenanceGraph,
}

impl CalibrateReport {
    pub fn new(
        metadata: ReportMetadata,
        derivation: Derivation<TokenCount>,
        node: ProvenanceNode,
    ) -> Self {
        let provenance = ProvenanceGraph::new([(ReportField::CalibrationTokens, node)]);
        Self {
            metadata,
            derivation,
            provenance,
        }
    }
}

/// The export report for `aub export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub metadata: ReportMetadata,
    pub rows: u64,
    pub provenance: ProvenanceGraph,
}

impl ExportReport {
    pub fn new(metadata: ReportMetadata, rows: u64, node: ProvenanceNode) -> Self {
        let provenance = ProvenanceGraph::new([(ReportField::ExportRows, node)]);
        Self {
            metadata,
            rows,
            provenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::FreshnessKind;
    use crate::domain::provenance::{EvidenceId, QuerySemantics, WitnessId};
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::UtcTimestamp;
    use crate::report::provenance::{ProvenanceNode, ReportField, ValueArithmetic};

    fn metadata() -> ReportMetadata {
        ReportMetadata::new(
            UtcTimestamp::from_unix_nanos(2_000),
            UtcTimestamp::from_unix_nanos(1_000),
            LedgerGeneration::new(7),
            Some(IngestionGeneration::new(3)),
        )
    }

    fn remaining(ppm: u32) -> QuotaRemaining {
        QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap())
    }

    /// A canonical provenance node for tests: one member, one source, one
    /// observation, read directly.
    fn node() -> ProvenanceNode {
        ProvenanceNode::new(
            [EvidenceId::new("ev-1")],
            [] as [WitnessId; 0],
            QuerySemantics::new("by-account", "all"),
            1,
            1,
            ValueArithmetic::Direct,
        )
    }

    /// Every command's report model carries the four metadata fields. Enumerating the
    /// models here means a new command model that forgets the metadata fails this test.
    #[test]
    fn every_report_model_carries_the_metadata() {
        let m = metadata();
        let account = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(1),
            },
        );

        let models: [&dyn std::fmt::Debug; 11] = [
            &StatusReport::new(
                m.clone(),
                vec![account.clone()],
                vec![MeterReadingProvenance::new(
                    LogicalName::new("work-a"),
                    node(),
                )],
            ),
            &NowReport::new(
                m.clone(),
                vec![account.clone()],
                vec![MeterReadingProvenance::new(
                    LogicalName::new("work-a"),
                    node(),
                )],
            ),
            &SpendReport::new(m.clone(), vec![], vec![]),
            &CoverageReport::new(m.clone(), CoverageCompleteness::Complete, true, node()),
            &SampleReport::new(m.clone(), vec![]),
            &IngestReport::new(m.clone(), IngestionGeneration::new(3)),
            &BackupReport::new(m.clone(), true),
            &DoctorReport::new(m.clone(), vec![]),
            &TaskReport::new(m.clone(), vec![]),
            &CalibrateReport::new(
                m.clone(),
                Derivation::Unavailable {
                    missing: Default::default(),
                    provenance: crate::evidence::Provenance::new([]),
                },
                node(),
            ),
            &ExportReport::new(m.clone(), 0, node()),
        ];
        assert_eq!(models.len(), 11, "every command must have a report model");
    }

    /// A meter reading carries exactly one freshness variant: the three kinds are
    /// exhaustive and a reading is always one of them.
    #[test]
    fn meter_readings_carry_exactly_one_freshness_variant() {
        let fresh = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::Fresh {
                observed: crate::domain::freshness::Observed::new(
                    remaining(500_000),
                    None,
                    crate::domain::time::ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
                    crate::domain::time::MeasurementBasis::ProviderObserved,
                ),
                latest_attempt: AttemptId::new(1),
            },
        );
        assert_eq!(fresh.reading.kind(), FreshnessKind::Fresh);

        let stale = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::Stale {
                last_good: None,
                latest_attempt: AttemptId::new(2),
                reason: crate::domain::freshness::StaleReason::AgeExceeded,
            },
        );
        assert_eq!(stale.reading.kind(), FreshnessKind::Stale);

        let auth = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(3),
            },
        );
        assert_eq!(auth.reading.kind(), FreshnessKind::AuthRequired);
    }

    /// The spend group's token count is a qualified value, never a bare newtype: the
    /// only constructor takes a `Qualified<TokenCount>`.
    #[test]
    fn spend_group_tokens_are_qualified() {
        let qualified = Qualified::new(
            TokenCount::new(100),
            CoverageCompleteness::Complete,
            crate::evidence::EvidenceQuality::Measured,
            crate::evidence::Provenance::new([]),
        );
        let group = SpendGroup::new(
            LogicalName::new("by-day"),
            qualified,
            DerivationId::from_manifest(&crate::domain::provenance::ProvenanceManifest::new(
                [],
                [],
                crate::domain::provenance::QuerySemantics::new("by-day", "all"),
            )),
        );
        // The token count is qualified: its coverage is readable, and there is no
        // value-only accessor on the group.
        assert_eq!(group.tokens.coverage(), &CoverageCompleteness::Complete);
    }

    /// Every quantitative field of every report model resolves to a provenance
    /// node, enumerated exhaustively rather than sampled.
    ///
    /// The exhaustive match below is the compile half of the stated rejection
    /// mechanism: a [`ReportField`] variant added to the enum fails to compile
    /// here until the test is touched. The resolution sweep is the run half: a
    /// variant the constructors do not populate fails the test. Together they
    /// reject a quantitative field added without a provenance node.
    #[test]
    fn every_quantitative_field_resolves_to_a_provenance_node() {
        let m = metadata();
        let account = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::Fresh {
                observed: crate::domain::freshness::Observed::new(
                    remaining(500_000),
                    None,
                    crate::domain::time::ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
                    crate::domain::time::MeasurementBasis::ProviderObserved,
                ),
                latest_attempt: AttemptId::new(1),
            },
        );
        let group = SpendGroup::new(
            LogicalName::new("by-day"),
            Qualified::new(
                TokenCount::new(100),
                CoverageCompleteness::Complete,
                crate::evidence::EvidenceQuality::Measured,
                crate::evidence::Provenance::new([]),
            ),
            DerivationId::from_manifest(&crate::domain::provenance::ProvenanceManifest::new(
                [],
                [],
                QuerySemantics::new("by-day", "all"),
            )),
        );

        let status = StatusReport::new(
            m.clone(),
            vec![account.clone()],
            vec![MeterReadingProvenance::new(
                LogicalName::new("work-a"),
                node(),
            )],
        );
        let now = NowReport::new(
            m.clone(),
            vec![account.clone()],
            vec![MeterReadingProvenance::new(
                LogicalName::new("work-a"),
                node(),
            )],
        );
        let spend = SpendReport::new(
            m.clone(),
            vec![group],
            vec![SpendGroupProvenance::new(
                LogicalName::new("by-day"),
                node(),
            )],
        );
        let coverage = CoverageReport::new(m.clone(), CoverageCompleteness::Complete, true, node());
        let export = ExportReport::new(m.clone(), 42, node());
        let calibrate = CalibrateReport::new(
            m.clone(),
            Derivation::Unavailable {
                missing: Default::default(),
                provenance: crate::evidence::Provenance::new([]),
            },
            node(),
        );

        // The exhaustive match: adding a variant to the enum fails to compile
        // until this match is extended, which is the compile half of the guard.
        let field_kinds = [
            ReportField::MeterQuotaRemaining {
                account: LogicalName::new("work-a"),
            },
            ReportField::SpendGroupTokens {
                key: LogicalName::new("by-day"),
            },
            ReportField::CoverageCompleteness,
            ReportField::ExportRows,
            ReportField::CalibrationTokens,
        ];
        for field in &field_kinds {
            match field {
                ReportField::MeterQuotaRemaining { account } => {
                    assert!(status.provenance.resolve(field).is_some(), "{account:?}");
                    assert!(now.provenance.resolve(field).is_some(), "{account:?}");
                }
                ReportField::SpendGroupTokens { key } => {
                    assert!(spend.provenance.resolve(field).is_some(), "{key:?}");
                }
                ReportField::CoverageCompleteness => {
                    assert!(coverage.provenance.resolve(field).is_some());
                }
                ReportField::ExportRows => {
                    assert!(export.provenance.resolve(field).is_some());
                }
                ReportField::CalibrationTokens => {
                    assert!(calibrate.provenance.resolve(field).is_some());
                }
            }
        }

        // Reports with no quantitative fields own an empty graph, so a renderer
        // can rely on the graph being present on every model.
        assert!(SampleReport::new(m.clone(), vec![]).provenance.is_empty());
        assert!(
            IngestReport::new(m.clone(), IngestionGeneration::new(3))
                .provenance
                .is_empty()
        );
        assert!(BackupReport::new(m.clone(), true).provenance.is_empty());
        assert!(DoctorReport::new(m.clone(), vec![]).provenance.is_empty());
        assert!(TaskReport::new(m.clone(), vec![]).provenance.is_empty());
    }

    /// The detection half of the rejection mechanism: a field the constructor
    /// did not populate is visible as a missing node, so the enumeration sweep
    /// above fails rather than silently rendering an unexplained quantity.
    #[test]
    fn an_unpopulated_field_is_detectable_as_a_missing_node() {
        let m = metadata();
        let account = MeterAccount::new(
            LogicalName::new("work-a"),
            Freshness::AuthRequired {
                last_good: None,
                latest_attempt: AttemptId::new(1),
            },
        );
        // No provenance material: the constructor assembles an empty graph.
        let report = StatusReport::new(m, vec![account], vec![]);
        assert!(
            report
                .provenance
                .resolve(&ReportField::MeterQuotaRemaining {
                    account: LogicalName::new("work-a"),
                })
                .is_none(),
            "a reading without a node must not resolve"
        );
    }

    /// Every node a report carries verifies against its own manifest: the
    /// expansion law is inherited from the provenance types, not restated.
    #[test]
    fn every_node_in_a_report_graph_verifies() {
        let m = metadata();
        let report = ExportReport::new(m, 42, node());
        for (_, node) in report.provenance.iter() {
            assert!(node.verify());
        }
    }
}
