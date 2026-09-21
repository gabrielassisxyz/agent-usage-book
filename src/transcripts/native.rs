//! Native-usage transcript parsers: Claude Code, Codex, opencode, and pi.
//!
//! These four sources all report provider- or CLI-measured token counts, so
//! everything this module emits is classified [`EvidenceClassification::Reported`].
//! The estimated-source parser (`aub-lqe.5`) owns reconstruction; this module never
//! estimates.
//!
//! The three field vocabularies do not agree, and the differences are not
//! cosmetic: pi, Codex and opencode each report a reasoning count and Claude
//! Code does not, Codex splits cache into read and write while pi names them `cacheRead` and
//! `cacheWrite`, and Claude Code alone breaks cache creation down by ephemeral
//! lifetime. A normalisation that silently drops a field it does not recognise
//! understates the source it least understands, so every token class a source
//! reports is either mapped to one of the four known kinds or preserved in the
//! usage vector's unknown-component map. The one deliberate exception is the
//! total: `total_tokens` / `totalTokens` is a derived sum, not a token class,
//! and the token model deliberately has no total slot (see `domain::tokens`).
//! The reasoning counts are the other exception, and each parser's doc says
//! how its source's count relates to `output`: `output` means every token the
//! model generated, in every harness.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    TokenKind, UsageVector,
};
use crate::evidence::{ComponentKind, CoverageCompleteness, EvidenceQuality, Provenance};
use crate::transcripts::parser::{
    EvidenceClassification, FixtureCoverage, FixtureShape, InputFormatVersion,
    NormalizedUsageEvent, ParseOutput, ParserAdapter, ParserVersion, QuarantineClass,
    QuarantineRecord, STRONG_IDENTITY_PREFIX, SourceLocation,
};

/// The source namespaces the four parsers attribute sessions under. One
/// definition each, so a session join never sees two spellings of one source.
pub const CLAUDE_CODE_NAMESPACE: &str = "claude-code";
pub const CODEX_NAMESPACE: &str = "codex";
pub const OPENCODE_NAMESPACE: &str = "opencode";
pub const PI_NAMESPACE: &str = "pi";

/// The source namespace a declared format's events and sessions are attributed
/// under, or `None` for a format no parser reads. One definition next to the
/// constants: an occurrence row and the session rows the same parser produces
/// always carry one spelling of the source, because both read this mapping.
pub fn namespace_for_format(format: &str) -> Option<&'static str> {
    match format {
        "agy" => Some(crate::transcripts::estimated::AGY_NAMESPACE),
        "claude-code" => Some(CLAUDE_CODE_NAMESPACE),
        "codex" => Some(CODEX_NAMESPACE),
        "opencode" => Some(OPENCODE_NAMESPACE),
        "pi" => Some(PI_NAMESPACE),
        _ => None,
    }
}

/// The fixture directory for this parser, relative to the crate root.
pub const FIXTURE_DIR: &str = "tests/fixtures/transcripts/native";

/// A measured usage vector: the four known kinds plus any unknown components.
fn measured_usage(counts: UsageCounts) -> UsageVector {
    UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(counts.input),
            OutputTokens::new(counts.output),
            CacheReadTokens::new(counts.cache_read),
            CacheWriteTokens::new(counts.cache_write),
        ),
        counts.unknown,
        if counts.missing.is_empty() {
            CoverageCompleteness::Complete
        } else {
            CoverageCompleteness::partial(counts.missing)
        },
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

/// The stable coverage component name for one known token kind. Source field
/// names differ between CLIs, so coverage records the normalized kind instead.
fn component_kind(kind: TokenKind) -> ComponentKind {
    match kind {
        TokenKind::Input => ComponentKind::new("input"),
        TokenKind::Output => ComponentKind::new("output"),
        TokenKind::CacheRead => ComponentKind::new("cache-read"),
        TokenKind::CacheWrite => ComponentKind::new("cache-write"),
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
    let mut counts = UsageCounts::default();

    for (key, value) in usage {
        let kind = known.iter().find(|(name, _)| *name == key).map(|(_, k)| *k);
        match kind {
            Some(TokenKind::Input) => counts.input = count_value(value)?,
            Some(TokenKind::Output) => counts.output = count_value(value)?,
            Some(TokenKind::CacheRead) => counts.cache_read = count_value(value)?,
            Some(TokenKind::CacheWrite) => counts.cache_write = count_value(value)?,
            None if ignored.contains(&key.as_str()) => {}
            None => {
                if let Some(count) = unknown_count(value) {
                    counts.unknown.insert(key.clone(), TokenCount::new(count));
                }
            }
        }
    }

    for field in required {
        if !usage.contains_key(*field) {
            return Err(QuarantineClass::MissingRequiredField);
        }
    }

    for (field, kind) in known {
        if !usage.contains_key(*field) && !required.contains(field) {
            counts.missing.insert(component_kind(*kind));
        }
    }

    Ok(counts)
}

/// The record-level context every source writes beside its counts: the record
/// timestamp, the session the record belongs to, the working directory the
/// transcript states the session ran in, and the stable event identifier
/// where the source has one. Each is optional so an absent value stays absent.
struct RecordContext<'a> {
    event_id: Option<&'a str>,
    occurred_at: Option<UtcTimestamp>,
    session: Option<SessionId>,
    working_directory: Option<String>,
    model: Option<&'a str>,
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
    if let Some(model) = context.model {
        sources.push(format!("model:{model}"));
    }
    let mut event = NormalizedUsageEvent::new(
        usage,
        EvidenceClassification::Reported,
        Provenance::new(sources),
        parser_version,
    )
    .with_working_directory(context.working_directory);
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

/// A record's top-level `cwd`, when the source writes one. An empty directory
/// stays absent: no transcript states an empty directory legitimately.
fn record_working_directory(value: &Value) -> Option<String> {
    value
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|dir| !dir.is_empty())
        .map(str::to_string)
}

fn session_id(namespace: &str, native: &str) -> SessionId {
    SessionId::new(
        SourceNamespace::new(namespace),
        NativeSessionId::new(native),
    )
}

