//! Character-count reconstruction for Antigravity (`agy`) transcripts.
//!
//! Antigravity's `transcript_full.jsonl` records no token counts. This parser
//! therefore ports the existing reconstruction used by `bin/agent-tokens` and
//! names it `agy-character-count` version `1`:
//!
//! * each `PLANNER_RESPONSE` is one model turn;
//! * input is 34,492 fixed tokens plus all preceding transcript characters / 4;
//! * output is the current turn's `content`, `thinking`, and JSON `tool_calls`
//!   characters / 4;
//! * every step, including a model turn, joins the history only after that
//!   turn's estimate, because it can be input only to a later call.
//!
//! Character counts are Unicode scalar values, matching Python's `len(str)` for
//! the ordinary transcript strings. `tool_calls` uses Python-compatible JSON
//! separator widths (comma-space and colon-space), so the port stays comparable
//! to the estimator it replaces. Integer division rounds down, as the original
//! did. Both constants come from the 2026-08-14 measurement recorded for the
//! predecessor estimator, rather than from a provider claim. Four characters
//! per token is that estimator's approximation. The fixed floor covers the
//! prompt material absent from the transcript: the project instructions, system
//! prompt, and tool definitions resent on every call. The measurement found
//! 34,492 tokens for the Anthropic-backed path and 27,525 for gpt-oss. Because
//! the transcript stores no model identity, version 1 uses the larger value for
//! every call instead of silently selecting the smaller floor.
//!
//! Input grows quadratically across a conversation by design: each turn pays
//! the fixed floor plus all prior transcript characters because the whole
//! history is resent. A `PLANNER_RESPONSE` is the model turn because sampled
//! transcripts carried `thinking` on exactly those steps; other step types are
//! material a later model turn reads. These observations and constants define
//! version 1, so changing any of them requires a new estimator version.
//!
//! The known error has no defensible event-level bound: output is understated,
//! compaction is invisible and can overstate input, and the source exposes no
//! model attribution. Events are therefore explicitly estimated with no
//! uncertainty interval, never presented as measured and never decorated with
//! an invented range.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, UsageVector,
};
use crate::evidence::{CoverageCompleteness, EstimatorId, EvidenceQuality, Provenance};
use crate::transcripts::parser::{
    EstimatorVersion, EvidenceClassification, InputFormatVersion, NormalizedUsageEvent,
    ParseOutput, ParserAdapter, ParserVersion, QuarantineClass, QuarantineRecord, SourceLocation,
};

pub const AGY_NAMESPACE: &str = "agy";
pub const AGY_ESTIMATOR_ID: &str = "agy-character-count";
pub const AGY_ESTIMATOR_VERSION: &str = "1";

const AGY_CALL_FLOOR_TOKENS: u64 = 34_492;
const AGY_CHARACTERS_PER_TOKEN: u64 = 4;
const AGY_MODEL_BUCKET: &str = "antigravity";

/// Parser for Antigravity's `transcript_full.jsonl` step stream.
pub struct AgyParser;

impl ParserAdapter for AgyParser {
    fn parser_version(&self) -> ParserVersion {
        ParserVersion::new("agy-1")
    }

    fn input_format_version(&self) -> InputFormatVersion {
        InputFormatVersion::new("agy-transcript-jsonl-v1")
    }

    fn parse(&self, input: &str, location: &SourceLocation) -> ParseOutput {
        let mut events = Vec::new();
        let mut quarantined = Vec::new();
        let mut history_characters = 0u64;
        let session = agy_session_from_path(location.file());

        for (index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record_location =
                SourceLocation::new(location.file().to_string(), location.line() + index as u64);
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => {
                    quarantined.push(QuarantineRecord::new(
                        record_location,
                        self.parser_version(),
                        QuarantineClass::TruncatedStructure,
                    ));
                    continue;
                }
            };
            let step_characters = match agy_step_character_count(&value) {
                Ok(count) => count,
                Err(class) => {
                    quarantined.push(QuarantineRecord::new(
                        record_location,
                        self.parser_version(),
                        class,
                    ));
                    continue;
                }
            };

            if value.get("type").and_then(Value::as_str) == Some("PLANNER_RESPONSE") {
                match agy_estimated_event(
                    &value,
                    location.file(),
                    history_characters,
                    step_characters,
                    session.clone(),
                    self.parser_version(),
                ) {
                    Ok(event) => events.push(event),
                    Err(class) => quarantined.push(QuarantineRecord::new(
                        record_location,
                        self.parser_version(),
                        class,
                    )),
                }
            }
            history_characters = history_characters.saturating_add(step_characters);
        }

