//! The codex transcript renderer behind `aub export transcript`
//! (`aub-51wv`).
//!
//! Field names and shapes below were read off real rollout files on this
//! machine before the renderer was written (the meter-adapter rule: a
//! renderer written from memory of a format is the fixture-agrees-with-itself
//! failure). Every line is `{timestamp, type, payload}`: a `session_meta`
//! header first, then `response_item` payloads whose `type` distinguishes
//! `message` (roles `developer`/`user`/`assistant`, content kinds
//! `input_text`/`output_text`), `reasoning` (`summary`, `encrypted_content`),
//! `custom_tool_call`/`function_call` (`name`, `input`/`arguments`,
//! `call_id`) and their `custom_tool_call_output`/`function_call_output`
//! (`call_id`, `output: [{type: input_text, text}]`), plus `event_msg`
//! (`task_started`, `user_message`, `agent_message`, `token_count`,
//! `patch_apply_end`, `task_complete`, `context_compacted`) and the
//! file-level `turn_context`, `world_state` and `compacted` records.
//!
//! Conversation comes only from `response_item` messages: `event_msg`
//! carries the same user and assistant text a second time (as `user_message`
//! and `agent_message`), so rendering both would duplicate every turn.
//! `developer` messages are session instructions, not conversation, and are
//! dropped the way the claude-code renderer drops system reminders.
//!
//! Reasoning is opaque on the files observed: every `reasoning` payload
//! carried `summary: []` with the reasoning inside `encrypted_content`,
//! which is never rendered. The renderer still owns the `reasoning` case
//! (so those lines are never counted as skipped) and surfaces `summary`
//! text items when a file carries them, under `--include-thinking`.
//!
//! A line whose type the renderer has no case for is counted by its most
//! specific type label and reported once per type by the caller, never
//! dropped silently and never fatal.
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

/// Renders codex transcript files (`session.source == "codex"`).
pub struct CodexTranscriptRenderer;

impl TranscriptRenderer for CodexTranscriptRenderer {
    fn harness(&self) -> &'static str {
        "codex"
    }

    fn render_file(&self, body: &str) -> Vec<TranscriptMessage> {
        render_codex_file_with_skipped(body).0
    }

    fn render_file_with_skipped(
        &self,
        body: &str,
    ) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
        render_codex_file_with_skipped(body)
    }
}

/// One tool call still waiting for its output, paired by `call_id`.
struct PendingCodexToolUse {
    message_index: usize,
    call_index: usize,
}

fn render_codex_file_with_skipped(body: &str) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut pending: BTreeMap<String, PendingCodexToolUse> = BTreeMap::new();
    // Outputs whose call never appeared, kept in arrival order per message.
    let mut orphans: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            *skipped.entry("unparseable".to_string()).or_insert(0) += 1;
            continue;
        };
        let top = value.get("type").and_then(serde_json::Value::as_str);
        match top {
            Some("response_item") => read_codex_response_item(
                &value,
                &mut messages,
                &mut pending,
                &mut orphans,
                &mut skipped,
            ),
            // The conversation channel is `response_item` alone: `event_msg`
            // repeats user and assistant text and carries usage, so the whole
            // envelope is an understood skip rather than per-kind counting.
            Some("event_msg") | Some("session_meta") | Some("turn_context")
            | Some("world_state") | Some("compacted") => {}
            Some(unknown) => {
                *skipped.entry(unknown.to_string()).or_insert(0) += 1;
            }
            None => {
                *skipped.entry("untyped".to_string()).or_insert(0) += 1;
            }
        }
    }

    // Orphan outputs render as calls with no recorded input of their own:
    // the output text is transcript content and is never dropped silently.
    for (index, results) in orphans {
        for result in results {
            messages[index].tool_calls.push(TranscriptToolCall {
                name: "(unknown)".to_string(),
                input: serde_json::Value::Null,
                result,
            });
        }
    }
    (messages, skipped)
}

