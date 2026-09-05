//! Typed report models.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

pub mod models;
pub mod provenance;

pub mod activity;
pub mod calibrate;
pub mod can_run;
pub mod can_run_evidence;
pub mod coverage;
pub mod export;
pub mod spend;
pub mod task;

pub use activity::{
    ActiveActivityState, ActiveActivityStateKind, ActiveSpendClaim, InactiveSpendClaim,
    LivenessGap, compose_active_activity,
};
pub use calibrate::{
    CalibrateActivateReport, CalibrateCompareReport, CalibrateCostModelCoverage,
    CalibrateCostModelKindCoverage, CalibrateHistoryEntry, CalibrateHistoryReport,
    CalibrateLifecycleEventView, CalibrateShowEntry, CalibrateShowReport, calibrate_difference_bps,
    format_calibrate_difference_percent,
};
pub use can_run::*;
pub use can_run_evidence::gather_task_history_group_report;
pub use models::{
    AccountGroupExplain, AccountMarkerReference, BackupReport, CalibrateReport,
    ClearDiagnosticsReport, CoverageAccount, CoverageBreach, CoverageBreachDimension,
    CoverageReport, CoverageReset, CoverageThreshold, DoctorReport, ExportReport, IngestReport,
    IngestSummary, IngestionGeneration, LedgerGeneration, LimitingWindow, MeterAccount,
    MeterExplanation, MeterReadingProvenance, MeterWindowExplanation, NowReport,
    ProjectionReadState, ReportMetadata, SampleAttempt, SampleReport, SharePpm, SpendDiagnostic,
    SpendDiagnosticProvenance, SpendGroup, SpendGroupCreditsProvenance, SpendGroupProvenance,
    SpendGrouping, SpendReport, StatusReport, TaskIdentityRow, TaskIngestReport,
    TaskOverheadBucket, TaskOverheadReport, TaskReport, TaskSessionUsage, UNKNOWN_ACCOUNT_LABEL,
};
pub use provenance::{ProvenanceGraph, ProvenanceNode, ReportField, Unit, ValueArithmetic};

use crate::logging::RunId;

/// Metadata shared by every rendered report format.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReportEnvelope {
    pub run: RunId,
}

impl ReportEnvelope {
    pub fn new(run: RunId) -> Self {
        Self { run }
    }

    /// The smallest JSON report shape used until typed reports land.
    pub fn as_json(&self) -> String {
        format!("{{\"run\":\"{}\"}}", self.run.as_str())
    }
}
