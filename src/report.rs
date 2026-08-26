//! Typed report models.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

pub mod models;

pub use models::{
    BackupReport, CalibrateReport, CoverageReport, DoctorReport, ExportReport, IngestReport,
    IngestionGeneration, LedgerGeneration, MeterAccount, NowReport, ReportMetadata, SampleAttempt,
    SampleReport, SpendGroup, SpendReport, StatusReport, TaskReport,
};

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
