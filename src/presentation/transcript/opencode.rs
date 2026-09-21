//! The opencode transcript renderer behind `aub export transcript` (`aub-m76e`).
//!
//! Shapes below were read off the operator's live `opencode.db` on 2026-09-21
//! before the renderer was written (the meter-adapter rule: a renderer written
//! from memory of a format is the fixture-agrees-with-itself failure). The
//! database holds a `session` table, a `message` table (`id`, `session_id`,
//! row times, `data` JSON carrying `role` among usage fields) and a `part`
//! table (`id`, `message_id`, `session_id`, row times, `data` JSON). Part
//! `type` values on the live database: `text`, `tool`, `reasoning`,
//! `step-start`, `step-finish`, `patch`, `file`, `compaction`, `subtask`.
//!
//! A `text` part carries the visible prose in `text`. A `tool` part carries
//! the whole call in one row: the name in `tool`, the input in
//! `state.input`, and the result in `state.output` when the status is
//! `completed`; an `error` status carries the failure text under
//! `state.error` with no `output` key, and a still-open status (`running`,
//! `pending`) carries neither, so the call renders with no result block until
//! its row lands. A `reasoning` part carries readable thinking in `text` on
//! some rows and an empty string with only opaque metadata on others; the
//! empty ones contribute nothing under any flag, the way the codex renderer
//! treats opaque reasoning. `step-start` and `step-finish` are run markers
//! (the finish row carries the step's token and cost usage), classified the
//! way the codex renderer classifies its setup envelopes: understood, never
//! counted. Every other part type is counted by type and reported once per
//! type by the caller, never dropped silently and never fatal.
//!
//! The renderer reads text, not the database: the caller (`export transcript`
//! in `cli.rs`) reads the session's rows through the store's one opencode
//! connection function and formats one interchange line per row with
//! [`opencode_line`] - `{"message": <id>, "role": <role>, "data": <part.data
//! document>}` - and this module parses those lines. The database stays
//! behind the store boundary (rule 09), and the line format keeps the trait
//! funnel the file-backed renderers use.
//!
//! May not depend on:
//! - provider adapters
//! - store or calibration (boundary rule 09)
//! - the system clock or the filesystem

use std::collections::BTreeMap;

use super::{
    TranscriptMessage, TranscriptRenderer, TranscriptRole, TranscriptToolCall,
    clean_transcript_text,
};

/// Renders opencode transcript sessions (`session.source == "opencode"`).
pub struct OpencodeTranscriptRenderer;

impl TranscriptRenderer for OpencodeTranscriptRenderer {
    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn render_file(&self, body: &str) -> Vec<TranscriptMessage> {
        render_opencode_body_with_skipped(body).0
    }

    fn render_file_with_skipped(
        &self,
        body: &str,
    ) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
        render_opencode_body_with_skipped(body)
    }
}

/// One interchange line for a store row: the message id groups the parts of
/// one turn, the role is the `message.data` role string, and `data` is the
/// raw `part.data` document. A part row whose stored JSON does not parse
/// degrades to a null document, which the reader counts as an untyped part
/// rather than failing the whole session.
pub fn opencode_line(message_id: &str, role: &str, part_data: &str) -> String {
    let data: serde_json::Value =
        serde_json::from_str(part_data).unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "message": message_id,
        "role": role,
        "data": data,
    })
    .to_string()
}

/// The message the parts are currently landing in: its row id and its index
/// in the output, or neither while the reader is inside a message whose role
/// was counted as skipped.
struct OpencodeActive {
    id: String,
    index: Option<usize>,
}

