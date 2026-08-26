//! Typed report models.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

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
