//! The pi transcript renderer behind `aub export transcript` (`aub-51wv`).
//!
//! Field names and shapes below were read off a real session file on this
//! machine before the renderer was written (the meter-adapter rule: a
//! renderer written from memory of a format is the fixture-agrees-with-itself
//! failure). Every line carries a top-level `type`: a `session` header
//! (`version`, `id`, `timestamp`, `cwd`), `model_change` and
//! `thinking_level_change` records, then `message` entries (`id`,
//! `parentId`, `timestamp`, and `message` with `role`, `content` and
//! `timestamp`). Roles are `user`, `assistant` and `toolResult`;
//! assistant content blocks are `text` (`text`), `thinking` (`thinking`,
//! `thinkingSignature`) and `toolCall` (`id`, `name`, `arguments`); a
//! `toolResult` message carries `toolCallId`, `toolName`, `content` text and
//! `isError`.
//!
//! Tool results arrive in later `toolResult` messages carrying the `toolCall`
//! id, so the renderer pairs them the way the claude-code renderer pairs
//! `tool_result` blocks. A result whose call never appeared is kept under an
//! unknown call, never dropped.
//!
//! A line whose top-level type the renderer has no case for is counted by
//! that type and reported once per type by the caller, never dropped
//! silently and never fatal.
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

/// Renders pi transcript files (`session.source == "pi"`).
pub struct PiTranscriptRenderer;

impl TranscriptRenderer for PiTranscriptRenderer {
    fn harness(&self) -> &'static str {
        "pi"
    }

    fn render_file(&self, body: &str) -> Vec<TranscriptMessage> {
        render_pi_file_with_skipped(body).0
    }

    fn render_file_with_skipped(
        &self,
        body: &str,
    ) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
        render_pi_file_with_skipped(body)
    }
}

/// One tool call still waiting for its result, paired by the `toolCall` id.
struct PendingPiToolUse {
    message_index: usize,
    call_index: usize,
}

fn render_pi_file_with_skipped(body: &str) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut pending: BTreeMap<String, PendingPiToolUse> = BTreeMap::new();
    // Results whose call never appeared, kept in arrival order per message.
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
        match value.get("type").and_then(serde_json::Value::as_str) {
            // The header names the session the ledger already resolved; the
            // model and thinking-level records state agent setup, and none of
            // the three is conversation.
            Some("session" | "model_change" | "thinking_level_change") => {}
            Some("message") => read_pi_message(&value, &mut messages, &mut pending, &mut orphans),
            Some(unknown) => {
                *skipped.entry(unknown.to_string()).or_insert(0) += 1;
            }
            None => {
                *skipped.entry("untyped".to_string()).or_insert(0) += 1;
            }
        }
    }

    // Orphan results render as calls with no recorded input of their own:
    // the result text is transcript content and is never dropped silently.
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

/// One `message` line: user and assistant text render, `toolResult` lines
/// pair back to their call, and any other role contributes nothing.
fn read_pi_message(
    value: &serde_json::Value,
    messages: &mut Vec<TranscriptMessage>,
    pending: &mut BTreeMap<String, PendingPiToolUse>,
    orphans: &mut BTreeMap<usize, Vec<String>>,
) {
    let Some(message) = value.get("message").and_then(serde_json::Value::as_object) else {
        return;
    };
    match message.get("role").and_then(serde_json::Value::as_str) {
        Some("user" | "assistant") => read_pi_turn(message, messages, pending),
        Some("toolResult") => read_pi_tool_result_message(message, messages, pending, orphans),
        _ => {}
    }
}

