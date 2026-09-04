//! The `aub doctor` check registry and its check-versus-repair split (`aub-n27.7`,
//! PLAN.md sections 20.3, 27, 36).
//!
//! `doctor` is a registry, not one function that reaches into every subsystem: each
//! module contributes a check next to the evidence it owns, and this module only
//! composes the results. That inversion is what keeps a check honest. A check written
//! by whoever assembles the report tends to test what is easy to observe from
//! outside a module; a check written beside the evidence sees the failure the module
//! itself would flag.
//!
//! [`CheckName::EXPECTED`] is the design's full eighteen-condition list, encoded once
//! so the registry can be compared against it rather than trusted by inspection. An
//! entry with no registered [`CheckOutcome`] is a failing build
//! ([`missing_checks`]), and an entry whose owning subsystem is not built yet is
//! registered as [`CheckStatus::NotYetAvailable`] naming the bead that will own it,
//! never silently omitted.
//!
//! May not depend on:
//! - provider adapters directly (a check reads the store and the config, never a
//!   live endpoint: `doctor` performs no network operation)

pub mod checks;
pub mod fix;

pub use checks::{DoctorContext, build_registry, configuration_failed_registry};
pub use fix::{FixReport, ForbiddenRepair, RepairAction, run_fix};

/// Every diagnostic condition `doctor` is expected to cover. One variant per bullet
/// of PLAN.md's check list (sections 27 and 36), in the list's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckName {
    ConfigurationValidity,
    SqliteAndSchemaHealth,
    StrictAndConstraintIntegrity,
    PendingEvidence,
    SamplingCadence,
    UnresolvedAuthentication,
    TranscriptRoots,
    ParserFailures,
    UnmappedAccounts,
    MissingActiveCalibrations,
    StaleRateCards,
    ProjectionVersusDatabaseGeneration,
    BackupAge,
    MeterAnomalies,
    UnexplainedResidual,
    HeuristicDedupCounts,
    ClockSkew,
    LocalFilesystemAndWalSuitability,
    AccumulatedDiagnosticMaterial,
}

impl CheckName {
    /// The design's full check list (PLAN.md 27, 36, aub-smqu), encoded once so the
    /// registry can be compared against it. Nineteen entries.
    pub const EXPECTED: [CheckName; 19] = [
        Self::ConfigurationValidity,
        Self::SqliteAndSchemaHealth,
        Self::StrictAndConstraintIntegrity,
        Self::PendingEvidence,
        Self::SamplingCadence,
        Self::UnresolvedAuthentication,
        Self::TranscriptRoots,
        Self::ParserFailures,
        Self::UnmappedAccounts,
        Self::MissingActiveCalibrations,
        Self::StaleRateCards,
        Self::ProjectionVersusDatabaseGeneration,
        Self::BackupAge,
        Self::MeterAnomalies,
        Self::UnexplainedResidual,
        Self::HeuristicDedupCounts,
        Self::ClockSkew,
        Self::LocalFilesystemAndWalSuitability,
        Self::AccumulatedDiagnosticMaterial,
    ];

    /// The stable kebab-case name: the public identifier in text and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfigurationValidity => "configuration-validity",
            Self::SqliteAndSchemaHealth => "sqlite-and-schema-health",
            Self::StrictAndConstraintIntegrity => "strict-and-constraint-integrity",
            Self::PendingEvidence => "pending-evidence",
            Self::SamplingCadence => "sampling-cadence",
            Self::UnresolvedAuthentication => "unresolved-authentication",
            Self::TranscriptRoots => "transcript-roots",
            Self::ParserFailures => "parser-failures",
            Self::UnmappedAccounts => "unmapped-accounts",
            Self::MissingActiveCalibrations => "missing-active-calibrations",
            Self::StaleRateCards => "stale-rate-cards",
            Self::ProjectionVersusDatabaseGeneration => "projection-versus-database-generation",
            Self::BackupAge => "backup-age",
            Self::MeterAnomalies => "meter-anomalies",
            Self::UnexplainedResidual => "unexplained-residual",
            Self::HeuristicDedupCounts => "heuristic-dedup-counts",
            Self::ClockSkew => "clock-skew",
            Self::LocalFilesystemAndWalSuitability => "local-filesystem-and-wal-suitability",
            Self::AccumulatedDiagnosticMaterial => "accumulated-diagnostic-material",
        }
    }
}

/// The outcome of one check. Never a bare pass/fail boolean: a check that cannot
/// apply to the current configuration says so and why, and a check whose owning
/// subsystem is not built yet says which bead will own it, rather than going
/// missing in either case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    PassWithDetail(String),
    Fail(String),
    NotApplicable(String),
    NotYetAvailable { owning_bead: &'static str },
}

impl CheckStatus {
    /// The stable label used by both the text and the JSON renderer, so the two
    /// cannot drift from each other about what a state is called.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pass | Self::PassWithDetail(_) => "pass",
            Self::Fail(_) => "fail",
            Self::NotApplicable(_) => "not_applicable",
            Self::NotYetAvailable { .. } => "not_yet_available",
        }
    }
}

/// One registered check's declaration and the result of running it: the name, the
/// module that owns the evidence, the condition it detects, whether a repair
/// exists for it, and its outcome. `doctor` composes and renders these without
/// knowing what any individual check read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub name: CheckName,
    pub owner_module: &'static str,
    pub condition: &'static str,
    pub has_repair: bool,
    pub status: CheckStatus,
}