fn render_opencode_body_with_skipped(
    body: &str,
) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let mut active: Option<OpencodeActive> = None;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            *skipped.entry("unparseable".to_string()).or_insert(0) += 1;
            continue;
        };
        let Some(message_id) = value.get("message").and_then(serde_json::Value::as_str) else {
            *skipped.entry("message:untyped".to_string()).or_insert(0) += 1;
            continue;
        };
        let boundary = active.as_ref().is_none_or(|open| open.id != message_id);
        if boundary {
            let role = value.get("role").and_then(serde_json::Value::as_str);
            match role {
                Some("user" | "assistant") => {
                    let transcript_role = if role == Some("user") {
                        TranscriptRole::User
                    } else {
                        TranscriptRole::Assistant
                    };
                    messages.push(TranscriptMessage {
                        role: transcript_role,
                        text: String::new(),
                        tool_calls: Vec::new(),
                        thinking: Vec::new(),
                    });
                    active = Some(OpencodeActive {
                        id: message_id.to_string(),
                        index: Some(messages.len() - 1),
                    });
                }
                Some(unknown) => {
                    *skipped.entry(format!("role:{unknown}")).or_insert(0) += 1;
                    active = Some(OpencodeActive {
                        id: message_id.to_string(),
                        index: None,
                    });
                }
                None => {
                    *skipped.entry("role:untyped".to_string()).or_insert(0) += 1;
                    active = Some(OpencodeActive {
                        id: message_id.to_string(),
                        index: None,
                    });
                }
            }
            if active.as_ref().is_none_or(|open| open.index.is_none()) {
                continue;
            }
        }
        let Some(index) = active.as_ref().and_then(|open| open.index) else {
            // The message's role was counted as skipped at its boundary; its
            // parts belong to that same skip, not to a new one.
            continue;
        };
        read_opencode_part(value.get("data"), &mut messages[index], &mut skipped);
    }

    messages.retain(|message| {
        !(message.text.is_empty() && message.tool_calls.is_empty() && message.thinking.is_empty())
    });
    (messages, skipped)
}

