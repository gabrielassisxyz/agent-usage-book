// A parser must not reach a cost model, a calibration, a rate card, task history,
// or a meter observation: the trait's output is a normalized usage event, and a
// parser implementation cannot return anything else from `parse`. Returning a raw
// usage vector (let alone a priced or calibrated quantity) is a compile error.

use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality};
use agent_usage_book::transcripts::parser::{
    InputFormatVersion, ParseOutput, ParserAdapter, ParserVersion, SourceLocation,
};

struct BadParser;

impl ParserAdapter for BadParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("bad")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("bad")
    }

    fn parse(&self, _input: &str, _location: &SourceLocation) -> ParseOutput {
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(1),
                OutputTokens::new(1),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            std::collections::BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        )
    }
}

fn main() {}