        ParseOutput::new(events, quarantined)
    }
}

fn agy_estimated_event(
    step: &Value,
    source_file: &str,
    history_characters: u64,
    step_characters: u64,
    session: Option<SessionId>,
    parser_version: ParserVersion,
) -> Result<NormalizedUsageEvent, QuarantineClass> {
    let occurred_at = step
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(UtcTimestamp::parse_rfc3339)
        .ok_or(QuarantineClass::MissingRequiredField)?;
    let estimator = EstimatorId::new(AGY_ESTIMATOR_ID);
    let input = AGY_CALL_FLOOR_TOKENS.saturating_add(history_characters / AGY_CHARACTERS_PER_TOKEN);
    let output = step_characters / AGY_CHARACTERS_PER_TOKEN;
    let usage = UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        ),
        BTreeMap::new(),
        CoverageCompleteness::Complete,
        EvidenceQuality::estimated([estimator.clone()], None),
    );
    let mut event = NormalizedUsageEvent::new(
        usage,
        EvidenceClassification::Reconstructed {
            estimator,
            version: EstimatorVersion::new(AGY_ESTIMATOR_VERSION),
        },
        Provenance::new([source_file.to_string(), format!("model:{AGY_MODEL_BUCKET}")]),
        parser_version,
    )
    .with_occurred_at(occurred_at);
    if let Some(session) = session {
        event = event.with_session(session);
    }
    Ok(event)
}

fn agy_session_from_path(source_file: &str) -> Option<SessionId> {
    let path = Path::new(source_file);
    let native = path
        .ancestors()
        .find(|candidate| {
            candidate
                .file_name()
                .is_some_and(|name| name == ".system_generated")
        })
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())?;
    Some(SessionId::new(
        SourceNamespace::new(AGY_NAMESPACE),
        NativeSessionId::new(native),
    ))
}

fn agy_step_character_count(step: &Value) -> Result<u64, QuarantineClass> {
    let object = step.as_object().ok_or(QuarantineClass::WrongFieldType)?;
    if object.get("type").is_some_and(|value| !value.is_string()) {
        return Err(QuarantineClass::WrongFieldType);
    }
    let mut count = 0u64;
    for field in ["content", "thinking"] {
        if let Some(value) = object.get(field) {
            let text = value.as_str().ok_or(QuarantineClass::WrongFieldType)?;
            count = count.saturating_add(text.chars().count() as u64);
        }
    }
    if let Some(tool_calls) = object.get("tool_calls")
        && agy_json_is_truthy(tool_calls)
    {
        count = count.saturating_add(agy_python_json_character_count(tool_calls));
    }
    Ok(count)
}