/// Reads one `response_item` line: messages, reasoning and tool traffic render,
/// a payload type with no case here is counted, never dropped silently.
fn read_codex_response_item(
    value: &serde_json::Value,
    messages: &mut Vec<TranscriptMessage>,
    pending: &mut BTreeMap<String, PendingCodexToolUse>,
    orphans: &mut BTreeMap<usize, Vec<String>>,
    skipped: &mut BTreeMap<String, usize>,
) {
    let payload = match value.get("payload").and_then(serde_json::Value::as_object) {
        Some(payload) => payload,
        None => {
            *skipped.entry("response_item".to_string()).or_insert(0) += 1;
            return;
        }
    };
    match payload.get("type").and_then(serde_json::Value::as_str) {
        Some("message") => read_codex_message(payload, messages),
        Some("reasoning") => read_codex_reasoning(payload, messages),
        Some("custom_tool_call" | "function_call") => {
            read_codex_tool_call(payload, messages, pending);
        }
        Some("custom_tool_call_output" | "function_call_output") => {
            read_codex_tool_output(payload, messages, pending, orphans);
        }
        Some(unknown) => {
            *skipped.entry(unknown.to_string()).or_insert(0) += 1;
        }
        None => {
            *skipped.entry("response_item".to_string()).or_insert(0) += 1;
        }
    }
}

/// One `message` payload: `user` and `assistant` roles render, `developer`
/// instructions and any other role do not.
fn read_codex_message(
    payload: &serde_json::Map<String, serde_json::Value>,
    messages: &mut Vec<TranscriptMessage>,
) {
    let role = match payload.get("role").and_then(serde_json::Value::as_str) {
        Some("user") => TranscriptRole::User,
        Some("assistant") => TranscriptRole::Assistant,
        _ => return,
    };
    let Some(blocks) = payload.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    let kind = match role {
        TranscriptRole::User => "input_text",
        TranscriptRole::Assistant => "output_text",
    };
    let mut texts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(serde_json::Value::as_str) != Some(kind) {
            continue;
        }
        if let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
            && let Some(kept) = clean_transcript_text(text)
        {
            texts.push(kept);
        }
    }
    if texts.is_empty() {
        return;
    }
    messages.push(TranscriptMessage {
        role,
        text: texts.join("\n\n"),
        tool_calls: Vec::new(),
        thinking: Vec::new(),
    });
}

/// One `reasoning` payload: the `summary` text items a file carries become
/// thinking. Every summary observed on this machine was empty (the reasoning
/// itself travels in `encrypted_content`, which is never rendered), so an
/// empty summary contributes nothing under any flag.
fn read_codex_reasoning(
    payload: &serde_json::Map<String, serde_json::Value>,
    messages: &mut Vec<TranscriptMessage>,
) {
    let Some(items) = payload.get("summary").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut thinking = Vec::new();
    for item in items {
        // Enumerated rather than wildcarded: serde_json::Value is not
        // #[non_exhaustive], so a variant added upstream fails to compile
        // here instead of silently classifying as the wrong shape.
        let text = match item {
            serde_json::Value::String(text) => Some(text.as_str()),
            serde_json::Value::Object(_) => item.get("text").and_then(serde_json::Value::as_str),
            serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Array(_) => None,
        };
        if let Some(text) = text
            && let Some(kept) = clean_transcript_text(text)
        {
            thinking.push(kept);
        }
    }
    if thinking.is_empty() {
        return;
    }
    messages.push(TranscriptMessage {
        role: TranscriptRole::Assistant,
        text: String::new(),
        tool_calls: Vec::new(),
        thinking,
    });
}

