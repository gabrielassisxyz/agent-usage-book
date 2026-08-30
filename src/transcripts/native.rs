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
//! A key inside the usage object that no parser recognises is an unknown token
//! component only when its value is a non-negative integer. The real sources
//! write strings, objects and arrays beside the counts (`service_tier`,
//! `server_tool_use`, `iterations`, a nested `cost`), and none of those is a count:
//! they are ignored, while a known key with a wrong type still quarantines.
//!
//! Every source writes a record timestamp and a session identifier, and both are
//! passed through on the event, because a report that groups by day or by session
//! has nothing else to group on.
//!
//! May not depend on:
//! - calibration, cost models, rate cards, task history, or meter observations

use std::collections::BTreeMap;

use serde_json::Value;

use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    TokenKind, UsageVector,
};
use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
use crate::transcripts::parser::{
    EvidenceClassification, FixtureCoverage, FixtureShape, InputFormatVersion,
    NormalizedUsageEvent, ParseOutput, ParserAdapter, ParserVersion, QuarantineClass,
    QuarantineRecord, STRONG_IDENTITY_PREFIX, SourceLocation,
};

/// The source namespaces the three parsers attribute sessions under. One
/// definition each, so a session join never sees two spellings of one source.
pub const CLAUDE_CODE_NAMESPACE: &str = "claude-code";
pub const CODEX_NAMESPACE: &str = "codex";
pub const PI_NAMESPACE: &str = "pi";

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
    // Enumerated rather than left to a wildcard: serde_json::Value is not
    // #[non_exhaustive], so a variant added upstream fails to compile here
    // instead of silently classifying as the wrong field type.
    match value {
        Value::Number(n) => n.as_u64().ok_or(QuarantineClass::WrongFieldType),
        Value::Null | Value::Bool(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            Err(QuarantineClass::WrongFieldType)
        }
    }
}

/// An unrecognised key's value as a count, when it is one. A string, object,
/// array, boolean, null or non-integer number under a key no parser knows is
/// not a token component and is ignored; only a non-negative integer survives
/// into the unknown map.
fn unknown_count(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::Null | Value::Bool(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            None
        }
    }
}

/// Extracts the four known kinds and the unknown components from a usage
/// object.
///
/// `known` maps field names to token kinds; `ignored` names fields that are
/// recognised but are not token kinds (a nested breakdown or a derived total);
/// `required` names fields that must be present, whose absence is a missing
/// field rather than a zero. Any other non-negative integer field is an unknown
/// usage component and survives in the unknown map under its reported key.
fn extract_usage(
    usage: &serde_json::Map<String, Value>,
    known: &[(&str, TokenKind)],
    ignored: &[&str],
    required: &[&str],
) -> Result<UsageCounts, QuarantineClass> {
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
                if let Some(count) = unknown_count(value) {
                    unknown.insert(key.clone(), TokenCount::new(count));
                }
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

/// The record-level context every source writes beside its counts: the record
/// timestamp, the session the record belongs to, and the stable event identifier
/// where the source has one. Each is optional so an absent value stays absent.
struct RecordContext<'a> {
    event_id: Option<&'a str>,
    occurred_at: Option<UtcTimestamp>,
    session: Option<SessionId>,
}

/// Builds a measured event from the four kinds, unknown components, the source
/// file, and the record context.
fn event(
    usage: UsageVector,
    file: &str,
    context: RecordContext<'_>,
    parser_version: ParserVersion,
) -> NormalizedUsageEvent {
    let mut sources = vec![file.to_string()];
    if let Some(id) = context.event_id {
        sources.push(format!("{STRONG_IDENTITY_PREFIX}{id}"));
    }
    let mut event = NormalizedUsageEvent::new(
        usage,
        EvidenceClassification::Reported,
        Provenance::new(sources),
        parser_version,
    );
    if let Some(occurred_at) = context.occurred_at {
        event = event.with_occurred_at(occurred_at);
    }
    if let Some(session) = context.session {
        event = event.with_session(session);
    }
    event
}

