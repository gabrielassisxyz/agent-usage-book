//! Recursive discovery and source-specific parsers.
//!
//! May not depend on:
//! - calibration
//! - cost models
//! - subscription window capacity, API pricing, task history, or meter percentages

pub mod discovery;
pub mod parser;

pub use discovery::{DiscoveryError, DiscoveryOptions, SourceDiscovery, discover};
pub use parser::{
    EstimatorVersion, EvidenceClassification, FIXTURE_CATALOG, FixtureCoverage, FixtureShape,
    InputFormatVersion, MutationExpectation, MutationKind, NormalizedUsageEvent, ParseOutput,
    ParserAdapter, ParserVersion, QuarantineClass, QuarantineRecord, SourceLocation,
    assert_mutation_outcome, verify_fixture_coverage,
};