/// One part document inside its message: text, tool traffic and readable
/// thinking render, step markers are classified setup, and any other type is
/// counted by name rather than dropped silently.
fn read_opencode_part(
    data: Option<&serde_json::Value>,
    message: &mut TranscriptMessage,
    skipped: &mut BTreeMap<String, usize>,
) {
    let Some(part) = data.and_then(serde_json::Value::as_object) else {
        *skipped.entry("part:untyped".to_string()).or_insert(0) += 1;
        return;
    };
    match part.get("type").and_then(serde_json::Value::as_str) {
        Some("text") => {
            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                && let Some(kept) = clean_transcript_text(text)
            {
                if !message.text.is_empty() {
                    message.text.push_str("\n\n");
                }
                message.text.push_str(&kept);
            }
        }
        Some("tool") => message.tool_calls.push(TranscriptToolCall {
            name: part
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("(unknown)")
                .to_string(),
            input: part
                .get("state")
                .and_then(|state| state.get("input"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            result: opencode_tool_result(part),
        }),
        Some("reasoning") => {
            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                && let Some(kept) = clean_transcript_text(text)
            {
                message.thinking.push(kept);
            }
        }
        // Step markers frame the run (the finish row carries the step's token
        // and cost usage); they are classified, not conversation.
        Some("step-start" | "step-finish") => {}
        Some(part_type) => {
            *skipped.entry(format!("part:{part_type}")).or_insert(0) += 1;
        }
        None => {
            *skipped.entry("part:untyped".to_string()).or_insert(0) += 1;
        }
    }
}

/// The full result text of one tool part: the `output` string on a finished
/// call, the `error` string on a failed one (which carries no `output` key),
/// empty while the call is still open. Never truncated.
fn opencode_tool_result(part: &serde_json::Map<String, serde_json::Value>) -> String {
    let state = part.get("state");
    match state.and_then(|state| state.get("output")) {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Null) | None => state
            .and_then(|state| state.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TranscriptRenderOptions, render_transcript_markdown};
    use super::*;

    fn render(body: &str) -> Vec<TranscriptMessage> {
        OpencodeTranscriptRenderer.render_file(body)
    }

    fn rendered(body: &str, include_tools: bool, include_thinking: bool) -> String {
        let (messages, _) = OpencodeTranscriptRenderer.render_file_with_skipped(body);
        let document = super::super::TranscriptDocument {
            harness: "opencode".to_string(),
            project: "fixture-project".to_string(),
            session_id: "ses_m76e_fixture".to_string(),
            started: crate::domain::time::UtcTimestamp::from_unix_nanos(1_788_220_900_000_000_000),
            files: vec![super::super::TranscriptFile {
                file_name: String::new(),
                is_subagent: false,
                messages,
            }],
        };
        render_transcript_markdown(
            &document,
            &TranscriptRenderOptions {
                include_tools,
                include_thinking,
            },
        )
    }

    fn seed_value() -> serde_json::Value {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/transcripts/opencode/transcript_seed.json"),
        )
        .expect("the opencode transcript seed must exist");
        serde_json::from_str(&text).expect("the opencode transcript seed must parse as JSON")
    }

    /// The committed seed as interchange lines, in seed order: the same lines
    /// the export path formats from the store rows, so the goldens below pin
    /// the database-to-markdown shape rather than a hand-written one.
    fn fixture_body() -> String {
        let seed = seed_value();
        let mut lines = Vec::new();
        for part in seed["parts"].as_array().expect("seed must hold parts") {
            let message_id = part["message_id"].as_str().expect("part message id");
            let role = seed["messages"]
                .as_array()
                .expect("seed must hold messages")
                .iter()
                .find(|message| message["id"].as_str() == Some(message_id))
                .and_then(|message| message["data"]["role"].as_str())
                .expect("every fixture part belongs to a message with a role");
            lines.push(opencode_line(
                message_id,
                role,
                &serde_json::to_string(&part["data"]).expect("part data must serialize"),
            ));
        }
        lines.join("\n")
    }

    fn golden(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/transcripts/opencode")
                .join(name),
        )
        .unwrap_or_else(|_| panic!("the opencode golden {name} must exist"))
    }

    /// The three goldens pin the fixture's rendering under each flag
    /// combination: plain conversation, whole tool traffic, and thinking.
    #[test]
    fn golden_plain_renders_the_fixture() {
        assert_eq!(rendered(&fixture_body(), false, false), golden("plain.md"));
    }

    #[test]
    fn golden_tools_renders_the_fixture() {
        assert_eq!(rendered(&fixture_body(), true, false), golden("tools.md"));
    }

    #[test]
    fn golden_thinking_renders_the_fixture() {
        assert_eq!(
            rendered(&fixture_body(), false, true),
            golden("thinking.md")
        );
    }

    /// The plain rendering shows user and assistant text with no tool or
    /// thinking traffic: the tool call and the reasoning block need theirs.
    #[test]
    fn plain_rendering_has_three_sections_and_no_tool_or_thinking() {
        let out = rendered(&fixture_body(), false, false);
        assert!(out.contains("## User\nRender the usage summary for review."));
        assert!(out.contains("## Assistant\nI will read the ledger first."));
        assert!(out.contains("## Assistant\nThe summary is ready for review."));
        assert!(!out.contains("**Tool:"), "tool traffic needs the flag");
        assert!(
            !out.contains("The ledger numbers check out"),
            "thinking needs the flag"
        );
        assert!(
            !out.contains("Week 38 opened"),
            "the tool output needs the flag"
        );
    }

    /// The whole tool output survives under `--include-tools`, paired with
    /// its call in the same row: head and tail prove nothing was cut.
    #[test]
    fn tools_rendering_keeps_the_full_output_intact() {
        let out = rendered(&fixture_body(), true, false);
        assert!(out.contains("**Tool: read**"));
        assert!(out.contains("\"path\": \"/work/fixture-project/summary.md\""));
        assert!(out.contains("Week 38 opened with three providers"));
        assert!(out.contains("Trailing line to prove the output survives whole"));
        assert!(
            !out.contains("The ledger numbers check out"),
            "thinking still needs its own flag"
        );
    }

    /// The reasoning block renders as a blockquote, only under its flag.
    #[test]
    fn thinking_rendering_adds_the_blockquote_only_with_the_flag() {
        let out = rendered(&fixture_body(), false, true);
        assert!(out.contains("> The ledger numbers check out against the meter"));
        assert!(!out.contains("**Tool:"));
    }

    /// The fixture the bead requires: one user message, two assistant turns
    /// holding a tool call with its output and a reasoning block, with a
    /// multi-line tool output. This scans the seed itself, so deleting a
    /// covered kind fails here rather than hiding inside a golden diff.
    #[test]
    fn the_fixture_covers_a_user_message_a_tool_call_with_its_output_and_a_reasoning_block() {
        let seed = seed_value();
        let mut user = false;
        let mut assistant = 0;
        let mut call = false;
        let mut output = false;
        let mut longest_output = 0;
        let mut thinking = false;
        for message in seed["messages"].as_array().expect("seed messages") {
            match message["data"]["role"].as_str() {
                Some("user") => user = true,
                Some("assistant") => assistant += 1,
                other => panic!("fixture roles stay user or assistant, found {other:?}"),
            }
        }
        for part in seed["parts"].as_array().expect("seed parts") {
            match part["data"]["type"].as_str() {
                Some("text") => {}
                Some("tool") => {
                    call = true;
                    if let Some(text) = part["data"]["state"]["output"].as_str() {
                        output = true;
                        longest_output = longest_output.max(text.len());
                    }
                }
                Some("reasoning") => {
                    if part["data"]["text"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                    {
                        thinking = true;
                    }
                }
                Some("step-start" | "step-finish") => {}
                other => panic!("fixture part types stay known, found {other:?}"),
            }
        }
        assert!(user, "the fixture holds a user message");
        assert_eq!(assistant, 2, "the fixture holds two assistant messages");
        assert!(call, "the fixture holds a tool call");
        assert!(output, "the fixture holds the call output");
        assert!(thinking, "the fixture holds a reasoning block");
        assert!(
            longest_output >= 500,
            "the fixture tool output is {longest_output} characters, too short to prove no truncation"
        );
    }

    /// Each part kind in isolation: text lands as prose, a finished tool part
    /// as a call with its result, readable reasoning as thinking.
    #[test]
    fn each_part_kind_lands_in_its_own_slot() {
        let body = [
            opencode_line("m1", "user", r#"{"type":"text","text":"Hi"}"#),
            opencode_line(
                "m2",
                "assistant",
                r#"{"type":"text","text":"Working"}"#,
            ),
            opencode_line(
                "m2",
                "assistant",
                r#"{"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls"},"output":"out"}}"#,
            ),
            opencode_line(
                "m2",
                "assistant",
                r#"{"type":"reasoning","text":"Plan first"}"#,
            ),
        ]
        .join("\n");
        let messages = render(&body);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "Hi");
        assert_eq!(messages[1].text, "Working");
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].name, "bash");
        assert_eq!(messages[1].tool_calls[0].result, "out");
        assert_eq!(messages[1].thinking, vec!["Plan first".to_string()]);
    }

    /// A failed tool part carries its failure text under `error` with no
    /// `output` key: the error text is the result. The planted negative pins
    /// the `error` arm: without it the failure would render with no result.
    #[test]
    fn a_failed_tool_part_renders_its_error_as_the_result() {
        let body = opencode_line(
            "m1",
            "assistant",
            r#"{"type":"tool","tool":"skill","callID":"c9","state":{"status":"error","input":{"name":"list"},"error":"Skill or command \"list\" not found."}}"#,
        );
        let messages = render(&body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(
            messages[0].tool_calls[0].result,
            "Skill or command \"list\" not found."
        );
    }

    /// A still-open tool part carries neither output nor error: the call
    /// renders with no result block rather than an invented one.
    #[test]
    fn an_open_tool_part_renders_the_call_with_no_result() {
        let body = opencode_line(
            "m1",
            "assistant",
            r#"{"type":"tool","tool":"task","callID":"c3","state":{"status":"running","input":{"description":"Wave 4"}}}"#,
        );
        let messages = render(&body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].result, "");
    }

    /// Reasoning with empty text (the opaque rows the live database holds
    /// beside readable ones) contributes no thinking entry under any flag.
    #[test]
    fn empty_reasoning_contributes_nothing() {
        let body = opencode_line(
            "m1",
            "assistant",
            r#"{"type":"reasoning","text":"","time":{"start":1,"end":2}}"#,
        );
        let messages = render(&body);
        assert!(
            messages.is_empty(),
            "an opaque reasoning row leaves no message behind"
        );
    }

    /// Parts group by message id: two assistant messages stay two messages,
    /// and a message holding only step markers leaves no empty section.
    #[test]
    fn parts_group_by_message_and_marker_only_messages_vanish() {
        let body = [
            opencode_line("m1", "assistant", r#"{"type":"text","text":"First"}"#),
            opencode_line("m2", "assistant", r#"{"type":"step-start","snapshot":"x"}"#),
            opencode_line("m3", "assistant", r#"{"type":"text","text":"Third"}"#),
        ]
        .join("\n");
        let messages = render(&body);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "First");
        assert_eq!(messages[1].text, "Third");
    }

    /// The step markers are classified setup: they render nothing and count
    /// nothing, while an unknown part type counts by name. The planted
    /// negative pins the classification over mere presence: without the step
    /// arm the markers would report as skipped on every live export.
    #[test]
    fn step_markers_are_classified_and_unknown_part_types_are_counted() {
        let body = [
            opencode_line("m1", "assistant", r#"{"type":"step-start","snapshot":"x"}"#),
            opencode_line(
                "m1",
                "assistant",
                r#"{"type":"step-finish","reason":"tool-calls"}"#,
            ),
            opencode_line("m1", "assistant", r#"{"type":"patch","files":[]}"#),
            opencode_line("m2", "assistant", r#"{"type":"text","text":"Kept"}"#),
        ]
        .join("\n");
        let (messages, skipped) = OpencodeTranscriptRenderer.render_file_with_skipped(&body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Kept");
        assert_eq!(skipped.get("part:patch"), Some(&1));
        assert!(
            !skipped.keys().any(|kind| kind.contains("step")),
            "classified markers are never counted: {skipped:?}"
        );
    }

    /// An unknown message role is counted once per message, and its parts
    /// take nothing down with them: the known message beside it still renders.
    #[test]
    fn unknown_roles_are_counted_once_per_message() {
        let body = [
            opencode_line("m1", "system", r#"{"type":"text","text":"bg"}"#),
            opencode_line("m1", "system", r#"{"type":"text","text":"bg again"}"#),
            opencode_line("m2", "assistant", r#"{"type":"text","text":"Still here"}"#),
        ]
        .join("\n");
        let (messages, skipped) = OpencodeTranscriptRenderer.render_file_with_skipped(&body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Still here");
        assert_eq!(skipped.get("role:system"), Some(&1));
    }

    /// Lines the reader has no case for are counted: a line that is not JSON
    /// at all, a line with no message id, and a part document with no type.
    /// The planted negative pins the counting over mere presence: two lines
    /// of one kind count two.
    #[test]
    fn unreadable_lines_are_counted_once_per_kind() {
        let body = [
            "not json at all".to_string(),
            "not json at all".to_string(),
            opencode_line("m1", "assistant", r#"{"no_type_here":true}"#),
        ]
        .join("\n");
        let (messages, skipped) = OpencodeTranscriptRenderer.render_file_with_skipped(&body);
        assert!(messages.is_empty());
        assert_eq!(skipped.get("unparseable"), Some(&2));
        assert_eq!(skipped.get("part:untyped"), Some(&1));
    }

    /// A part row whose stored JSON does not parse degrades to an untyped
    /// part, never to a failed session.
    #[test]
    fn a_corrupt_part_row_counts_as_untyped_never_fatal() {
        let body = opencode_line("m1", "assistant", "{broken json");
        let (messages, skipped) = OpencodeTranscriptRenderer.render_file_with_skipped(&body);
        assert!(messages.is_empty());
        assert_eq!(skipped.get("part:untyped"), Some(&1));
    }

    /// System reminders are dropped from opencode text the same way the
    /// file-backed renderers drop them.
    #[test]
    fn system_reminders_are_dropped_but_prose_is_kept() {
        let body = opencode_line(
            "m1",
            "user",
            r#"{"type":"text","text":"Keep this. <system-reminder>drop this</system-reminder>"}"#,
        );
        let messages = render(&body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Keep this.");
    }
}
