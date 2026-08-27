//! Native-usage transcript parsers: Claude Code, Codex, and pi.
//!
//! These three sources all report provider- or CLI-measured token counts, so
//! everything this module emits is classified [`EvidenceClassification::Reported`].
//! The estimated-source parser (`aub-lqe.5`) owns reconstruction; this module never
//! estimates.
//!
//! The three field vocabularies do not agree, and the differences are not
//! cosmetic: pi reports `reasoning` separately and Claude Code does not, Codex
//! splits cache into read and write while pi names them `cacheRead` and
//! `cacheWrite`, and Claude Code alone breaks cache creation down by ephemeral
//! lifetime. A normalisation that silently drops a field it does not recognise
//! understates the source it least understands, so every token class a source
//! reports is either mapped to one of the four known kinds or preserved in the
//! usage vector's unknown-component map. The one deliberate exception is the
//! total: `total_tokens` / `totalTokens` is a derived sum, not a token class,
//! and the token model deliberately has no total slot (see `domain::tokens`).
//!
//! Codex reports cumulatively, not per-delta: each `token_count` record carries
//! the running total for the session, so summing them multiplies the real
//! figure. This parser takes the last `token_count` record per file, which is
//! what the legacy tooling already does.
//!
//! May not depend on:
//! - calibration, cost models, rate cards, task history, or meter observations

use std::collections::BTreeMap;

use serde_json::Value;

use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    TokenKind, UsageVector,
};
use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use crate::transcripts::parser::{
    EvidenceClassification, FixtureCoverage, FixtureShape, InputFormatVersion,
    NormalizedUsageEvent, ParseOutput, ParserAdapter, ParserVersion, QuarantineClass,
    QuarantineRecord, SourceLocation,
};

/// The provenance prefix for a stable native event identifier. The dedup
/// framework treats a provenance entry with this prefix as strong identity
/// rather than a heuristic fingerprint.
const EVENT_ID_PREFIX: &str = "event-id:";

/// The fixture directory for this parser, relative to the crate root.
pub const FIXTURE_DIR: &str = "tests/fixtures/transcripts/native";

/// A measured usage vector: the four known kinds plus any unknown components.
fn measured_usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    unknown: BTreeMap<String, TokenCount>,
) -> UsageVector {
    UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(cache_write),
        ),
        unknown,
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    )
}

/// A non-negative integer count from a JSON value, or the class for a value
/// that is not one. A float or negative number is a wrong type: token counts
/// are integers.
fn count_value(value: &Value) -> Result<u64, QuarantineClass> {
    match value {
        Value::Number(n) => n.as_u64().ok_or(QuarantineClass::WrongFieldType),
        _ => Err(QuarantineClass::WrongFieldType),
    }
}

/// Extracts the four known kinds and the unknown components from a usage
/// object.
///
/// `known` maps field names to token kinds; `ignored` names fields that are
/// recognised but are not token kinds (a nested breakdown or a derived total);
/// `required` names fields that must be present, whose absence is a missing
/// field rather than a zero. Any other numeric field is an unknown usage
/// component and survives in the unknown map under its reported key.
fn extract_usage(
    usage: &serde_json::Map<String, Value>,
    known: &[(&str, TokenKind)],
    ignored: &[&str],
    required: &[&str],
) -> Result<(u64, u64, u64, u64, BTreeMap<String, TokenCount>), QuarantineClass> {
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut unknown = BTreeMap::new();

    for (key, value) in usage {
        let kind = known.iter().find(|(name, _)| *name == key).map(|(_, k)| *k);
        match kind {
            Some(TokenKind::Input) => input = count_value(value)?,
            Some(TokenKind::Output) => output = count_value(value)?,
            Some(TokenKind::CacheRead) => cache_read = count_value(value)?,
            Some(TokenKind::CacheWrite) => cache_write = count_value(value)?,
            None if ignored.contains(&key.as_str()) => {}
            None => {
                unknown.insert(key.clone(), TokenCount::new(count_value(value)?));
            }
        }
    }

    for field in required {
        if !usage.contains_key(*field) {
            return Err(QuarantineClass::MissingRequiredField);
        }
    }

    Ok((input, output, cache_read, cache_write, unknown))
}

/// Builds a measured event from the four kinds, unknown components, the source
/// file, and an optional stable event identifier.
fn event(
    usage: UsageVector,
    file: &str,
    event_id: Option<&str>,
    parser_version: ParserVersion,
) -> NormalizedUsageEvent {
    let mut sources = vec![file.to_string()];
    if let Some(id) = event_id {
        sources.push(format!("{EVENT_ID_PREFIX}{id}"));
    }
    NormalizedUsageEvent::new(
        usage,
        EvidenceClassification::Reported,
        Provenance::new(sources),
        parser_version,
    )
}

