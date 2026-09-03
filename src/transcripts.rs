//! Recursive discovery and source-specific parsers.
//!
//! May not depend on:
//! - calibration
//! - cost models
//! - subscription window capacity, API pricing, task history, or meter percentages

pub mod discovery;
pub mod drift;
pub mod native;
pub mod parser;
pub mod watermark;

pub use discovery::{DiscoveryError, DiscoveryOptions, SourceDiscovery, discover};
pub use drift::{
    DEFAULT_FIXTURE_DIR, FIXTURE_CAPTURE_PROCEDURE_DOC, FixtureCorpusShape, RecordShape,
    ShapeSummary, SourceDriftReport, TranscriptDriftReport, ValueType, collect_field_paths,
    detect_drift, extract_record_kind, extract_record_shape, load_fixture_corpus,
};
pub use native::{ClaudeCodeParser, CodexParser, PiParser};

/// The parser for a source's declared `format`, or `None` for a format no parser
/// reads. The three names are the configuration vocabulary; a report over a source
/// with an unknown format refuses rather than guessing a parser from a path.
pub fn parser_for_format(format: &str) -> Option<Box<dyn ParserAdapter>> {
    match format {
        "claude-code" => Some(Box::new(ClaudeCodeParser)),
        "codex" => Some(Box::new(CodexParser)),
        "pi" => Some(Box::new(PiParser)),
        _ => None,
    }
}
pub use parser::{
    EstimatorVersion, EvidenceClassification, FIXTURE_CATALOG, FixtureCoverage, FixtureShape,
    InputFormatVersion, MutationExpectation, MutationKind, NormalizedUsageEvent, ParseOutput,
    ParserAdapter, ParserVersion, QuarantineClass, QuarantineRecord, SourceLocation,
    assert_mutation_outcome, verify_fixture_coverage,
};
pub use watermark::{ChangeClass, FileState, Watermark, classify, last_complete_line_offset};
