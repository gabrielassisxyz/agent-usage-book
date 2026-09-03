//! Typed report models.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

pub mod models;
pub mod provenance;

pub mod export;
pub mod spend;

pub use models::{
    BackupReport, CalibrateReport, CoverageReport, DoctorReport, ExportReport, IngestReport,
    IngestSummary, IngestionGeneration, LedgerGeneration, MeterAccount, MeterReadingProvenance,
    NowReport, ReportMetadata, SampleAttempt, SampleReport, SpendGroup, SpendGroupProvenance,
    SpendReport, StatusReport, TaskReport,
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