/// The Claude Code transcript parser.
///
/// Reads `message.usage.{input_tokens, output_tokens, cache_read_input_tokens,
/// cache_creation_input_tokens}` and passes `message.id` through as the stable
/// event identifier. The `cache_creation` ephemeral breakdown is a sub-detail
/// of the cache write, not a separate kind; it is used only as a fallback when
/// the total is absent.
pub struct ClaudeCodeParser;

const CLAUDE_KNOWN: [(&str, TokenKind); 4] = [
    ("input_tokens", TokenKind::Input),
    ("output_tokens", TokenKind::Output),
    ("cache_read_input_tokens", TokenKind::CacheRead),
    ("cache_creation_input_tokens", TokenKind::CacheWrite),
];
const CLAUDE_IGNORED: [&str; 1] = ["cache_creation"];

impl ParserAdapter for ClaudeCodeParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("claude-code-1")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("claude-code-jsonl-v1")
    }

    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        let mut events = Vec::new();
        let mut quarantined = Vec::new();
        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_claude_line(line, &record_location, self.parser_version()) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(class) => quarantined.push(QuarantineRecord::new(
                    record_location,
                    self.parser_version(),
                    class,
                )),
            }
        }
        ParseOutput::new(events, quarantined)
    }
}

fn parse_claude_line(
    line: &str,
    location: &SourceLocation,
    parser_version: ParserVersion,
) -> Result<Option<NormalizedUsageEvent>, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(usage) = message.get("usage").and_then(Value::as_object) else {
        return Ok(None);
    };
    let (input, output, cache_read, mut cache_write, unknown) =
        extract_usage(usage, &CLAUDE_KNOWN, &CLAUDE_IGNORED, &["input_tokens"])?;
    if !usage.contains_key("cache_creation_input_tokens") {
        cache_write = claude_cache_write_fallback(usage)?;
    }
    let event_id = message.get("id").and_then(Value::as_str);
    Ok(Some(event(
        measured_usage(input, output, cache_read, cache_write, unknown),
        location.file(),
        event_id,
        parser_version,
    )))
}

/// The cache write when the total is absent: the sum of the ephemeral
/// breakdown, which is the only place the figure then lives.
fn claude_cache_write_fallback(
    usage: &serde_json::Map<String, Value>,
) -> Result<u64, QuarantineClass> {
    let Some(breakdown) = usage.get("cache_creation").and_then(Value::as_object) else {
        return Ok(0);
    };
    let mut sum = 0u64;
    for key in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
        if let Some(value) = breakdown.get(key) {
            sum += count_value(value)?;
        }
    }
    Ok(sum)
}

/// The Codex transcript parser.
///
/// Reads `payload.info.total_token_usage.{input_tokens, cached_input_tokens,
/// cache_write_input_tokens, output_tokens}` on records of payload type
/// `token_count`. Codex reports cumulatively, so this parser emits one event:
/// the last `token_count` record in the file. Codex provides no stable
/// per-event identifier, so no strong dedup identity is reported.
pub struct CodexParser;

const CODEX_KNOWN: [(&str, TokenKind); 4] = [
    ("input_tokens", TokenKind::Input),
    ("output_tokens", TokenKind::Output),
    ("cached_input_tokens", TokenKind::CacheRead),
    ("cache_write_input_tokens", TokenKind::CacheWrite),
];
const CODEX_IGNORED: [&str; 1] = ["total_tokens"];

impl ParserAdapter for CodexParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("codex-1")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("codex-jsonl-v1")
    }

    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        let mut last: Option<(u64, u64, u64, u64, BTreeMap<String, TokenCount>)> = None;
        let mut quarantined = Vec::new();
        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_codex_line(line) {
                Ok(Some(usage)) => last = Some(usage),
                Ok(None) => {}
                Err(class) => quarantined.push(QuarantineRecord::new(
                    record_location,
                    self.parser_version(),
                    class,
                )),
            }
        }
        let events = last
            .map(|(input, output, cache_read, cache_write, unknown)| {
                event(
                    measured_usage(input, output, cache_read, cache_write, unknown),
                    location.file(),
                    None,
                    self.parser_version(),
                )
            })
            .into_iter()
            .collect();
        ParseOutput::new(events, quarantined)
    }
}