/// One `custom_tool_call` or `function_call` payload: the full input is kept,
/// never truncated. A string input that parses as JSON is stored parsed, so
/// the markdown shows the call's arguments as structure; anything else stays
/// a string.
fn read_codex_tool_call(
    payload: &serde_json::Map<String, serde_json::Value>,
    messages: &mut Vec<TranscriptMessage>,
    pending: &mut BTreeMap<String, PendingCodexToolUse>,
) {
    let name = payload
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("(unknown)")
        .to_string();
    let input = codex_tool_input(payload);
    let call_id = payload
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    messages.push(TranscriptMessage {
        role: TranscriptRole::Assistant,
        text: String::new(),
        tool_calls: vec![TranscriptToolCall {
            name,
            input,
            result: String::new(),
        }],
        thinking: Vec::new(),
    });
    if !call_id.is_empty() {
        pending.insert(
            call_id,
            PendingCodexToolUse {
                message_index: messages.len() - 1,
                call_index: 0,
            },
        );
    }
}

/// The full tool input: `input` on a custom call, `arguments` on a function
/// call, parsed when it arrives as a JSON-encoded string.
fn codex_tool_input(payload: &serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let raw = payload
        .get("input")
        .or_else(|| payload.get("arguments"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match raw {
        serde_json::Value::String(text) => {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
        }
        // Enumerated rather than wildcarded, for the same reason as the
        // summary items above: a new variant must fail loudly here.
        other @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_)) => other,
    }
}

/// One `custom_tool_call_output` or `function_call_output` payload: the
/// `output` array's text joined whole, paired back to its call by `call_id`.
fn read_codex_tool_output(
    payload: &serde_json::Map<String, serde_json::Value>,
    messages: &mut Vec<TranscriptMessage>,
    pending: &mut BTreeMap<String, PendingCodexToolUse>,
    orphans: &mut BTreeMap<usize, Vec<String>>,
) {
    let mut texts = Vec::new();
    if let Some(parts) = payload.get("output").and_then(serde_json::Value::as_array) {
        for part in parts {
            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
                texts.push(text.to_string());
            } else if let Some(text) = part.as_str() {
                texts.push(text.to_string());
            }
        }
    }
    let result = texts.join("\n");
    let call_id = payload
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !call_id.is_empty()
        && let Some(found) = pending.get(call_id)
    {
        messages[found.message_index].tool_calls[found.call_index].result = result;
        return;
    }
    let index = messages.len();
    messages.push(TranscriptMessage {
        role: TranscriptRole::Assistant,
        text: String::new(),
        tool_calls: Vec::new(),
        thinking: Vec::new(),
    });
    orphans.entry(index).or_default().push(result);
}

#[cfg(test)]
mod tests {
    use super::super::{TranscriptRenderOptions, render_transcript_markdown};
    use super::*;

    fn render(body: &str) -> Vec<TranscriptMessage> {
        CodexTranscriptRenderer.render_file(body)
    }

    fn rendered(body: &str, include_tools: bool, include_thinking: bool) -> String {
        let (messages, _) = CodexTranscriptRenderer.render_file_with_skipped(body);
        let document = super::super::TranscriptDocument {
            harness: "codex".to_string(),
            project: "proj".to_string(),
            session_id: "sess".to_string(),
            started: crate::domain::time::UtcTimestamp::from_unix_nanos(0),
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

    fn fixture_body() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/transcripts/codex/session.jsonl"),
        )
        .expect("the codex fixture must exist")
    }

