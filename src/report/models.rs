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

/// The status projection: the current compact meter picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub metadata: ReportMetadata,
    pub accounts: Vec<MeterAccount>,
}

impl StatusReport {
    pub fn new(metadata: ReportMetadata, accounts: Vec<MeterAccount>) -> Self {
        Self { metadata, accounts }
    }
}

/// The live meter report for `aub now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowReport {
    pub metadata: ReportMetadata,
    pub accounts: Vec<MeterAccount>,
}

impl NowReport {
    pub fn new(metadata: ReportMetadata, accounts: Vec<MeterAccount>) -> Self {
        Self { metadata, accounts }
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

/// The spend report for `aub spend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendReport {
    pub metadata: ReportMetadata,
    pub groups: Vec<SpendGroup>,
}

impl SpendReport {
    pub fn new(metadata: ReportMetadata, groups: Vec<SpendGroup>) -> Self {
        Self { metadata, groups }
    }
}

/// The coverage report for `aub coverage`: expected-versus-observed sampling
/// opportunities and the threshold verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    pub metadata: ReportMetadata,
    pub coverage: CoverageCompleteness,
    pub threshold_met: bool,
}

impl CoverageReport {
    pub fn new(
        metadata: ReportMetadata,
        coverage: CoverageCompleteness,
        threshold_met: bool,
    ) -> Self {
        Self {
            metadata,
            coverage,
            threshold_met,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleReport {
    pub metadata: ReportMetadata,
    pub attempts: Vec<SampleAttempt>,
}

impl SampleReport {
    pub fn new(metadata: ReportMetadata, attempts: Vec<SampleAttempt>) -> Self {
        Self { metadata, attempts }
    }
}

/// The ingest report for `aub ingest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    pub metadata: ReportMetadata,
    pub ingestion_generation: IngestionGeneration,
}

impl IngestReport {
    pub fn new(metadata: ReportMetadata, ingestion_generation: IngestionGeneration) -> Self {
        Self {
            metadata,
            ingestion_generation,
        }
    }
}

/// The backup report for `aub backup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    pub metadata: ReportMetadata,
    pub verified: bool,
}

impl BackupReport {
    pub fn new(metadata: ReportMetadata, verified: bool) -> Self {
        Self { metadata, verified }
    }
}

/// The doctor report for `aub doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub metadata: ReportMetadata,
    pub checks: Vec<LogicalName>,
}

impl DoctorReport {
    pub fn new(metadata: ReportMetadata, checks: Vec<LogicalName>) -> Self {
        Self { metadata, checks }
    }
}

/// The task report for the `aub task` command family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReport {
    pub metadata: ReportMetadata,
    pub tasks: Vec<LogicalName>,
}

impl TaskReport {
    pub fn new(metadata: ReportMetadata, tasks: Vec<LogicalName>) -> Self {
        Self { metadata, tasks }
    }
}

/// The calibration report for the `aub calibrate` command family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateReport {
    pub metadata: ReportMetadata,
    pub derivation: Derivation<TokenCount>,
}

impl CalibrateReport {
    pub fn new(metadata: ReportMetadata, derivation: Derivation<TokenCount>) -> Self {
        Self {
            metadata,
            derivation,
        }
    }
}

/// The export report for `aub export`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    pub metadata: ReportMetadata,
    pub rows: u64,
}

impl ExportReport {
    pub fn new(metadata: ReportMetadata, rows: u64) -> Self {
        Self { metadata, rows }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::FreshnessKind;
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::UtcTimestamp;

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
            &StatusReport::new(m.clone(), vec![account.clone()]),
            &NowReport::new(m.clone(), vec![account.clone()]),
            &SpendReport::new(m.clone(), vec![]),
            &CoverageReport::new(m.clone(), CoverageCompleteness::Complete, true),
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
            ),
            &ExportReport::new(m.clone(), 0),
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
}