fn parse_codex_line(
    line: &str,
) -> Result<Option<(u64, u64, u64, u64, BTreeMap<String, TokenCount>)>, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return Ok(None);
    };
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return Ok(None);
    }
    let Some(usage) = payload
        .get("info")
        .and_then(|info| info.get("total_token_usage"))
        .and_then(Value::as_object)
    else {
        return Err(QuarantineClass::MissingRequiredField);
    };
    extract_usage(usage, &CODEX_KNOWN, &CODEX_IGNORED, &["input_tokens"]).map(Some)
}

/// The pi transcript parser.
///
/// Reads `message.usage.{input, output, cacheRead, cacheWrite}` and passes
/// `message.id` through as the stable event identifier. `reasoning` is a token
/// class outside the four known kinds and survives in the unknown map; the
/// `cost` object is not usage and is ignored.
pub struct PiParser;

const PI_KNOWN: [(&str, TokenKind); 4] = [
    ("input", TokenKind::Input),
    ("output", TokenKind::Output),
    ("cacheRead", TokenKind::CacheRead),
    ("cacheWrite", TokenKind::CacheWrite),
];
const PI_IGNORED: [&str; 1] = ["totalTokens"];

impl ParserAdapter for PiParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("pi-1")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("pi-jsonl-v1")
    }

    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        let mut events = Vec::new();
        let mut quarantined = Vec::new();
        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_pi_line(line, &record_location, self.parser_version()) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(class) => quarantined.push(QuarantineRecord::new(
                    record_location,
                    self.parser_version(),
                    class,
                )),
            }
        }
        ParseOutput::new(events, quarantined)
    }
}

fn parse_pi_line(
    line: &str,
    location: &SourceLocation,
    parser_version: ParserVersion,
) -> Result<Option<NormalizedUsageEvent>, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(usage) = message.get("usage").and_then(Value::as_object) else {
        return Ok(None);
    };
    let (input, output, cache_read, cache_write, unknown) =
        extract_usage(usage, &PI_KNOWN, &PI_IGNORED, &["input"])?;
    let event_id = message.get("id").and_then(Value::as_str);
    Ok(Some(event(
        measured_usage(input, output, cache_read, cache_write, unknown),
        location.file(),
        event_id,
        parser_version,
    )))
}