/// The full report: every registered outcome, in registration order, plus the
/// shared report metadata every command's JSON envelope carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub metadata: crate::report::ReportMetadata,
    pub outcomes: Vec<CheckOutcome>,
}

impl DoctorReport {
    pub fn passed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::Pass | CheckStatus::PassWithDetail(_)))
            .count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::Fail(_)))
            .count()
    }

    pub fn not_applicable(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::NotApplicable(_)))
            .count()
    }

    pub fn not_yet_available(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, CheckStatus::NotYetAvailable { .. }))
            .count()
    }
}

/// Every expected [`CheckName`] with no matching entry in `outcomes`: the
/// consistency test this bead's acceptance criteria require. A non-empty result
/// names exactly which condition would go silently missing, rather than failing a
/// generic assertion nobody can act on.
pub fn missing_checks(outcomes: &[CheckOutcome]) -> Vec<CheckName> {
    let registered: std::collections::BTreeSet<CheckName> =
        outcomes.iter().map(|outcome| outcome.name).collect();
    CheckName::EXPECTED
        .iter()
        .copied()
        .filter(|name| !registered.contains(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_list_has_no_duplicate() {
        let unique: std::collections::BTreeSet<_> = CheckName::EXPECTED.iter().collect();
        assert_eq!(unique.len(), CheckName::EXPECTED.len());
    }

    #[test]
    fn every_check_name_has_a_distinct_stable_string() {
        let strings: std::collections::BTreeSet<&str> = CheckName::EXPECTED
            .iter()
            .map(|name| name.as_str())
            .collect();
        assert_eq!(strings.len(), CheckName::EXPECTED.len());
    }

    /// The consistency test itself: an outcome list missing one expected entry is
    /// named, not silently accepted.
    #[test]
    fn missing_checks_names_an_unregistered_entry() {
        let mut outcomes: Vec<CheckOutcome> = CheckName::EXPECTED
            .iter()
            .map(|name| CheckOutcome {
                name: *name,
                owner_module: "test",
                condition: "test condition",
                has_repair: false,
                status: CheckStatus::Pass,
            })
            .collect();
        // Planted negative: drop one entry and prove it is named, not swallowed.
        outcomes.retain(|o| o.name != CheckName::ClockSkew);
        let missing = missing_checks(&outcomes);
        assert_eq!(missing, vec![CheckName::ClockSkew]);
    }

    #[test]
    fn a_complete_registry_reports_nothing_missing() {
        let outcomes: Vec<CheckOutcome> = CheckName::EXPECTED
            .iter()
            .map(|name| CheckOutcome {
                name: *name,
                owner_module: "test",
                condition: "test condition",
                has_repair: false,
                status: CheckStatus::Pass,
            })
            .collect();
        assert!(missing_checks(&outcomes).is_empty());
    }

    #[test]
    fn not_yet_available_entry_names_its_owning_bead() {
        let outcome = CheckOutcome {
            name: CheckName::UnmappedAccounts,
            owner_module: "attribution",
            condition: "every observed account maps to a configured one",
            has_repair: false,
            status: CheckStatus::NotYetAvailable {
                owning_bead: "aub-mgv.3",
            },
        };
        match outcome.status {
            CheckStatus::NotYetAvailable { owning_bead } => assert_eq!(owning_bead, "aub-mgv.3"),
            // Named rather than a wildcard: the crate denies a catch-all over an enum, so a
            // status added later fails this assertion instead of being folded into the panic.
            CheckStatus::Pass
            | CheckStatus::PassWithDetail(_)
            | CheckStatus::Fail(_)
            | CheckStatus::NotApplicable(_) => {
                panic!("expected not-yet-available")
            }
        }
    }

    fn test_metadata() -> crate::report::ReportMetadata {
        let ts = crate::domain::time::UtcTimestamp::from_unix_nanos(0);
        crate::report::ReportMetadata::new(ts, ts, crate::report::LedgerGeneration::new(0), None)
    }

    #[test]
    fn report_counts_split_by_status() {
        let report = DoctorReport {
            metadata: test_metadata(),
            outcomes: vec![
                CheckOutcome {
                    name: CheckName::ConfigurationValidity,
                    owner_module: "config",
                    condition: "c",
                    has_repair: false,
                    status: CheckStatus::Pass,
                },
                CheckOutcome {
                    name: CheckName::ClockSkew,
                    owner_module: "doctor",
                    condition: "c",
                    has_repair: false,
                    status: CheckStatus::Fail("drift".to_string()),
                },
                CheckOutcome {
                    name: CheckName::BackupAge,
                    owner_module: "doctor",
                    condition: "c",
                    has_repair: false,
                    status: CheckStatus::NotApplicable("no destination".to_string()),
                },
                CheckOutcome {
                    name: CheckName::UnmappedAccounts,
                    owner_module: "attribution",
                    condition: "c",
                    has_repair: false,
                    status: CheckStatus::NotYetAvailable {
                        owning_bead: "aub-mgv.3",
                    },
                },
            ],
        };
        assert_eq!(report.passed(), 1);
        assert_eq!(report.failed(), 1);
        assert_eq!(report.not_applicable(), 1);
        assert_eq!(report.not_yet_available(), 1);
    }
}