/// The four headline counts plus unknown and absent components from one native
/// usage record. A missing known kind stays zero in the numeric vector, but its
/// coverage witness prevents that placeholder from being read as evidence.
#[derive(Default)]
struct UsageCounts {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    unknown: BTreeMap<String, TokenCount>,
    missing: BTreeSet<ComponentKind>,
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
    let mut counts = extract_usage(usage, &CLAUDE_KNOWN, &CLAUDE_IGNORED, &["input_tokens"])?;
    if !usage.contains_key("cache_creation_input_tokens")
        && let Some(cache_write) = claude_cache_write_fallback(usage)?
    {
        counts.cache_write = cache_write;
        counts
            .missing
            .remove(&component_kind(TokenKind::CacheWrite));
    }
    let context = RecordContext {
        event_id: message.get("id").and_then(Value::as_str),
        occurred_at: record_timestamp(&value),
        session: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(|native| session_id(CLAUDE_CODE_NAMESPACE, native)),
        working_directory: record_working_directory(&value),
        model: message.get("model").and_then(Value::as_str),
    };
    Ok(Some(event(
        measured_usage(counts),
        location.file(),
        context,
        parser_version,
    )))
}

/// The cache write when the total is absent: the sum of the ephemeral
/// breakdown, which is the only place the figure then lives.
fn claude_cache_write_fallback(
    usage: &serde_json::Map<String, Value>,
) -> Result<Option<u64>, QuarantineClass> {
    let Some(breakdown) = usage.get("cache_creation").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut sum = 0u64;
    let mut found = false;
    for key in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
        if let Some(value) = breakdown.get(key) {
            sum += count_value(value)?;
            found = true;
        }
    }
    Ok(found.then_some(sum))
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
///
/// `reasoning_output_tokens` is a breakdown of `output_tokens`, not a bucket
/// beside it: across 400 of 400 real events measured on 2026-09-20,
/// `input_tokens + output_tokens == total_tokens` with the reasoning count
/// already inside that output (`aub-i589`). `output` is priced whole, so the
/// breakdown is ignored rather than carried as an unknown component, which
/// the cost model would refuse and which would double count if priced.
///
/// The model comes from the `turn_context` records, which is the only place a
/// rollout states it: `session_meta` names `model_provider` and not the model,
/// and a `token_count` payload carries `info`, `rate_limits` and `type` only.
/// A rollout may carry several turn contexts, so the model in force is the one
/// the most recent `turn_context` *before that record* named, never the last
/// one in the file. A record before any turn context keeps an empty model, and
/// a `session_meta` that does name a model is honoured so a future rollout
/// format that moves it into the header does not silently lose it.
pub struct CodexParser;

const CODEX_KNOWN: [(&str, TokenKind); 4] = [
    ("input_tokens", TokenKind::Input),
    ("output_tokens", TokenKind::Output),
    ("cached_input_tokens", TokenKind::CacheRead),
    ("cache_write_input_tokens", TokenKind::CacheWrite),
];
const CODEX_IGNORED: [&str; 2] = ["total_tokens", "reasoning_output_tokens"];

impl ParserAdapter for CodexParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("codex-3")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("codex-jsonl-v1")
    }

    /// Codex reports totals so far on every `token_count` record, so its
    /// events are points of one monotonic series per session, never
    /// independent consumption. The cumulative pipeline orders the surviving
    /// series and differences it; summing the records as they arrive would
    /// count every earlier snapshot again.
    fn reports_cumulative(&self) -> bool {
        true
    }

    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        let mut last: Option<(UsageCounts, Option<UtcTimestamp>, Option<String>)> = None;
        let mut session: Option<SessionId> = None;
        let mut parent_session: Option<SessionId> = None;
        let mut working_directory: Option<String> = None;
        let mut model: Option<String> = None;
        let mut quarantined = Vec::new();
        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_codex_line(line) {
                // The model travels with the record rather than being read off
                // the end of the file: a rollout that switched models after its
                // last usage record would otherwise price that usage under a
                // model it never ran.
                Ok(CodexLine::Usage(usage, occurred_at)) => {
                    last = Some((usage, occurred_at, model.clone()));
                }
                Ok(CodexLine::Session(header)) => {
                    session = Some(header.id);
                    if header.parent.is_some() {
                        parent_session = header.parent;
                    }
                    if header.model.is_some() {
                        model = header.model;
                    }
                    if header.working_directory.is_some() {
                        working_directory = header.working_directory;
                    }
                }
                Ok(CodexLine::TurnContext(turn_model)) => model = Some(turn_model),
                Ok(CodexLine::Nothing) => {}
                Err(class) => quarantined.push(QuarantineRecord::new(
                    record_location,
                    self.parser_version(),
                    class,
                )),
            }
        }
        let events = last
            .map(|(counts, occurred_at, model)| {
                event(
                    measured_usage(counts),
                    location.file(),
                    RecordContext {
                        event_id: None,
                        occurred_at,
                        session,
                        working_directory,
                        model: model.as_deref(),
                    },
                    self.parser_version(),
                )
                .with_parent_session(parent_session.clone())
            })
            .into_iter()
            .collect();
        ParseOutput::new(events, quarantined)
    }
}

/// What a Codex `session_meta` header names: the session, the subagent parent
/// thread when the rollout is a subagent (`source.subagent.thread_spawn.
/// parent_thread_id`), the model when a rollout states one there, and the
/// working directory the session ran in (`cwd` in the header payload).
struct CodexSession {
    id: SessionId,
    parent: Option<SessionId>,
    model: Option<String>,
    working_directory: Option<String>,
}

/// What one Codex line contributes: a cumulative usage record, the session
/// header, a turn context naming the model from here on, or nothing this
/// parser reads.
enum CodexLine {
    Usage(UsageCounts, Option<UtcTimestamp>),
    Session(CodexSession),
    TurnContext(String),
    Nothing,
}

/// The subagent parent thread a Codex `session_meta` payload names, when it
/// names one (`source.subagent.thread_spawn.parent_thread_id`).
///
/// A top-level session carries `"source":"exec"` (or another string) there and
/// yields nothing, as does a missing `source`. A malformed `source` object (a
/// `subagent` that is not an object, a `thread_spawn` that is not an object,
/// or a `parent_thread_id` that is not a non-empty string) yields nothing
/// without an error: the rollout is still a usable session, it simply records
/// no parent link.
fn codex_parent_thread_id(payload: &serde_json::Map<String, Value>) -> Option<SessionId> {
    let source = payload.get("source")?;
    let source_object = source.as_object()?;
    let subagent = source_object.get("subagent")?;
    let subagent_object = subagent.as_object()?;
    let spawn = subagent_object.get("thread_spawn")?;
    let spawn_object = spawn.as_object()?;
    let parent = spawn_object.get("parent_thread_id")?.as_str()?;
    if parent.is_empty() {
        return None;
    }
    Some(session_id(CODEX_NAMESPACE, parent))
}