/// The declared fixture coverage: one entry per catalog shape, so a shape added
/// to the contract fails the golden test until a fixture (or a rationale)
/// exists here.
pub fn fixture_coverage() -> BTreeMap<FixtureShape, FixtureCoverage> {
    let applicable = |fixture: &str| FixtureCoverage::Applicable {
        fixture: fixture.to_string(),
    };
    BTreeMap::from([
        (
            FixtureShape::SimpleSession,
            applicable("claude-simple-session.jsonl"),
        ),
        (
            FixtureShape::NestedSubagentPaths,
            applicable("claude-nested-subagent.jsonl"),
        ),
        (
            FixtureShape::TruncatedFile,
            applicable("codex-truncated.jsonl"),
        ),
        (
            FixtureShape::PartiallyWrittenFinalRecord,
            applicable("pi-partial-final.jsonl"),
        ),
        (
            FixtureShape::FileRotation,
            FixtureCoverage::NotApplicable {
                rationale: "file rotation is a discovery and ingestion concern; the parser \
                            normalizes one file's content and never sees the rotation boundary"
                    .to_string(),
            },
        ),
        (
            FixtureShape::MalformedRecords,
            applicable("claude-malformed.jsonl"),
        ),
        (
            FixtureShape::ModelChangeMidSession,
            applicable("pi-model-change.jsonl"),
        ),
        (
            FixtureShape::CacheReadsAndWrites,
            applicable("codex-cache.jsonl"),
        ),
        (
            FixtureShape::NoNativeUsageField,
            applicable("pi-no-usage.jsonl"),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::parser::{
        FIXTURE_CATALOG, MutationExpectation, assert_mutation_outcome, verify_fixture_coverage,
    };
    use std::path::{Path, PathBuf};

    fn location() -> SourceLocation {
        SourceLocation::new("fixture.transcript", 1)
    }

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FIXTURE_DIR)
            .join(name)
    }

    fn read_fixture(name: &str) -> String {
        std::fs::read_to_string(fixture_path(name)).expect("fixture must be readable")
    }

    /// The shared mutation suite over the Claude Code adapter.
    #[test]
    fn claude_code_passes_the_shared_mutation_suite() {
        let parser = ClaudeCodeParser;
        let loc = location();
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"output_tokens":5}}}"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::MissingRequiredField),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"input_tokens":"abc","output_tokens":5}}}"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::WrongFieldType),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::TruncatedStructure),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","model":"opus","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            &loc,
            MutationExpectation::Parses,
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"future_tokens":99}}}"#,
            &loc,
            MutationExpectation::PreservesUnknownComponent {
                key: "future_tokens".to_string(),
            },
        );
    }

    /// The shared mutation suite over the Codex adapter.
    #[test]
    fn codex_passes_the_shared_mutation_suite() {
        let parser = CodexParser;
        let loc = location();
        assert_mutation_outcome(
            &parser,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":5}}}}"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::MissingRequiredField),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":"abc","output_tokens":5}}}}"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::WrongFieldType),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::TruncatedStructure),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}},"session":"s1"}}"#,
            &loc,
            MutationExpectation::Parses,
        );
        assert_mutation_outcome(
            &parser,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5,"future_tokens":99}}}}"#,
            &loc,
            MutationExpectation::PreservesUnknownComponent {
                key: "future_tokens".to_string(),
            },
        );
    }

    /// The shared mutation suite over the pi adapter.
    #[test]
    fn pi_passes_the_shared_mutation_suite() {
        let parser = PiParser;
        let loc = location();
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"output":5}}}"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::MissingRequiredField),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"input":"abc","output":5}}}"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::WrongFieldType),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"input":10,"output":5"#,
            &loc,
            MutationExpectation::Quarantines(QuarantineClass::TruncatedStructure),
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","model":"opus","usage":{"input":10,"output":5}}}"#,
            &loc,
            MutationExpectation::Parses,
        );
        assert_mutation_outcome(
            &parser,
            r#"{"message":{"id":"m1","usage":{"input":10,"output":5,"futureTokens":99}}}"#,
            &loc,
            MutationExpectation::PreservesUnknownComponent {
                key: "futureTokens".to_string(),
            },
        );
    }

    /// Every catalog shape is covered, and every applicable fixture exists on
    /// disk. A shape added to the contract fails here until a fixture (or a
    /// rationale) exists.
    #[test]
    fn every_catalog_shape_has_a_fixture_or_rationale() {
        let coverage = fixture_coverage();
        let missing = verify_fixture_coverage(&coverage);
        assert!(
            missing.is_empty(),
            "missing fixture coverage for {missing:?}"
        );
        assert_eq!(coverage.len(), FIXTURE_CATALOG.len());

        for (shape, cov) in &coverage {
            if let FixtureCoverage::Applicable { fixture } = cov {
                assert!(
                    fixture_path(fixture).exists(),
                    "fixture {fixture} for {shape:?} does not exist"
                );
            }
        }
    }

    /// Each fixture parses to its golden output: the expected event and
    /// quarantine counts.
    #[test]
    fn each_fixture_parses_to_its_golden_output() {
        let cases: [(&str, &dyn ParserAdapter, usize, usize); 8] = [
            ("claude-simple-session.jsonl", &ClaudeCodeParser, 2, 0),
            ("claude-nested-subagent.jsonl", &ClaudeCodeParser, 1, 0),
            ("codex-truncated.jsonl", &CodexParser, 1, 1),
            ("pi-partial-final.jsonl", &PiParser, 1, 1),
            ("claude-malformed.jsonl", &ClaudeCodeParser, 1, 1),
            ("pi-model-change.jsonl", &PiParser, 2, 0),
            ("codex-cache.jsonl", &CodexParser, 1, 0),
            ("pi-no-usage.jsonl", &PiParser, 0, 0),
        ];
        for (fixture, parser, events, quarantined) in cases {
            let output = parser.parse(&read_fixture(fixture), &SourceLocation::new(fixture, 1));
            assert_eq!(output.events().len(), events, "fixture {fixture}");
            assert_eq!(output.quarantined().len(), quarantined, "fixture {fixture}");
        }
    }

    /// Cache read and cache write are their own kinds and are never folded into
    /// input or output.
    #[test]
    fn cache_read_and_write_are_their_own_kinds() {
        let parser = ClaudeCodeParser;
        let input = r#"{"message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}}}"#;
        let output = parser.parse(input, &location());
        let known = output.events()[0].usage().known();
        assert_eq!(
            known.input().value(),
            100,
            "cache read must not fold into input"
        );
        assert_eq!(
            known.output().value(),
            50,
            "cache write must not fold into output"
        );
        assert_eq!(known.cache_read().value(), 20, "cache read is its own kind");
        assert_eq!(
            known.cache_write().value(),
            10,
            "cache write is its own kind"
        );
    }

    /// Native-usage sources are measured: every event is classified reported,
    /// never reconstructed or derived.
    #[test]
    fn the_parser_classifies_everything_as_reported() {
        let parser = ClaudeCodeParser;
        let input = r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let output = parser.parse(input, &location());
        assert_eq!(
            output.events()[0].classification(),
            &EvidenceClassification::Reported,
            "native-usage sources are measured, never reconstructed or derived"
        );
    }

    /// A stable native event identifier is passed through in the provenance.
    #[test]
    fn a_stable_native_event_identifier_is_passed_through() {
        let parser = ClaudeCodeParser;
        let input =
            r#"{"message":{"id":"msg_abc123","usage":{"input_tokens":10,"output_tokens":5}}}"#;
        let output = parser.parse(input, &location());
        let event = &output.events()[0];
        assert!(
            event.provenance().sources().contains("event-id:msg_abc123"),
            "the stable event identifier must be passed through in the provenance"
        );
    }

    /// A source without a stable event identifier reports no strong identity:
    /// Codex's cumulative records carry no per-event id, so its provenance has
    /// no `event-id:` entry.
    #[test]
    fn a_source_without_an_event_id_reports_no_strong_identity() {
        let parser = CodexParser;
        let input = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#;
        let output = parser.parse(input, &location());
        let event = &output.events()[0];
        assert!(
            !event
                .provenance()
                .sources()
                .iter()
                .any(|s| s.starts_with(EVENT_ID_PREFIX)),
            "Codex provides no stable event identifier, so no strong identity is reported"
        );
    }

    /// A malformed record quarantines with its failure class and does not abort
    /// ingestion of the rest of the file.
    #[test]
    fn a_malformed_record_quarantines_without_aborting_the_rest() {
        let parser = ClaudeCodeParser;
        let input = concat!(
            r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            "\n",
            r#"{"message":{"id":"m2","usage":{"input_tokens":"not-a-number","output_tokens":5}}}"#,
            "\n",
            r#"{"message":{"id":"m3","usage":{"input_tokens":30,"output_tokens":15}}}"#,
        );
        let output = parser.parse(input, &location());
        assert_eq!(output.events().len(), 2, "the good records must survive");
        assert_eq!(
            output.quarantined().len(),
            1,
            "the bad record must quarantine"
        );
        assert_eq!(
            output.quarantined()[0].class(),
            QuarantineClass::WrongFieldType
        );
        assert_eq!(output.quarantined()[0].location().line(), 2);
    }

    /// Codex reports cumulatively: the parser emits the last token_count record,
    /// never the sum of all of them.
    #[test]
    fn codex_takes_the_last_cumulative_record_not_the_sum() {
        let parser = CodexParser;
        let input = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":50}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"output_tokens":100}}}}"#,
        );
        let output = parser.parse(input, &location());
        assert_eq!(output.events().len(), 1, "one cumulative record, one event");
        let known = output.events()[0].usage().known();
        assert_eq!(known.input().value(), 200, "the last record, not the sum");
        assert_eq!(known.output().value(), 100);
    }

    /// pi's `reasoning` field is a token class outside the four known kinds and
    /// survives in the unknown map rather than being dropped.
    #[test]
    fn pi_reasoning_survives_in_the_unknown_map() {
        let parser = PiParser;
        let input = r#"{"message":{"id":"m1","usage":{"input":100,"output":50,"reasoning":5}}}"#;
        let output = parser.parse(input, &location());
        let event = &output.events()[0];
        assert_eq!(
            event.usage().unknown().get("reasoning").map(|c| c.value()),
            Some(5),
            "reasoning must survive as an unknown component"
        );
    }

    /// No fixture contains a credential pattern, a personal identifier, or an
    /// absolute home path.
    #[test]
    fn no_fixture_contains_a_credential_a_personal_identifier_or_a_home_path() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
        for entry in std::fs::read_dir(&dir).expect("fixture directory must exist") {
            let path = entry.expect("fixture entry must be readable").path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).expect("fixture must be readable");
            let lower = contents.to_lowercase();
            for pattern in [
                "sk-ant-",
                "sk-",
                "api_key",
                "apikey",
                "access_token",
                "accesstoken",
                "refresh_token",
                "refreshtoken",
                "bearer",
            ] {
                assert!(
                    !lower.contains(pattern),
                    "fixture {} contains credential pattern {pattern:?}",
                    path.display()
                );
            }
            assert!(
                !lower.contains('@'),
                "fixture {} contains a personal identifier",
                path.display()
            );
            assert!(
                !lower.contains("/home/") && !lower.contains("/users/"),
                "fixture {} contains an absolute home path",
                path.display()
            );
        }
    }
}