/// A user or assistant turn: text, thinking and tool calls out of the
/// content blocks. A turn left with nothing to show under any flag is
/// dropped rather than rendered as an empty section.
fn read_pi_turn(
    message: &serde_json::Map<String, serde_json::Value>,
    messages: &mut Vec<TranscriptMessage>,
    pending: &mut BTreeMap<String, PendingPiToolUse>,
) {
    let role = match message.get("role").and_then(serde_json::Value::as_str) {
        Some("user") => TranscriptRole::User,
        _ => TranscriptRole::Assistant,
    };
    let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) else {
        return;
    };
    let index = messages.len();
    messages.push(TranscriptMessage {
        role,
        text: String::new(),
        tool_calls: Vec::new(),
        thinking: Vec::new(),
    });
    let mut texts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
                    && let Some(kept) = clean_transcript_text(text)
                {
                    texts.push(kept);
                }
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(serde_json::Value::as_str)
                    && let Some(kept) = clean_transcript_text(text)
                {
                    messages[index].thinking.push(kept);
                }
            }
            Some("toolCall") => {
                let name = block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string();
                let input = block
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                messages[index].tool_calls.push(TranscriptToolCall {
                    name,
                    input,
                    result: String::new(),
                });
                if !id.is_empty() {
                    pending.insert(
                        id,
                        PendingPiToolUse {
                            message_index: index,
                            call_index: messages[index].tool_calls.len() - 1,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    if !texts.is_empty() {
        messages[index].text = texts.join("\n\n");
    }
    if messages[index].text.is_empty()
        && messages[index].tool_calls.is_empty()
        && messages[index].thinking.is_empty()
    {
        messages.pop();
    }
}

/// A `toolResult` message: its text pairs back to the `toolCall` id, or is
/// kept as an orphan when the call never appeared.
fn read_pi_tool_result_message(
    message: &serde_json::Map<String, serde_json::Value>,
    messages: &mut Vec<TranscriptMessage>,
    pending: &mut BTreeMap<String, PendingPiToolUse>,
    orphans: &mut BTreeMap<usize, Vec<String>>,
) {
    let result = pi_text_content(message.get("content"));
    let id = message
        .get("toolCallId")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !id.is_empty()
        && let Some(found) = pending.get(id)
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

/// The full text of a `toolResult` content array: every text item joined,
/// never truncated.
fn pi_text_content(content: Option<&serde_json::Value>) -> String {
    let Some(parts) = content.and_then(serde_json::Value::as_array) else {
        return match content.and_then(serde_json::Value::as_str) {
            Some(text) => text.to_string(),
            None => String::new(),
        };
    };
    let mut texts = Vec::new();
    for part in parts {
        if let Some(text) = part.as_str() {
            texts.push(text.to_string());
        } else if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
            texts.push(text.to_string());
        }
    }
    texts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::{TranscriptRenderOptions, render_transcript_markdown};
    use super::*;

    fn render(body: &str) -> Vec<TranscriptMessage> {
        PiTranscriptRenderer.render_file(body)
    }

    fn rendered(body: &str, include_tools: bool, include_thinking: bool) -> String {
        let (messages, _) = PiTranscriptRenderer.render_file_with_skipped(body);
        let document = super::super::TranscriptDocument {
            harness: "pi".to_string(),
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
                .join("tests/fixtures/transcripts/pi/session.jsonl"),
        )
        .expect("the pi fixture must exist")
    }

    fn golden(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/transcripts/pi")
                .join(name),
        )
        .unwrap_or_else(|_| panic!("the pi golden {name} must exist"))
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

    /// The bead's golden fixture renders user and assistant text without
    /// flags: the thinking block and the tool traffic need theirs.
    #[test]
    fn plain_rendering_has_two_sections_and_no_tool_or_thinking() {
        let out = rendered(&fixture_body(), false, false);
        assert!(out.contains("## User\nDo the thing"));
        assert!(out.contains("## Assistant\nFinished"));
        assert!(!out.contains("**Tool:"), "tool traffic needs the flag");
        assert!(
            !out.contains("read the project file first"),
            "thinking needs the flag"
        );
        assert!(!out.contains('o'.to_string().repeat(100).as_str()));
    }

    /// The 2000-character tool result survives whole under
    /// `--include-tools`, paired back to its `toolCall` by id.
    #[test]
    fn tools_rendering_keeps_the_2000_character_output_intact() {
        let big = "o".repeat(2000);
        let out = rendered(&fixture_body(), true, false);
        assert!(out.contains("**Tool: read**"));
        assert!(
            out.contains(&big),
            "the full result survives, never truncated"
        );
        assert!(out.contains("\"/work/project/README.md\""));
        assert!(
            !out.contains("**Tool: (unknown)**"),
            "the result pairs back to its call by toolCallId"
        );
        assert!(!out.contains("read the project file first"));
    }

    /// The thinking block renders as a blockquote, only under its flag.
    #[test]
    fn thinking_rendering_adds_the_blockquote_only_with_the_flag() {
        let out = rendered(&fixture_body(), false, true);
        assert!(out.contains("> The user wants the thing done; read the project file first."));
        assert!(!out.contains("**Tool:"));
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
        let mut thinking = false;
        let mut longest_output = 0;
        for line in body.lines() {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("fixture lines parse as JSON");
            if value.get("type").and_then(serde_json::Value::as_str) != Some("message") {
                continue;
            }
            let message = value.get("message");
            match message
                .and_then(|message| message.get("role"))
                .and_then(serde_json::Value::as_str)
            {
                Some("user") => user = true,
                Some("assistant") => {
                    assistant = true;
                    if let Some(blocks) = message
                        .and_then(|message| message.get("content"))
                        .and_then(serde_json::Value::as_array)
                    {
                        for block in blocks {
                            match block.get("type").and_then(serde_json::Value::as_str) {
                                Some("toolCall") => call = true,
                                Some("thinking") => thinking = true,
                                _ => {}
                            }
                        }
                    }
                }
                Some("toolResult") => {
                    output = true;
                    if let Some(parts) = message
                        .and_then(|message| message.get("content"))
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
                _ => {}
            }
        }
        assert!(user, "the fixture holds a user message");
        assert!(assistant, "the fixture holds an assistant message");
        assert!(call, "the fixture holds a tool call");
        assert!(output, "the fixture holds the call output");
        assert!(thinking, "the fixture holds a reasoning block");
        assert!(
            longest_output >= 2000,
            "the fixture tool output is {longest_output} characters, not 2000"
        );
    }

    /// The header and setup records contribute no message and no skipped
    /// count: they are classified, not merely tolerated.
    #[test]
    fn header_and_setup_records_are_classified_not_counted() {
        let body = "{\"type\":\"session\",\"version\":3,\"id\":\"s\",\"timestamp\":\"2026-09-19T17:59:01.154Z\",\"cwd\":\"/work/project\"}\n\
            {\"type\":\"model_change\",\"id\":\"m\",\"modelId\":\"x\"}\n\
            {\"type\":\"thinking_level_change\",\"id\":\"t\",\"thinkingLevel\":\"off\"}\n";
        let (messages, skipped) = PiTranscriptRenderer.render_file_with_skipped(body);
        assert!(messages.is_empty());
        assert!(
            skipped.is_empty(),
            "understood lines are not skipped: {skipped:?}"
        );
    }

    /// Each content kind in isolation: text, thinking and a tool call each
    /// land in their own slot of one assistant turn.
    #[test]
    fn each_assistant_content_kind_lands_in_its_own_slot() {
        let body = "{\"type\":\"message\",\"id\":\"a\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Working\"},{\"type\":\"thinking\",\"thinking\":\"Plan first\"},{\"type\":\"toolCall\",\"id\":\"call_1\",\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}]}}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Working");
        assert_eq!(messages[0].thinking, vec!["Plan first".to_string()]);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].name, "bash");
        // The planted negative: the result-only message pairs back and pops,
        // so one turn stays one turn rather than gaining a section.
        let paired = "{\"type\":\"message\",\"id\":\"b\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call_1\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"out\"}]}}\n";
        let messages = render(&format!("{body}{paired}"));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls[0].result, "out");
    }

    /// A result whose call never appeared is kept under an unknown call,
    /// never dropped.
    #[test]
    fn an_orphan_result_is_kept_never_dropped() {
        let body = "{\"type\":\"message\",\"id\":\"b\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call_missing\",\"toolName\":\"bash\",\"content\":[{\"type\":\"text\",\"text\":\"late output\"}]}}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].result, "late output");
    }

    /// A line the renderer has no case for is counted by type, including a
    /// line that is not JSON at all. The planted negative pins the counting
    /// over mere presence: two lines of one type count two.
    #[test]
    fn unknown_line_types_are_counted_once_per_type() {
        let body = "{\"type\":\"future_kind\",\"id\":\"1\"}\n\
            {\"type\":\"future_kind\",\"id\":\"2\"}\n\
            not json at all\n";
        let (messages, skipped) = PiTranscriptRenderer.render_file_with_skipped(body);
        assert!(messages.is_empty());
        assert_eq!(skipped.get("future_kind"), Some(&2));
        assert_eq!(skipped.get("unparseable"), Some(&1));
    }

    /// Unknown roles and unknown content blocks contribute nothing but take
    /// nothing down with them.
    #[test]
    fn unknown_roles_and_blocks_are_skipped_but_known_ones_still_render() {
        let body = "{\"type\":\"message\",\"id\":\"a\",\"message\":{\"role\":\"system\",\"content\":[{\"type\":\"text\",\"text\":\"bg\"}]}}\n\
            {\"type\":\"message\",\"id\":\"b\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"something_new\",\"data\":1},{\"type\":\"text\",\"text\":\"Still here\"}]}}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Still here");
    }
}