fn parse_codex_line(line: &str) -> Result<CodexLine, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return Ok(CodexLine::Nothing);
    };
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        let model = payload
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_string);
        let working_directory = payload
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|dir| !dir.is_empty())
            .map(str::to_string);
        let parent = codex_parent_thread_id(payload);
        return Ok(payload
            .get("id")
            .and_then(Value::as_str)
            .map(|native| {
                CodexLine::Session(CodexSession {
                    id: session_id(CODEX_NAMESPACE, native),
                    parent,
                    model,
                    working_directory,
                })
            })
            .unwrap_or(CodexLine::Nothing));
    }
    if value.get("type").and_then(Value::as_str) == Some("turn_context") {
        // A turn context with no model is not a defect: it states everything
        // else about the turn, and the model in force simply does not change.
        return Ok(payload
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map(|model| CodexLine::TurnContext(model.to_string()))
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
/// `{"type":"session","id":...}` header line. The `cost` object nested inside
/// `usage` is money, not usage, and is ignored.
///
/// `reasoning` is a breakdown of `output`, not a bucket beside it: across 400
/// of 400 real events measured on 2026-09-20,
/// `input + output + cacheRead + cacheWrite == totalTokens` with the reasoning
/// count already inside that output (`aub-i589`). `output` is priced whole, so
/// the breakdown is ignored; adding it to `output` would count those tokens
/// twice.
pub struct PiParser;

const PI_KNOWN: [(&str, TokenKind); 4] = [
    ("input", TokenKind::Input),
    ("output", TokenKind::Output),
    ("cacheRead", TokenKind::CacheRead),
    ("cacheWrite", TokenKind::CacheWrite),
];
const PI_IGNORED: [&str; 2] = ["totalTokens", "reasoning"];

impl ParserAdapter for PiParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("pi-2")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("pi-jsonl-v1")
    }

    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        let mut events = Vec::new();
        let mut quarantined = Vec::new();
        let mut session: Option<(SessionId, Option<String>)> = None;
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
                Ok(PiLine::Session(id, working_directory)) => {
                    session = Some((id, working_directory));
                }
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

/// What one pi line contributes: a usage event, the session header with the
/// working directory it states, or nothing this parser reads.
enum PiLine {
    Usage(Box<NormalizedUsageEvent>),
    Session(SessionId, Option<String>),
    Nothing,
}

fn parse_pi_line(
    line: &str,
    location: &SourceLocation,
    session: Option<(SessionId, Option<String>)>,
    parser_version: ParserVersion,
) -> Result<PiLine, QuarantineClass> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| QuarantineClass::TruncatedStructure)?;
    if value.get("type").and_then(Value::as_str) == Some("session") {
        let working_directory = record_working_directory(&value);
        return Ok(value
            .get("id")
            .and_then(Value::as_str)
            .map(|native| PiLine::Session(session_id(PI_NAMESPACE, native), working_directory))
            .unwrap_or(PiLine::Nothing));
    }
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        return Ok(PiLine::Nothing);
    };
    let Some(usage) = message.get("usage").and_then(Value::as_object) else {
        return Ok(PiLine::Nothing);
    };
    let counts = extract_usage(usage, &PI_KNOWN, &PI_IGNORED, &["input"])?;
    let (session, working_directory) = session
        .map(|(id, dir)| (Some(id), dir))
        .unwrap_or((None, None));
    let context = RecordContext {
        event_id: message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("id").and_then(Value::as_str)),
        occurred_at: record_timestamp(&value),
        session,
        working_directory,
        model: message.get("model").and_then(Value::as_str),
    };
    Ok(PiLine::Usage(Box::new(event(
        measured_usage(counts),
        location.file(),
        context,
        parser_version,
    ))))
}

/// The opencode transcript parser.
///
/// Reads the `message` table of the opencode session database (`opencode.db`)
/// through `crate::store::opencode`, one row per message. An assistant row's
/// `data` carries `tokens: {input, output, reasoning, cache: {write, read}}`
/// beside `role`, `modelID`, `providerID` and `time: {created, completed}`.
/// The nested cache pair is flattened into the vocabulary the shared
/// `extract_usage` helper reads, and `total` is a derived sum and is ignored the
/// way the other sources' totals are.
///
/// `reasoning` is added to `output`. Unlike pi and Codex, opencode's reasoning
/// is a bucket beside `output`, not inside it: across 3341 of 3341 real
/// events measured on 2026-09-20,
/// `input + output + cache.read + cache.write + reasoning == total`
/// (`aub-i589`). A reasoning token is priced as an output token of the same
/// model, so folding it into `output` values it exactly, with no separate
/// cost model term. The fold happens after each count is validated, so a
/// negative or non-integer `reasoning`, or a non-integer `output`, still
/// quarantines rather than being rescued by the sum. A negative `output` is
/// opencode's own subtraction gone below zero, not a bad count, and is read
/// back into the provider's count only when the row's total confirms it; see
/// `opencode_generated_from_difference`. Since 1.14.45 opencode clamps that
/// same difference at zero, so a row where the provider reported more
/// reasoning than output stores `output: 0` and the total is the only
/// surviving trace of the provider's count; such a row is read back as
/// `total - input - cache.read - cache.write` only when the total is present
/// and below the component sum, see `opencode_generated_from_difference`.
/// A row with `output: 0` whose total equals the component sum keeps the
/// ordinary fold, and a row with `output: 0` and no total keeps the ordinary
/// fold as well, because there is then nothing to confirm the clamped
/// reading against. `message.id`
/// is the stable event identifier, the strong identity dedup collapses
/// replays on. A user row carries no tokens and is skipped silently, the way
/// a record without a usage object is; an assistant row without one
/// quarantines, because a count the source should have written is missing,
/// not absent by shape.
pub struct OpencodeParser;

const OPENCODE_KNOWN: [(&str, TokenKind); 4] = [
    ("input", TokenKind::Input),
    ("output", TokenKind::Output),
    ("cache_read", TokenKind::CacheRead),
    ("cache_write", TokenKind::CacheWrite),
];
const OPENCODE_IGNORED: [&str; 1] = ["total"];