    fn golden(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/transcripts/codex")
                .join(name),
        )
        .unwrap_or_else(|_| panic!("the codex golden {name} must exist"))
    }

    /// The three goldens pin the fixture's rendering under each flag
    /// combination: plain conversation, whole tool traffic, and thinking
    /// (which the opaque observed reasoning leaves identical to plain).
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

    /// The bead's golden fixture renders user, assistant, the paired tool
    /// call and nothing else without flags: the empty-summary reasoning and
    /// the event and file-level skip lines contribute no section.
    #[test]
    fn plain_rendering_has_two_sections_and_no_tool_or_thinking() {
        let out = rendered(&fixture_body(), false, false);
        assert!(out.contains("## User\nDo the thing"));
        assert!(out.contains("## Assistant\nOn it"));
        assert!(!out.contains("**Tool:"), "tool traffic needs the flag");
        assert!(!out.contains('o'.to_string().repeat(100).as_str()));
    }

    /// The 2000-character tool output survives whole under
    /// `--include-tools`, paired back to its function call.
    #[test]
    fn tools_rendering_keeps_the_2000_character_output_intact() {
        let big = "o".repeat(2000);
        let out = rendered(&fixture_body(), true, false);
        assert!(out.contains("**Tool: read**"));
        assert!(
            out.contains(&big),
            "the full output survives, never truncated"
        );
        assert!(out.contains("\"/work/project/README.md\""));
        assert!(
            !out.contains("**Tool: (unknown)**"),
            "the output pairs back to its call by call_id"
        );
    }

    /// The observed reasoning shape (empty summary, opaque content) renders
    /// nothing under `--include-thinking`: there is no readable thinking to
    /// show, and the flag must not invent any.
    #[test]
    fn thinking_rendering_shows_no_blockquote_for_the_opaque_reasoning() {
        let out = rendered(&fixture_body(), false, true);
        assert!(!out.contains('>'), "no thinking text, no blockquote");
    }

    /// A `summary` text item a file does carry renders as thinking, only
    /// under the flag. The planted negative pins the flag: without it the
    /// same text stays hidden.
    #[test]
    fn a_summary_text_item_renders_as_thinking_only_with_the_flag() {
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Consider the path\"}],\"encrypted_content\":\"x\"}}\n";
        let hidden = rendered(body, false, false);
        assert!(!hidden.contains("Consider the path"));
        let shown = rendered(body, false, true);
        assert!(shown.contains("> Consider the path"));
    }

    /// The fixture the bead requires: one user message, one assistant
    /// message, one tool call with its output, and one reasoning block, with
    /// a tool output of at least 2000 characters. This scans the fixture
    /// itself, so deleting a covered kind fails here rather than hiding
    /// inside a golden diff.
    #[test]
    fn the_fixture_covers_a_user_message_an_assistant_message_a_tool_call_with_its_output_and_a_reasoning_block()
     {
        let body = fixture_body();
        let mut user = false;
        let mut assistant = false;
        let mut call = false;
        let mut output = false;
        let mut reasoning = false;
        let mut longest_output = 0;
        for line in body.lines() {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("fixture lines parse as JSON");
            if value.get("type").and_then(serde_json::Value::as_str) != Some("response_item") {
                continue;
            }
            let payload = value.get("payload");
            match payload
                .and_then(|payload| payload.get("type"))
                .and_then(serde_json::Value::as_str)
            {
                Some("message") => match payload
                    .and_then(|payload| payload.get("role"))
                    .and_then(serde_json::Value::as_str)
                {
                    Some("user") => user = true,
                    Some("assistant") => assistant = true,
                    _ => {}
                },
                Some("custom_tool_call" | "function_call") => call = true,
                Some("custom_tool_call_output" | "function_call_output") => {
                    output = true;
                    if let Some(parts) = payload
                        .and_then(|payload| payload.get("output"))
                        .and_then(serde_json::Value::as_array)
                    {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(serde_json::Value::as_str)
                            {
                                longest_output = longest_output.max(text.len());
                            }
                        }
                    }
                }
                Some("reasoning") => reasoning = true,
                _ => {}
            }
        }
        assert!(user, "the fixture holds a user message");
        assert!(assistant, "the fixture holds an assistant message");
        assert!(call, "the fixture holds a tool call");
        assert!(output, "the fixture holds the call output");
        assert!(reasoning, "the fixture holds a reasoning block");
        assert!(
            longest_output >= 2000,
            "the fixture tool output is {longest_output} characters, not 2000"
        );
    }

    /// Developer instructions are session setup, not conversation.
    #[test]
    fn developer_messages_contribute_nothing() {
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"id\":\"m1\",\"role\":\"developer\",\"content\":[{\"type\":\"input_text\",\"text\":\"Be helpful\"}]}}\n";
        assert!(render(body).is_empty());
    }

    /// A custom tool call pairs with its output the same way a function call
    /// does: the two spellings are one mechanism, not two.
    #[test]
    fn a_custom_tool_call_pairs_with_its_output() {
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"id\":\"c1\",\"status\":\"completed\",\"call_id\":\"call_9\",\"name\":\"exec\",\"input\":\"ls\"}}\n\
            {\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call_output\",\"id\":\"o1\",\"call_id\":\"call_9\",\"output\":[{\"type\":\"input_text\",\"text\":\"done\"}]}}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].name, "exec");
        assert_eq!(messages[0].tool_calls[0].result, "done");
    }

    /// A JSON-encoded string input is stored parsed, so the markdown shows
    /// structure; a plain string stays a string.
    #[test]
    fn a_string_input_parses_as_json_when_it_is_json() {
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"id\":\"f1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\": \\\"/work/x\\\"}\",\"call_id\":\"call_1\"}}\n";
        let messages = render(body);
        assert_eq!(
            messages[0].tool_calls[0].input,
            serde_json::json!({"path": "/work/x"})
        );
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"id\":\"c1\",\"call_id\":\"call_2\",\"name\":\"exec\",\"input\":\"plain text\"}}\n";
        let messages = render(body);
        assert_eq!(
            messages[0].tool_calls[0].input,
            serde_json::Value::String("plain text".to_string())
        );
    }

    /// An output whose call never appeared is kept under an unknown call,
    /// never dropped.
    #[test]
    fn an_orphan_output_is_kept_never_dropped() {
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"id\":\"o1\",\"call_id\":\"call_missing\",\"output\":[{\"type\":\"input_text\",\"text\":\"late output\"}]}}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].result, "late output");
    }

    /// The understood skip lines (usage, lifecycle, file-level records)
    /// contribute no message and no skipped count: they are classified, not
    /// merely tolerated.
    #[test]
    fn understood_skip_lines_are_classified_not_counted() {
        let body = "{\"timestamp\":\"2026-09-14T23:45:27.898Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s\"}}\n\
            {\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":null}}\n\
            {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"dup\"}}\n\
            {\"type\":\"turn_context\",\"payload\":{}}\n\
            {\"type\":\"world_state\",\"payload\":{}}\n\
            {\"type\":\"compacted\",\"payload\":{}}\n";
        let (messages, skipped) = CodexTranscriptRenderer.render_file_with_skipped(body);
        assert!(messages.is_empty());
        assert!(
            skipped.is_empty(),
            "understood lines are not skipped: {skipped:?}"
        );
    }

    /// A line the renderer has no case for is counted by type: an unknown
    /// envelope, an unknown response-item payload, a missing type, and a line
    /// that is not JSON at all. The planted negative pins the counting over
    /// mere presence: two lines of one type count two.
    #[test]
    fn unknown_line_types_are_counted_once_per_type() {
        let body = "{\"type\":\"future_envelope\",\"payload\":{}}\n\
            {\"type\":\"future_envelope\",\"payload\":{}}\n\
            {\"type\":\"response_item\",\"payload\":{\"type\":\"mystery_widget\"}}\n\
            {\"no_type_here\":true}\n\
            not json at all\n";
        let (messages, skipped) = CodexTranscriptRenderer.render_file_with_skipped(body);
        assert!(messages.is_empty());
        assert_eq!(skipped.get("future_envelope"), Some(&2));
        assert_eq!(skipped.get("mystery_widget"), Some(&1));
        assert_eq!(skipped.get("untyped"), Some(&1));
        assert_eq!(skipped.get("unparseable"), Some(&1));
    }

    /// System reminders are dropped from codex text the same way the
    /// claude-code renderer drops them.
    #[test]
    fn system_reminders_are_dropped_but_prose_is_kept() {
        let body = "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"id\":\"m1\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Keep this. <system-reminder>drop this</system-reminder>\"}]}}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Keep this.");
    }
}