fn agy_json_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn agy_python_json_character_count(value: &Value) -> u64 {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().chars().count() as u64,
        Value::String(text) => serde_json::to_string(text)
            .expect("serializing a JSON string cannot fail")
            .chars()
            .count() as u64,
        Value::Array(items) => {
            2 + items
                .iter()
                .map(agy_python_json_character_count)
                .sum::<u64>()
                + 2 * items.len().saturating_sub(1) as u64
        }
        Value::Object(fields) => {
            2 + fields
                .iter()
                .map(|(key, value)| {
                    serde_json::to_string(key)
                        .expect("serializing a JSON object key cannot fail")
                        .chars()
                        .count() as u64
                        + 2
                        + agy_python_json_character_count(value)
                })
                .sum::<u64>()
                + 2 * fields.len().saturating_sub(1) as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::canonical_identity;
    use crate::evidence::EvidenceQuality;

    fn agy_test_location() -> SourceLocation {
        SourceLocation::new(
            "brain/session-a/.system_generated/logs/transcript_full.jsonl",
            1,
        )
    }

    fn agy_test_input() -> &'static str {
        r#"{"type":"USER_INPUT","content":"abcdefgh"}
{"type":"PLANNER_RESPONSE","created_at":"2026-09-20T10:00:00Z","content":"abcd","thinking":"efgh"}
{"type":"TOOL_RESULT","content":"ijkl"}
{"type":"PLANNER_RESPONSE","created_at":"2026-09-20T10:01:00Z","content":"mnopqrst","tool_calls":[{"name":"read","args":{"path":"x"}}]}"#
    }

    #[test]
    fn agy_events_carry_the_named_estimator_version_and_no_invented_interval() {
        let output = AgyParser.parse(agy_test_input(), &agy_test_location());
        assert!(output.quarantined().is_empty());
        assert_eq!(output.events().len(), 2);
        for event in output.events() {
            let (estimator, version) = event
                .classification()
                .reconstructed()
                .expect("agy events are reconstructed");
            assert_eq!(estimator.as_str(), "agy-character-count");
            assert_eq!(version.as_str(), "1");
            match event.usage().quality() {
                EvidenceQuality::Estimated {
                    methods,
                    uncertainty,
                } => {
                    assert_eq!(
                        methods.iter().map(EstimatorId::as_str).collect::<Vec<_>>(),
                        ["agy-character-count"]
                    );
                    assert_eq!(uncertainty, &None);
                }
                quality @ (EvidenceQuality::Measured | EvidenceQuality::Mixed { .. }) => {
                    panic!("agy event must be estimated, got {quality:?}")
                }
            }
        }
    }

    #[test]
    fn agy_reconstruction_is_deterministic_and_accumulates_prior_history() {
        let first = AgyParser.parse(agy_test_input(), &agy_test_location());
        let second = AgyParser.parse(agy_test_input(), &agy_test_location());
        assert_eq!(first, second);
        let known: Vec<(u64, u64)> = first
            .events()
            .iter()
            .map(|event| {
                (
                    event.usage().known().input().value(),
                    event.usage().known().output().value(),
                )
            })
            .collect();
        assert_eq!(known[0], (34_494, 2));
        assert!(
            known[1].0 > known[0].0,
            "later input includes earlier history"
        );
    }

    #[test]
    fn agy_events_report_heuristic_identity() {
        let output = AgyParser.parse(agy_test_input(), &agy_test_location());
        for event in output.events() {
            let identity = canonical_identity(event);
            assert!(identity.native_event_id.is_none());
            assert!(identity.heuristic_key.is_some());
            assert!(identity.canonical_event_id.starts_with("heuristic:agy-1:"));
        }
    }

    #[test]
    fn measured_and_agy_quality_combine_as_mixed() {
        let output = AgyParser.parse(agy_test_input(), &agy_test_location());
        let combined = EvidenceQuality::Measured.combine(output.events()[0].usage().quality());
        assert!(matches!(combined, EvidenceQuality::Mixed { .. }));
    }

    #[test]
    fn malformed_or_wrong_typed_steps_quarantine_without_hiding_valid_turns() {
        let input = concat!(
            "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-09-20T10:00:00Z\",\"content\":\"abcd\"}\n",
            "{not-json}\n",
            "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-09-20T10:01:00Z\",\"content\":17}\n"
        );
        let output = AgyParser.parse(input, &agy_test_location());
        assert_eq!(output.events().len(), 1);
        assert_eq!(output.quarantined().len(), 2);
        assert_eq!(
            output.quarantined()[0].class(),
            QuarantineClass::TruncatedStructure
        );
        assert_eq!(
            output.quarantined()[1].class(),
            QuarantineClass::WrongFieldType
        );
    }

    #[test]
    fn python_json_character_count_keeps_separator_and_unicode_widths() {
        let value = serde_json::json!([{"name": "lé", "ok": true}, null]);
        let python_shape = r#"[{"name": "lé", "ok": true}, null]"#;
        assert_eq!(
            agy_python_json_character_count(&value),
            python_shape.chars().count() as u64
        );
    }

    #[test]
    fn empty_tool_calls_add_no_serialized_characters() {
        let without = serde_json::json!({"content": "abcd"});
        let empty = serde_json::json!({"content": "abcd", "tool_calls": []});
        assert_eq!(
            agy_step_character_count(&without),
            agy_step_character_count(&empty)
        );
    }
}