impl ParserAdapter for OpencodeParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("opencode-4")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("opencode-sqlite-v1")
    }

    /// A database source has no text form: empty input parses to nothing, and
    /// anything else quarantines as unsupported rather than silently
    /// producing zero events.
    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        if input.trim().is_empty() {
            return ParseOutput::new(Vec::new(), Vec::new());
        }
        ParseOutput::new(
            Vec::new(),
            vec![QuarantineRecord::new(
                location.clone(),
                self.parser_version(),
                QuarantineClass::UnsupportedInputFormat,
            )],
        )
    }

    fn is_database_source(&self) -> bool {
        true
    }

    fn parse_database_file(&self, path: &Path, location: &SourceLocation) -> ParseOutput {
        let (rows, directories) = match crate::store::opencode::open_opencode_database(path)
            .and_then(|connection| {
                let rows = crate::store::opencode::read_message_rows(&connection)?;
                let directories = crate::store::opencode::read_session_directories(&connection);
                Ok((rows, directories))
            }) {
            Ok(rows) => rows,
            // A file that is not an opencode database is an input this parser
            // does not understand, counted once at the file rather than
            // dropped silently or aborting the pass.
            Err(_) => {
                return ParseOutput::new(
                    Vec::new(),
                    vec![QuarantineRecord::new(
                        location.clone(),
                        self.parser_version(),
                        QuarantineClass::UnsupportedInputFormat,
                    )],
                );
            }
        };
        let mut events = Vec::new();
        let mut quarantined = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            match parse_opencode_row(row, &directories, &record_location, self.parser_version()) {
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

/// Turns one opencode message row into a normalized event: `None` for a user
/// row, which carries no tokens by shape, or the quarantine class for a row
/// that should carry usage but cannot be normalized. The working directory
/// comes from the session table the caller already read, keyed by the row's
/// session id; a session the table does not name leaves no directory.
fn parse_opencode_row(
    row: &crate::store::opencode::OpencodeMessageRow,
    directories: &std::collections::BTreeMap<String, String>,
    location: &SourceLocation,
    parser_version: ParserVersion,
) -> Result<Option<NormalizedUsageEvent>, QuarantineClass> {
    let data: Value =
        serde_json::from_str(&row.data).map_err(|_| QuarantineClass::TruncatedStructure)?;
    let message = data
        .as_object()
        .ok_or(QuarantineClass::TruncatedStructure)?;
    if message.get("role").and_then(Value::as_str) == Some("user") {
        return Ok(None);
    }
    let tokens = message
        .get("tokens")
        .ok_or(QuarantineClass::MissingRequiredField)?;
    let tokens = tokens.as_object().ok_or(QuarantineClass::WrongFieldType)?;
    let mut flat = tokens.clone();
    flat.remove("cache");
    if let Some(cache) = tokens.get("cache") {
        let cache = cache.as_object().ok_or(QuarantineClass::WrongFieldType)?;
        if let Some(read) = cache.get("read") {
            flat.insert("cache_read".to_string(), read.clone());
        }
        if let Some(write) = cache.get("write") {
            flat.insert("cache_write".to_string(), write.clone());
        }
    }
    let reasoning = flat.remove("reasoning");
    let generated = opencode_generated_from_difference(&flat, reasoning.as_ref())?;
    if let Some(generated) = generated {
        flat.insert("output".to_string(), Value::from(generated));
    }
    let mut counts = extract_usage(&flat, &OPENCODE_KNOWN, &OPENCODE_IGNORED, &["input"])?;
    if let (None, Some(reasoning)) = (generated, reasoning) {
        counts.output = counts
            .output
            .checked_add(count_value(&reasoning)?)
            .ok_or(QuarantineClass::WrongFieldType)?;
    }
    let occurred_at = message
        .get("time")
        .and_then(Value::as_object)
        .and_then(|time| {
            time.get("completed")
                .and_then(opencode_millis)
                .or_else(|| time.get("created").and_then(opencode_millis))
        })
        .or_else(|| opencode_millis_value(row.time_created_ms));
    let provider = message.get("providerID").and_then(Value::as_str);
    let model_id = message.get("modelID").and_then(Value::as_str);
    let model = match (provider, model_id) {
        (Some(provider), Some(model_id)) => Some(format!("{provider}/{model_id}")),
        (Some(provider), None) => Some(provider.to_string()),
        (None, Some(model_id)) => Some(model_id.to_string()),
        (None, None) => None,
    };
    let context = RecordContext {
        event_id: Some(row.message_id.as_str()),
        occurred_at,
        session: Some(session_id(OPENCODE_NAMESPACE, row.session_id.as_str())),
        working_directory: directories.get(&row.session_id).cloned(),
        model: model.as_deref(),
    };
    Ok(Some(event(
        measured_usage(counts),
        location.file(),
        context,
        parser_version,
    )))
}

/// The generated count behind an opencode `output` that is not the provider's
/// own count, or `None` when the ordinary fold applies.
///
/// opencode does not store the provider's output count: it stores
/// `outputTokens - reasoningTokens`, clamped at zero since 1.14.45 and
/// negative before that whenever a provider reported more reasoning than
/// output (`aub-o6bc`, `aub-rjy9`). The provider's own count is recovered
/// only when the row proves the reading. For a negative `output` that proof
/// is a present reasoning count, a non-negative sum, and a `total` equal to
/// `input + generated + cache.read + cache.write`, where `generated` is
/// `output + reasoning`. For a zero `output` the proof is a present `total`
/// below `input + reasoning + cache.read + cache.write`, and the generated
/// count is `total - input - cache.read - cache.write`. A zero `output`
/// whose total equals the component sum keeps the ordinary fold, as does a
/// zero `output` with no total, because there is then nothing to confirm the
/// clamped reading against. Any other negative or inconsistent `output`
/// still quarantines as a wrong type.
fn opencode_generated_from_difference(
    flat: &serde_json::Map<String, Value>,
    reasoning: Option<&Value>,
) -> Result<Option<u64>, QuarantineClass> {
    let component =
        |key: &str| -> Result<u64, QuarantineClass> { flat.get(key).map_or(Ok(0), count_value) };
    if let Some(difference) = flat
        .get("output")
        .and_then(Value::as_i64)
        .filter(|output| *output < 0)
    {
        let reasoning = count_value(reasoning.ok_or(QuarantineClass::WrongFieldType)?)?;
        let generated = reasoning
            .checked_add_signed(difference)
            .ok_or(QuarantineClass::WrongFieldType)?;
        let components = [
            component("input")?,
            generated,
            component("cache_read")?,
            component("cache_write")?,
        ];
        let summed = components
            .iter()
            .try_fold(0_u64, |sum, count| sum.checked_add(*count))
            .ok_or(QuarantineClass::WrongFieldType)?;
        let total = flat.get("total").ok_or(QuarantineClass::WrongFieldType)?;
        if count_value(total)? != summed {
            return Err(QuarantineClass::WrongFieldType);
        }
        return Ok(Some(generated));
    }
    if flat.get("output").and_then(Value::as_u64) != Some(0) {
        return Ok(None);
    }
    let Some(total_value) = flat.get("total") else {
        return Ok(None);
    };
    let total = count_value(total_value)?;
    let input = component("input")?;
    let cache_read = component("cache_read")?;
    let cache_write = component("cache_write")?;
    let base = input
        .checked_add(cache_read)
        .and_then(|sum| sum.checked_add(cache_write))
        .ok_or(QuarantineClass::WrongFieldType)?;
    let generated = total
        .checked_sub(base)
        .ok_or(QuarantineClass::WrongFieldType)?;
    let Some(reasoning_value) = reasoning else {
        if generated == 0 {
            return Ok(None);
        }
        return Err(QuarantineClass::WrongFieldType);
    };
    let reasoning_count = count_value(reasoning_value)?;
    let component_sum = base
        .checked_add(reasoning_count)
        .ok_or(QuarantineClass::WrongFieldType)?;
    if total == component_sum {
        return Ok(None);
    }
    if total > component_sum {
        return Err(QuarantineClass::WrongFieldType);
    }
    Ok(Some(generated))
}

/// A millisecond timestamp as opencode writes it, or `None` for a value that
/// is not one. A float, a string or a negative count is not a timestamp this
/// parser understands; the event then keeps no time rather than an invented
/// one.
fn opencode_millis(value: &Value) -> Option<UtcTimestamp> {
    let millis = value.as_i64().filter(|millis| *millis >= 0)?;
    opencode_millis_value(millis)
}

/// The row timestamp in whole-epoch units, or `None` when the multiplication
/// into nanoseconds would overflow.
fn opencode_millis_value(millis: i64) -> Option<UtcTimestamp> {
    millis
        .checked_mul(1_000_000)
        .map(UtcTimestamp::from_unix_nanos)
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

    /// opencode carries its own namespace, so its sessions never join another
    /// source's under a shared spelling.
    #[test]
    fn opencode_has_its_own_source_namespace() {
        assert_eq!(namespace_for_format("opencode"), Some(OPENCODE_NAMESPACE));
        assert_eq!(OPENCODE_NAMESPACE, "opencode");
    }

    /// opencode declares its parser and input-format versions, the pair the
    /// watermark and the fixture manifest pin a parse to.
    #[test]
    fn opencode_declares_its_parser_and_input_format_versions() {
        let parser = OpencodeParser;
        assert_eq!(parser.parser_version().as_str(), "opencode-4");
        assert_eq!(parser.input_format_version().as_str(), "opencode-sqlite-v1");
        assert!(parser.is_database_source());
    }

    /// Codex declares `codex-3`, so `ingest --changed-only` re-derives every
    /// existing rollout and fills `parent_native_session_id` for sessions
    /// ingested before the subagent column existed (`aub-wvrw`).
    #[test]
    fn codex_declares_its_parser_and_input_format_versions() {
        let parser = CodexParser;
        assert_eq!(parser.parser_version().as_str(), "codex-3");
        assert_eq!(parser.input_format_version().as_str(), "codex-jsonl-v1");
    }

    /// A database source has no text form: empty input parses to nothing, and
    /// anything else quarantines as unsupported rather than silently
    /// producing zero events.
    #[test]
    fn opencode_text_input_quarantines_instead_of_silently_parsing_nothing() {
        let parser = OpencodeParser;
        let loc = location();
        let empty = parser.parse("", &loc);
        assert!(empty.events().is_empty());
        assert!(empty.quarantined().is_empty());
        let text = parser.parse(r#"{"message":{"id":"m1","usage":{"input":10}}}"#, &loc);
        assert!(text.events().is_empty());
        assert_eq!(text.quarantined().len(), 1);
        assert_eq!(
            text.quarantined()[0].class(),
            QuarantineClass::UnsupportedInputFormat
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

    /// Each fixture parses to its golden output: every event's normalized token
    /// vector and the expected quarantine count.
    #[test]
    fn each_fixture_parses_to_its_golden_output() {
        type ExpectedUsage = (u64, u64, u64, u64);
        let cases: [(&str, &dyn ParserAdapter, &[ExpectedUsage], usize); 11] = [
            (
                "claude-real-shape.jsonl",
                &ClaudeCodeParser,
                &[
                    (2, 913, 26_503, 30_011),
                    (2, 1_188, 26_503, 30_011),
                    (4, 41, 12_000, 0),
                ],
                0,
            ),
            (
                "codex-real-shape.jsonl",
                &CodexParser,
                &[(34_830, 404, 19_200, 0)],
                0,
            ),
            (
                "pi-real-shape.jsonl",
                &PiParser,
                &[(19_221, 302, 0, 0), (20_001, 150, 512, 64)],
                0,
            ),
            (
                "claude-simple-session.jsonl",
                &ClaudeCodeParser,
                &[(1_200, 340, 0, 0), (800, 210, 0, 0)],
                0,
            ),
            (
                "claude-nested-subagent.jsonl",
                &ClaudeCodeParser,
                &[(100, 50, 0, 0)],
                0,
            ),
            (
                "codex-truncated.jsonl",
                &CodexParser,
                &[(100, 50, 20, 10)],
                1,
            ),
            ("pi-partial-final.jsonl", &PiParser, &[(100, 50, 20, 10)], 1),
            (
                "claude-malformed.jsonl",
                &ClaudeCodeParser,
                &[(100, 50, 0, 0)],
                1,
            ),
            (
                "pi-model-change.jsonl",
                &PiParser,
                &[(100, 50, 0, 0), (200, 100, 0, 0)],
                0,
            ),
            ("codex-cache.jsonl", &CodexParser, &[(200, 100, 40, 20)], 0),
            ("pi-no-usage.jsonl", &PiParser, &[], 0),
        ];
        for (fixture, parser, expected, quarantined) in cases {
            let output = parser.parse(&read_fixture(fixture), &SourceLocation::new(fixture, 1));
            let actual: Vec<ExpectedUsage> = output
                .events()
                .iter()
                .map(|event| {
                    let known = event.usage().known();
                    (
                        known.input().value(),
                        known.output().value(),
                        known.cache_read().value(),
                        known.cache_write().value(),
                    )
                })
                .collect();
            assert_eq!(actual, expected, "fixture {fixture}");
            assert_eq!(output.quarantined().len(), quarantined, "fixture {fixture}");
        }
    }

    /// A missing optional field keeps its numeric placeholder at zero while its
    /// coverage names the absent kinds. Explicit zero values are complete data.
    #[test]
    fn absent_optional_known_fields_are_partial_not_measured_zero() {
        let missing = CoverageCompleteness::partial([
            ComponentKind::new("output"),
            ComponentKind::new("cache-read"),
            ComponentKind::new("cache-write"),
        ]);
        let cases: [(&dyn ParserAdapter, &str, &str); 3] = [
            (
                &ClaudeCodeParser,
                r#"{"message":{"usage":{"input_tokens":1}}}"#,
                r#"{"message":{"usage":{"input_tokens":1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
            ),
            (
                &CodexParser,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1}}}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":0,"cached_input_tokens":0,"cache_write_input_tokens":0}}}}"#,
            ),
            (
                &PiParser,
                r#"{"message":{"usage":{"input":1}}}"#,
                r#"{"message":{"usage":{"input":1,"output":0,"cacheRead":0,"cacheWrite":0}}}"#,
            ),
        ];

        for (parser, absent, explicit_zero) in cases {
            let absent = parser.parse(absent, &location());
            assert_eq!(absent.events().len(), 1);
            assert_eq!(absent.events()[0].usage().coverage(), &missing);
            let explicit_zero = parser.parse(explicit_zero, &location());
            assert_eq!(explicit_zero.events().len(), 1);
            assert_eq!(
                explicit_zero.events()[0].usage().coverage(),
                &CoverageCompleteness::Complete
            );
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

    fn codex_model(output: &crate::transcripts::ParseOutput) -> Option<String> {
        output.events()[0]
            .provenance()
            .sources()
            .iter()
            .find_map(|source| source.strip_prefix("model:").map(str::to_string))
    }

    /// The model of the turn context in force when the last cumulative record
    /// was written, read from the real rollout shape: `turn_context` is the
    /// only record that names it.
    #[test]
    fn codex_carries_the_model_of_the_turn_context_in_force() {
        let parser = CodexParser;
        let output = parser.parse(
            &read_fixture("codex-model-change.jsonl"),
            &SourceLocation::new("codex-model-change.jsonl", 1),
        );
        assert_eq!(output.events().len(), 1);
        assert_eq!(codex_model(&output).as_deref(), Some("gpt-5.6-luna"));
    }

    /// The planted negative for the one above. The two inputs differ only in
    /// where the last `token_count` sits: here it precedes the second turn
    /// context, so the usage was produced under the first model and the second
    /// never ran a priced token. An implementation that scans the file for the
    /// last `turn_context` passes the positive and fails this.
    #[test]
    fn codex_never_takes_a_turn_context_that_follows_the_last_usage_record() {
        let parser = CodexParser;
        let input = concat!(
            r#"{"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5.6-terra"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"output_tokens":100}}}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"turn_id":"t2","model":"gpt-5.6-luna"}}"#,
        );
        let output = parser.parse(input, &location());
        assert_eq!(
            codex_model(&output).as_deref(),
            Some("gpt-5.6-terra"),
            "the model in force at the record, not the last one in the file"
        );
    }

    /// A record before any turn context keeps an empty model rather than
    /// borrowing one from later in the file.
    #[test]
    fn codex_usage_before_any_turn_context_carries_no_model() {
        let parser = CodexParser;
        let input = concat!(
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5.6-terra"}}"#,
        );
        let output = parser.parse(input, &location());
        assert_eq!(codex_model(&output), None);
    }

    /// A `turn_context` that states everything but the model leaves the model
    /// in force unchanged, rather than clearing it.
    #[test]
    fn a_turn_context_without_a_model_does_not_clear_the_one_in_force() {
        let parser = CodexParser;
        let input = concat!(
            r#"{"type":"turn_context","payload":{"turn_id":"t1","model":"gpt-5.6-terra"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"turn_id":"t2","cwd":"/work/project"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
        );
        let output = parser.parse(input, &location());
        assert_eq!(codex_model(&output).as_deref(), Some("gpt-5.6-terra"));
    }

    /// A `session_meta` that names a model is honoured, so a rollout format
    /// that moves the model into the header does not silently lose it.
    #[test]
    fn codex_session_meta_names_the_model_when_it_carries_one() {
        let parser = CodexParser;
        let input = concat!(
            r#"{"type":"session_meta","payload":{"id":"s1","model":"gpt-5.6-terra"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
        );
        let output = parser.parse(input, &location());
        assert_eq!(codex_model(&output).as_deref(), Some("gpt-5.6-terra"));
    }

    /// pi's `reasoning` is a breakdown inside `output`: it reaches neither the
    /// unknown map nor the output count. The planted negative is the opencode
    /// fold applied to pi, which would make `output` 55 and double count.
    #[test]
    fn pi_reasoning_is_inside_output_and_is_not_carried() {
        let parser = PiParser;
        let input = r#"{"message":{"id":"m1","usage":{"input":100,"output":50,"reasoning":5}}}"#;
        let output = parser.parse(input, &location());
        let event = &output.events()[0];
        assert!(
            event.usage().unknown().is_empty(),
            "reasoning must not reach the unknown map: {:?}",
            event.usage().unknown()
        );
        assert_eq!(
            event.usage().known().output().value(),
            50,
            "output must equal the source's output exactly"
        );
    }

    /// Codex's `reasoning_output_tokens` is a breakdown inside `output_tokens`,
    /// over every native fixture that carries one: `output` stays the source's
    /// own figure and no unknown component remains.
    #[test]
    fn codex_reasoning_is_inside_output_and_is_not_carried() {
        let cases = [
            ("codex-real-shape.jsonl", 404),
            ("codex-cache.jsonl", 100),
            ("codex-model-change.jsonl", 100),
        ];
        for (fixture, expected_output) in cases {
            let output =
                CodexParser.parse(&read_fixture(fixture), &SourceLocation::new(fixture, 1));
            assert!(output.quarantined().is_empty(), "fixture {fixture}");
            let last = output.events().last().expect("one cumulative event");
            assert!(
                last.usage().unknown().is_empty(),
                "fixture {fixture}: {:?}",
                last.usage().unknown()
            );
            assert_eq!(
                last.usage().known().output().value(),
                expected_output,
                "fixture {fixture}"
            );
        }
    }

    fn opencode_row(tokens: &str) -> crate::store::opencode::OpencodeMessageRow {
        crate::store::opencode::OpencodeMessageRow {
            message_id: "msg_1".to_string(),
            session_id: "ses_1".to_string(),
            time_created_ms: 1_788_220_800_000,
            data: format!(
                r#"{{"role":"assistant","modelID":"m","providerID":"p","tokens":{tokens}}}"#
            ),
        }
    }

    fn parse_opencode(tokens: &str) -> Result<Option<NormalizedUsageEvent>, QuarantineClass> {
        parse_opencode_row(
            &opencode_row(tokens),
            &BTreeMap::new(),
            &location(),
            OpencodeParser.parser_version(),
        )
    }

    /// opencode's `reasoning` is a bucket beside `output`, priced as output,
    /// so it lands in `TokenKind::Output` and leaves no unknown component.
    #[test]
    fn opencode_reasoning_is_folded_into_output() {
        let event = parse_opencode(
            r#"{"input":54601,"output":201,"reasoning":582,"cache":{"write":0,"read":4608},"total":59992}"#,
        )
        .expect("a valid row")
        .expect("an assistant row is an event");
        assert_eq!(event.usage().known().output().value(), 783);
        assert_eq!(event.usage().known().input().value(), 54_601);
        assert_eq!(event.usage().known().cache_read().value(), 4_608);
        assert!(
            event.usage().unknown().is_empty(),
            "{:?}",
            event.usage().unknown()
        );
        let without = parse_opencode(r#"{"input":10,"output":4}"#)
            .expect("a valid row")
            .expect("an assistant row is an event");
        assert_eq!(without.usage().known().output().value(), 4);
    }

    /// The real shape opencode 1.4.11 to 1.14.41 wrote when a provider reported
    /// more reasoning than output: `output` is `outputTokens - reasoning`
    /// gone negative, and the row's total confirms it. The generated count is
    /// the provider's own `output + reasoning`, never zero and never the
    /// absolute value. The planted negative is the same row with a positive
    /// output, which folds the ordinary way.
    #[test]
    fn opencode_negative_output_is_read_back_into_the_provider_count() {
        let event = parse_opencode(
            r#"{"total":59468,"input":19296,"output":-5,"reasoning":241,"cache":{"write":0,"read":39936}}"#,
        )
        .expect("a total-confirmed difference is a valid row")
        .expect("an assistant row is an event");
        assert_eq!(event.usage().known().output().value(), 236);
        assert_eq!(event.usage().known().input().value(), 19_296);
        assert_eq!(event.usage().known().cache_read().value(), 39_936);
        assert!(event.usage().unknown().is_empty());
        let positive = parse_opencode(
            r#"{"total":59478,"input":19296,"output":5,"reasoning":241,"cache":{"write":0,"read":39936}}"#,
        )
        .expect("a valid row")
        .expect("an assistant row is an event");
        assert_eq!(positive.usage().known().output().value(), 246);
    }

    /// A negative `output` is read back only when the row proves the reading:
    /// with no reasoning to subtract from, with a sum below zero, or with a
    /// total that disagrees or is absent, it quarantines as a wrong type. A
    /// negative `reasoning` quarantines instead of being dropped from a priced
    /// figure.
    #[test]
    fn opencode_unconfirmed_negative_counts_quarantine() {
        for tokens in [
            r#"{"total":59473,"input":19296,"output":-5,"cache":{"write":0,"read":39936}}"#,
            r#"{"total":59228,"input":19296,"output":-245,"reasoning":241,"cache":{"write":0,"read":39936}}"#,
            r#"{"total":59478,"input":19296,"output":-5,"reasoning":241,"cache":{"write":0,"read":39936}}"#,
            r#"{"input":19296,"output":-5,"reasoning":241,"cache":{"write":0,"read":39936}}"#,
            r#"{"input":10,"output":5,"reasoning":-2}"#,
        ] {
            assert_eq!(
                parse_opencode(tokens).err(),
                Some(QuarantineClass::WrongFieldType),
                "{tokens}"
            );
        }
    }

    /// A clamped row stores `output: 0` where the provider reported more
    /// reasoning than output, and the total is the only surviving trace of
    /// the provider's count. The generated count is the difference the total
    /// still proves, not the full reasoning count. The planted negative is
    /// the same row with the total at the component sum, which keeps the
    /// ordinary fold.
    #[test]
    fn opencode_clamped_zero_output_is_read_back_from_the_total() {
        let event = parse_opencode(
            r#"{"total":39852,"input":39469,"output":0,"reasoning":387,"cache":{"write":0,"read":0}}"#,
        )
        .expect("a clamped row is a valid row")
        .expect("an assistant row is an event");
        assert_eq!(
            event.usage().known().output().value(),
            383,
            "the total proves 383 generated tokens, not the 387 reasoning tokens"
        );
        assert_eq!(event.usage().known().input().value(), 39_469);
        assert!(event.usage().unknown().is_empty());
        let unclamped = parse_opencode(
            r#"{"total":39856,"input":39469,"output":0,"reasoning":387,"cache":{"write":0,"read":0}}"#,
        )
        .expect("a valid row")
        .expect("an assistant row is an event");
        assert_eq!(
            unclamped.usage().known().output().value(),
            387,
            "a total at the component sum keeps the ordinary fold"
        );
    }

    /// A clamped total that would imply a negative generated count is not a
    /// count the source could have written, so it quarantines as a wrong
    /// type rather than recording a wrapped figure.
    #[test]
    fn opencode_clamped_total_below_input_plus_cache_quarantines() {
        assert_eq!(
            parse_opencode(
                r#"{"total":39000,"input":39469,"output":0,"reasoning":387,"cache":{"write":0,"read":0}}"#,
            )
            .err(),
            Some(QuarantineClass::WrongFieldType),
        );
    }

    /// A row with `output: 0` and no total keeps the ordinary fold, because
    /// there is nothing to confirm the clamped reading against.
    #[test]
    fn opencode_zero_output_without_a_total_keeps_the_ordinary_fold() {
        let event = parse_opencode(
            r#"{"input":39469,"output":0,"reasoning":387,"cache":{"write":0,"read":0}}"#,
        )
        .expect("a valid row")
        .expect("an assistant row is an event");
        assert_eq!(event.usage().known().output().value(), 387);
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
        assert_eq!(first.usage().known().output().value(), 302);
        assert!(first.usage().unknown().is_empty());
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
    /// absolute home path. Reads the one shared forbidden-pattern list
    /// (`docs/forbidden-patterns.txt`) rather than a private copy, so a
    /// pattern added there protects this scan too (aub-n27.4).
    #[test]
    fn no_fixture_contains_a_credential_a_personal_identifier_or_a_home_path() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
        for entry in std::fs::read_dir(&dir).expect("fixture directory must exist") {
            let path = entry.expect("fixture entry must be readable").path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).expect("fixture must be readable");
            let hits = test_support::sanitization::matched_patterns(&contents);
            assert!(
                hits.is_empty(),
                "fixture {} matches forbidden patterns {hits:?}",
                path.display()
            );
        }
    }

    /// The working-directory fixture directory, beside the catalog fixtures:
    /// one file per harness carrying the directory the bead's table names.
    /// These live outside the catalog directory so the corpus audit's
    /// manifest declaration rule does not apply to them.
    const WORKING_DIRECTORY_FIXTURE_DIR: &str = "tests/fixtures/transcripts/working-directory";

    fn read_working_directory_fixture(name: &str) -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(WORKING_DIRECTORY_FIXTURE_DIR)
                .join(name),
        )
        .expect("working-directory fixture must be readable")
    }

    /// Claude Code states `cwd` on every line: the event carries each line's
    /// own value, and first-wins aggregation happens downstream where the
    /// session is visible as a whole.
    #[test]
    fn claude_code_carries_the_per_line_working_directory() {
        let parser = ClaudeCodeParser;
        let output = parser.parse(
            &read_working_directory_fixture("claude-code.jsonl"),
            &SourceLocation::new("claude-code.jsonl", 1),
        );
        assert_eq!(output.events().len(), 2);
        for event in output.events() {
            assert_eq!(
                event.working_directory(),
                Some("/tmp/aub-fixture-project"),
                "every line states the same directory"
            );
        }
        // A line without `cwd` leaves no directory rather than an invented one.
        let bare = parser.parse(
            r#"{"message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5}}}"#,
            &location(),
        );
        assert_eq!(bare.events()[0].working_directory(), None);
    }

    /// Codex states `cwd` in the `session_meta` header payload: the event
    /// carries it, and a header without one leaves no directory.
    #[test]
    fn codex_carries_the_session_meta_working_directory() {
        let parser = CodexParser;
        let output = parser.parse(
            &read_working_directory_fixture("codex.jsonl"),
            &SourceLocation::new("codex.jsonl", 1),
        );
        assert_eq!(output.events().len(), 1);
        assert_eq!(
            output.events()[0].working_directory(),
            Some("/tmp/aub-fixture-project")
        );
        let bare = parser.parse(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"s1"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
            ),
            &location(),
        );
        assert_eq!(bare.events()[0].working_directory(), None);
    }

    /// pi states `cwd` on the `{"type":"session"}` header line: every usage
    /// event of that session carries it.
    #[test]
    fn pi_carries_the_session_header_working_directory() {
        let parser = PiParser;
        let output = parser.parse(
            &read_working_directory_fixture("pi.jsonl"),
            &SourceLocation::new("pi.jsonl", 1),
        );
        assert_eq!(output.events().len(), 1);
        assert_eq!(
            output.events()[0].working_directory(),
            Some("/tmp/aub-fixture-project")
        );
        // A usage record before any session header carries no directory: the
        // header is the only place pi states it.
        let bare = parser.parse(
            r#"{"message":{"id":"m1","usage":{"input":10,"output":5}}}"#,
            &location(),
        );
        assert_eq!(bare.events()[0].working_directory(), None);
    }

    /// Two lines of one session stating different directories: each event
    /// carries its own line's value, so the downstream first-wins aggregation
    /// sees both and can count the disagreement. The planted negative is a
    /// parser that resolved first-wins itself: its second event would carry
    /// the first directory and this assertion on the stated values would fail.
    #[test]
    fn claude_code_events_carry_their_own_line_values_when_the_session_moves() {
        let parser = ClaudeCodeParser;
        let output = parser.parse(
            &read_working_directory_fixture("claude-code-changes.jsonl"),
            &SourceLocation::new("claude-code-changes.jsonl", 1),
        );
        assert_eq!(output.events().len(), 2);
        let directories: Vec<Option<&str>> = output
            .events()
            .iter()
            .map(|event| event.working_directory())
            .collect();
        assert_eq!(
            directories,
            vec![
                Some("/tmp/aub-fixture-project"),
                Some("/tmp/aub-fixture-project-moved")
            ],
            "each event carries what its own line stated"
        );
    }

    /// A Codex `session_meta` with a `thread_spawn` source yields the parent
    /// thread id (`aub-wvrw`): the subagent's usage inherits the parent's
    /// account-marker timeline.
    #[test]
    fn codex_session_meta_with_thread_spawn_yields_the_parent_thread_id() {
        let parser = CodexParser;
        let output = parser.parse(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"child-1","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-1","depth":1,"agent_role":"analyzer"}}}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
            ),
            &location(),
        );
        assert_eq!(output.events().len(), 1);
        let parent = output.events()[0]
            .parent_session()
            .expect("subagent rollout must carry its parent");
        assert_eq!(parent.native().as_str(), "parent-1");
        assert_eq!(parent.source().as_str(), CODEX_NAMESPACE);
    }

    /// A top-level Codex session carries a string `source` (`"exec"`) and
    /// records no parent (`aub-wvrw`).
    #[test]
    fn codex_session_meta_with_a_string_source_records_no_parent() {
        let parser = CodexParser;
        let output = parser.parse(
            concat!(
                r#"{"type":"session_meta","payload":{"id":"s1","source":"exec"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
            ),
            &location(),
        );
        assert_eq!(output.events().len(), 1);
        assert_eq!(output.events()[0].parent_session(), None);
    }

    /// A malformed `source` object records no parent without an error
    /// (`aub-wvrw`): the rollout is still a usable session, it simply carries
    /// no parent link. Each case is a different malformation of the same
    /// `source.subagent.thread_spawn.parent_thread_id` path.
    #[test]
    fn codex_session_meta_with_a_malformed_source_records_no_parent_without_an_error() {
        let parser = CodexParser;
        let cases = [
            // No source at all.
            r#"{"type":"session_meta","payload":{"id":"s1"}}"#,
            // subagent is not an object.
            r#"{"type":"session_meta","payload":{"id":"s1","source":{"subagent":"x"}}}"#,
            // thread_spawn is not an object.
            r#"{"type":"session_meta","payload":{"id":"s1","source":{"subagent":{"thread_spawn":"x"}}}}"#,
            // parent_thread_id is not a string.
            r#"{"type":"session_meta","payload":{"id":"s1","source":{"subagent":{"thread_spawn":{"parent_thread_id":42}}}}}"#,
            // parent_thread_id is empty.
            r#"{"type":"session_meta","payload":{"id":"s1","source":{"subagent":{"thread_spawn":{"parent_thread_id":""}}}}}"#,
            // parent_thread_id missing.
            r#"{"type":"session_meta","payload":{"id":"s1","source":{"subagent":{"thread_spawn":{"depth":1}}}}}"#,
        ];
        for line in cases {
            let input = format!(
                "{line}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}}}}}"
            );
            let output = parser.parse(&input, &location());
            assert_eq!(output.events().len(), 1, "line: {line}");
            assert_eq!(
                output.events()[0].parent_session(),
                None,
                "malformed source must yield no parent: {line}"
            );
            assert!(
                output.quarantined().is_empty(),
                "malformed source must not quarantine: {line}"
            );
        }
    }
}