/// A record's top-level `timestamp`, parsed when it is an RFC 3339 string.
fn record_timestamp(value: &Value) -> Option<UtcTimestamp> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(UtcTimestamp::parse_rfc3339)
}

fn session_id(namespace: &str, native: &str) -> SessionId {
    SessionId::new(
        SourceNamespace::new(namespace),
        NativeSessionId::new(native),
    )
}

/// The Claude Code transcript parser.
///
/// Reads `message.usage.{input_tokens, output_tokens, cache_read_input_tokens,
/// cache_creation_input_tokens}` and passes `message.id` through as the stable
/// event identifier. The `cache_creation` ephemeral breakdown is a sub-detail
/// of the cache write, not a separate kind; it is used only as a fallback when
/// the total is absent.
/// The four headline counts plus the per-kind breakdown a native-usage line
/// yields. Named because the tuple appears in three signatures and clippy
/// refuses a type this wide repeated inline.
type UsageCounts = (u64, u64, u64, u64, BTreeMap<String, TokenCount>);

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
    let context = RecordContext {
        event_id: message.get("id").and_then(Value::as_str),
        occurred_at: record_timestamp(&value),
        session: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(|native| session_id(CLAUDE_CODE_NAMESPACE, native)),
    };
    Ok(Some(event(
        measured_usage(input, output, cache_read, cache_write, unknown),
        location.file(),
        context,
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
/// the last `token_count` record in the file, stamped with that record's
/// timestamp and attributed to the session the file's `session_meta` header
/// names. A `token_count` whose `info` is null carries only a rate-limit update
/// and is neither an event nor a quarantine. Codex provides no stable per-event
/// identifier, so no strong dedup identity is reported.
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
        let mut last: Option<(UsageCounts, Option<UtcTimestamp>)> = None;
        let mut session: Option<SessionId> = None;
        let mut quarantined = Vec::new();
        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_codex_line(line) {
                Ok(CodexLine::Usage(usage, occurred_at)) => last = Some((usage, occurred_at)),
                Ok(CodexLine::Session(id)) => session = Some(id),
                Ok(CodexLine::Nothing) => {}
                Err(class) => quarantined.push(QuarantineRecord::new(
                    record_location,
                    self.parser_version(),
                    class,
                )),
            }
        }
        let events = last
            .map(
                |((input, output, cache_read, cache_write, unknown), occurred_at)| {
                    event(
                        measured_usage(input, output, cache_read, cache_write, unknown),
                        location.file(),
                        RecordContext {
                            event_id: None,
                            occurred_at,
                            session,
                        },
                        self.parser_version(),
                    )
                },
            )
            .into_iter()
            .collect();
        ParseOutput::new(events, quarantined)
    }
}

/// What one Codex line contributes: a cumulative usage record, the session
/// header, or nothing this parser reads.
enum CodexLine {
    Usage(UsageCounts, Option<UtcTimestamp>),
    Session(SessionId),
    Nothing,
}

fn parse_codex_line(line: &str) -> Result<CodexLine, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return Ok(CodexLine::Nothing);
    };
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        return Ok(payload
            .get("id")
            .and_then(Value::as_str)
            .map(|native| CodexLine::Session(session_id(CODEX_NAMESPACE, native)))
            .unwrap_or(CodexLine::Nothing));
    }
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return Ok(CodexLine::Nothing);
    }
    let usage = match payload.get("info") {
        // A rate-limit-only update: Codex writes `info: null` when nothing was
        // consumed, and a record with no usage is not a malformed record.
        None | Some(Value::Null) => return Ok(CodexLine::Nothing),
        Some(info) => info
            .get("total_token_usage")
            .and_then(Value::as_object)
            .ok_or(QuarantineClass::MissingRequiredField)?,
    };
    let counts = extract_usage(usage, &CODEX_KNOWN, &CODEX_IGNORED, &["input_tokens"])?;
    Ok(CodexLine::Usage(counts, record_timestamp(&value)))
}

/// The pi transcript parser.
///
/// Reads `message.usage.{input, output, cacheRead, cacheWrite}` and passes the
/// record's stable identifier through: pi writes it at the top level as `id`,
/// and `message.id` is honoured first where a record carries one. The record's
/// top-level `timestamp` is the event time, and the session comes from the
/// `{"type":"session","id":...}` header line. `reasoning` is a token class
/// outside the four known kinds and survives in the unknown map; the `cost`
/// object nested inside `usage` is money, not usage, and is ignored.
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
        let mut session: Option<SessionId> = None;
        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_pi_line(
                line,
                &record_location,
                session.clone(),
                self.parser_version(),
            ) {
                Ok(PiLine::Usage(event)) => events.push(*event),
                Ok(PiLine::Session(id)) => session = Some(id),
                Ok(PiLine::Nothing) => {}
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

/// What one pi line contributes: a usage event, the session header, or nothing
/// this parser reads.
enum PiLine {
    Usage(Box<NormalizedUsageEvent>),
    Session(SessionId),
    Nothing,
}

fn parse_pi_line(
    line: &str,
    location: &SourceLocation,
    session: Option<SessionId>,
    parser_version: ParserVersion,
) -> Result<PiLine, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    if value.get("type").and_then(Value::as_str) == Some("session") {
        return Ok(value
            .get("id")
            .and_then(Value::as_str)
            .map(|native| PiLine::Session(session_id(PI_NAMESPACE, native)))
            .unwrap_or(PiLine::Nothing));
    }
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return Ok(PiLine::Nothing);
    };
    let Some(usage) = message.get("usage").and_then(Value::as_object) else {
        return Ok(PiLine::Nothing);
    };
    let (input, output, cache_read, cache_write, unknown) =
        extract_usage(usage, &PI_KNOWN, &PI_IGNORED, &["input"])?;
    let context = RecordContext {
        event_id: message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("id").and_then(Value::as_str)),
        occurred_at: record_timestamp(&value),
        session,
    };
    Ok(PiLine::Usage(Box::new(event(
        measured_usage(input, output, cache_read, cache_write, unknown),
        location.file(),
        context,
        parser_version,
    ))))
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
        let cases: [(&str, &dyn ParserAdapter, usize, usize); 11] = [
            ("claude-real-shape.jsonl", &ClaudeCodeParser, 3, 0),
            ("codex-real-shape.jsonl", &CodexParser, 1, 0),
            ("pi-real-shape.jsonl", &PiParser, 2, 0),
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
                .any(|s| s.starts_with(STRONG_IDENTITY_PREFIX)),
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

    /// The real Claude Code shape: strings, objects and an array inside `usage`
    /// beside the four counts. The counts survive, the non-counts are ignored,
    /// and the record's timestamp and session are carried on the event.
    #[test]
    fn claude_real_shape_keeps_the_counts_and_ignores_non_count_fields() {
        let parser = ClaudeCodeParser;
        let output = parser.parse(
            &read_fixture("claude-real-shape.jsonl"),
            &SourceLocation::new("claude-real-shape.jsonl", 1),
        );
        assert!(
            output.quarantined().is_empty(),
            "{:?}",
            output.quarantined()
        );
        let first = &output.events()[0];
        let known = first.usage().known();
        assert_eq!(known.input().value(), 2);
        assert_eq!(known.cache_write().value(), 30_011);
        assert_eq!(known.cache_read().value(), 26_503);
        assert_eq!(known.output().value(), 913);
        assert!(
            first.usage().unknown().is_empty(),
            "no string or object is a token component: {:?}",
            first.usage().unknown()
        );
        assert_eq!(first.strong_identity(), Some("msg_real_0001"));
        assert_eq!(
            first.occurred_at(),
            UtcTimestamp::parse_rfc3339("2026-08-25T17:43:19.599Z")
        );
        assert_eq!(
            first.session(),
            Some(&session_id(CLAUDE_CODE_NAMESPACE, "session-real-0001"))
        );
    }

    /// The planted negative pair: an unrecognised key with an integer value is an
    /// unknown component; the same key with a string value is not a component
    /// and is ignored, while a known key with a string value still quarantines.
    #[test]
    fn only_integer_values_under_unknown_keys_become_components() {
        let parser = ClaudeCodeParser;
        let integer = r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"future_tokens":99}}}"#;
        let string = r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"future_tokens":"99"}}}"#;
        let known_wrong =
            r#"{"message":{"id":"m1","usage":{"input_tokens":"10","output_tokens":5}}}"#;
        let with_integer = parser.parse(integer, &location());
        assert_eq!(
            with_integer.events()[0]
                .usage()
                .unknown()
                .get("future_tokens")
                .map(|c| c.value()),
            Some(99)
        );
        let with_string = parser.parse(string, &location());
        assert_eq!(with_string.events().len(), 1);
        assert!(with_string.events()[0].usage().unknown().is_empty());
        let wrong = parser.parse(known_wrong, &location());
        assert!(wrong.events().is_empty());
        assert_eq!(
            wrong.quarantined()[0].class(),
            QuarantineClass::WrongFieldType
        );
    }

    /// The real pi shape: the identifier and the timestamp at the top level, a
    /// `cost` object nested inside `usage`, and the session from the header line.
    #[test]
    fn pi_real_shape_takes_identity_time_and_session_from_where_pi_writes_them() {
        let parser = PiParser;
        let output = parser.parse(
            &read_fixture("pi-real-shape.jsonl"),
            &SourceLocation::new("pi-real-shape.jsonl", 1),
        );
        assert!(
            output.quarantined().is_empty(),
            "{:?}",
            output.quarantined()
        );
        assert_eq!(output.events().len(), 2);
        let first = &output.events()[0];
        assert_eq!(first.strong_identity(), Some("rec-real-0001"));
        assert_eq!(first.usage().known().input().value(), 19_221);
        assert_eq!(
            first.usage().unknown().get("reasoning").map(|c| c.value()),
            Some(202)
        );
        assert_eq!(
            first.occurred_at(),
            UtcTimestamp::parse_rfc3339("2026-08-25T23:33:39.627Z")
        );
        assert_eq!(
            first.session(),
            Some(&session_id(PI_NAMESPACE, "session-real-pi-0001"))
        );
        // `message.id` still wins where a record carries one.
        let explicit = parser.parse(
            r#"{"id":"top","message":{"id":"inner","usage":{"input":1,"output":1}}}"#,
            &location(),
        );
        assert_eq!(explicit.events()[0].strong_identity(), Some("inner"));
    }

    /// The real Codex shape: a `session_meta` header, a rate-limit-only
    /// `token_count` with null info, then cumulative records. One event, no
    /// quarantine, the last record's timestamp, the header's session.
    #[test]
    fn codex_real_shape_skips_null_info_and_keeps_the_last_record() {
        let parser = CodexParser;
        let output = parser.parse(
            &read_fixture("codex-real-shape.jsonl"),
            &SourceLocation::new("codex-real-shape.jsonl", 1),
        );
        assert!(
            output.quarantined().is_empty(),
            "{:?}",
            output.quarantined()
        );
        assert_eq!(output.events().len(), 1);
        let only = &output.events()[0];
        assert_eq!(only.usage().known().input().value(), 34_830);
        assert_eq!(only.usage().known().cache_read().value(), 19_200);
        assert_eq!(
            only.occurred_at(),
            UtcTimestamp::parse_rfc3339("2026-08-25T14:33:10.001Z")
        );
        assert_eq!(
            only.session(),
            Some(&session_id(CODEX_NAMESPACE, "session-real-codex-0001"))
        );
        // A file holding only the null-info record is neither an event nor a quarantine.
        let null_only = parser.parse(
            r#"{"timestamp":"2026-08-25T14:31:33.849Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{}}}"#,
            &location(),
        );
        assert!(null_only.events().is_empty());
        assert!(null_only.quarantined().is_empty());
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
