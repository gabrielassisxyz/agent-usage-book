//! The claude-code transcript renderer behind `aub export transcript`
//! (`aub-xpfl`).
//!
//! Reads each JSONL line's `type`, `message.role`, `message.content` (a
//! string, or an array of `text`, `tool_use`, `tool_result` and `thinking`
//! blocks) and `timestamp`; file order is conversation order. Tool results
//! arrive in later user messages carrying the `tool_use_id` of their call
//! (`toolUseID` is also accepted), so the renderer pairs them by id. System
//! reminders and injected skill text are dropped per block, `isMeta`
//! snapshot records and `summary` lines are not conversation and are skipped. A line that is not
//! JSON, or that carries no renderable message, contributes nothing: a wrong
//! guess here would read as a plausible session.
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

/// Renders claude-code transcript files (`session.source ==
/// "claude-code"`).
pub struct ClaudeCodeTranscriptRenderer;

impl TranscriptRenderer for ClaudeCodeTranscriptRenderer {
    fn harness(&self) -> &'static str {
        "claude-code"
    }

    fn render_file(&self, body: &str) -> Vec<TranscriptMessage> {
        render_claude_code_file(body)
    }
}

/// One parsed content block that still needs its result paired.
#[derive(Debug)]
struct PendingToolUse {
    message_index: usize,
    call_index: usize,
}

fn render_claude_code_file(body: &str) -> Vec<TranscriptMessage> {
    let mut messages: Vec<TranscriptMessage> = Vec::new();
    let mut pending: BTreeMap<String, PendingToolUse> = BTreeMap::new();
    // Results whose call never appeared, kept in arrival order per message.
    let mut orphans: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("summary") {
            continue;
        }
        if value.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        let Some(message) = value.get("message").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let role = match message.get("role").and_then(serde_json::Value::as_str) {
            Some("user") => TranscriptRole::User,
            Some("assistant") => TranscriptRole::Assistant,
            _ => continue,
        };
        let content = match message.get("content") {
            None | Some(serde_json::Value::Null) => continue,
            Some(content) => content,
        };
        let index = messages.len();
        messages.push(TranscriptMessage {
            role,
            text: String::new(),
            tool_calls: Vec::new(),
            thinking: Vec::new(),
        });
        read_claude_content(content, index, &mut messages, &mut pending, &mut orphans);
        if messages[index].text.is_empty()
            && messages[index].tool_calls.is_empty()
            && messages[index].thinking.is_empty()
            && !orphans.contains_key(&index)
        {
            messages.pop();
        }
    }

    // Orphan results render as calls with no recorded input of their own:
    // the result text is transcript content and is never dropped silently.
    for (index, results) in orphans {
        for (_, result) in results {
            messages[index].tool_calls.push(TranscriptToolCall {
                name: "(unknown)".to_string(),
                input: serde_json::Value::Null,
                result,
            });
        }
    }
    messages
}

/// Reads one message's `content` (a string or an array of blocks) into the
/// message under construction, recording tool uses for result pairing.
fn read_claude_content(
    content: &serde_json::Value,
    index: usize,
    messages: &mut [TranscriptMessage],
    pending: &mut BTreeMap<String, PendingToolUse>,
    orphans: &mut BTreeMap<usize, Vec<(String, String)>>,
) {
    if let Some(text) = content.as_str() {
        push_text(&mut messages[index], text);
        return;
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    let mut texts: Vec<String> = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(serde_json::Value::as_str);
        match kind {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(serde_json::Value::as_str)
                    && let Some(kept) = clean_transcript_text(text)
                {
                    texts.push(kept);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("(unknown)")
                    .to_string();
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                messages[index].tool_calls.push(TranscriptToolCall {
                    name,
                    input,
                    result: String::new(),
                });
                if !id.is_empty() {
                    pending.insert(
                        id,
                        PendingToolUse {
                            message_index: index,
                            call_index: messages[index].tool_calls.len() - 1,
                        },
                    );
                }
            }
            Some("tool_result") => {
                let result = tool_result_text(block);
                let id = block
                    .get("toolUseID")
                    .or_else(|| block.get("tool_use_id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !id.is_empty()
                    && let Some(found) = pending.get(&id)
                {
                    messages[found.message_index].tool_calls[found.call_index].result = result;
                } else {
                    orphans.entry(index).or_default().push((id, result));
                }
            }
            Some("thinking") => {
                let thinking = block
                    .get("thinking")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if let Some(kept) = clean_transcript_text(thinking) {
                    messages[index].thinking.push(kept);
                }
            }
            _ => {}
        }
    }
    if !texts.is_empty() {
        messages[index].text = texts.join("\n\n");
    }
}

fn push_text(message: &mut TranscriptMessage, text: &str) {
    if let Some(kept) = clean_transcript_text(text) {
        message.text = kept;
    }
}

/// The full result text of a `tool_result` block: a string as-is, or an
/// array of blocks joined on their text, never truncated.
fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => {
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
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{TranscriptRenderOptions, render_transcript_markdown};
    use super::*;

    fn render(body: &str) -> Vec<TranscriptMessage> {
        ClaudeCodeTranscriptRenderer.render_file(body)
    }

    fn visible(body: &str, include_tools: bool, include_thinking: bool) -> String {
        let document = super::super::TranscriptDocument {
            harness: "claude-code".to_string(),
            project: "proj".to_string(),
            session_id: "sess".to_string(),
            started: crate::domain::time::UtcTimestamp::from_unix_nanos(0),
            files: vec![super::super::TranscriptFile {
                file_name: String::new(),
                is_subagent: false,
                messages: render(body),
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

    /// The bead's golden fixture: user text, assistant text, one `tool_use`
    /// with a 3000-character input, its `tool_result`, and one `thinking`
    /// block.
    fn golden_body() -> String {
        let big = "x".repeat(3000);
        format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"Do the thing\"}},\"timestamp\":\"2026-09-06T10:00:00.000Z\",\"sessionId\":\"sess\"}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"On it\"}},{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{{\"data\":\"{big}\"}}}}]}},\"timestamp\":\"2026-09-06T10:01:00.000Z\",\"sessionId\":\"sess\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"README contents\"}}]}},\"timestamp\":\"2026-09-06T10:02:00.000Z\",\"sessionId\":\"sess\"}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"First do this\",\"signature\":\"sig\"}},{{\"type\":\"text\",\"text\":\"Finished\"}}]}},\"timestamp\":\"2026-09-06T10:03:00.000Z\",\"sessionId\":\"sess\"}}\n"
        )
    }

    #[test]
    fn plain_rendering_has_two_sections_and_no_tool_or_thinking() {
        let out = visible(&golden_body(), false, false);
        assert!(out.contains("## User\nDo the thing"));
        assert!(out.contains("## Assistant\nOn it"));
        assert!(out.contains("## Assistant\nFinished"));
        assert!(!out.contains("**Tool:"), "tool traffic needs the flag");
        assert!(!out.contains("First do this"), "thinking needs the flag");
        assert!(!out.contains('x'.to_string().repeat(100).as_str()));
    }

    #[test]
    fn tools_rendering_keeps_the_3000_character_input_intact() {
        let big = "x".repeat(3000);
        let out = visible(&golden_body(), true, false);
        assert!(out.contains("**Tool: Read**"));
        assert!(
            out.contains(&big),
            "the full input survives, never truncated"
        );
        assert!(out.contains("**Tool result:**"));
        assert!(out.contains("README contents"));
        // The fixture carries the real key, `tool_use_id`: an unpaired
        // result would still print its text, but under an unknown call.
        assert!(
            !out.contains("**Tool: (unknown)**"),
            "the result pairs back to its call by tool_use_id"
        );
        assert!(!out.contains("First do this"));
    }

    #[test]
    fn thinking_rendering_adds_the_blockquote() {
        let out = visible(&golden_body(), false, true);
        assert!(out.contains("> First do this"));
        assert!(!out.contains("**Tool:"));
    }

    #[test]
    fn a_string_content_and_a_summary_line() {
        let body = "{\"type\":\"summary\",\"summary\":\"compacted\",\"leafUuid\":\"u1\"}\n\
            {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"A plain string reply\"},\"timestamp\":\"2026-09-06T10:00:00.000Z\"}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1, "the summary line is not conversation");
        assert_eq!(messages[0].text, "A plain string reply");
    }

    #[test]
    fn a_tool_result_with_an_array_of_text_blocks_joins_them() {
        let body = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]},\"timestamp\":\"2026-09-06T10:00:00.000Z\"}\n\
            {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"toolUseID\":\"toolu_9\",\"content\":[{\"type\":\"text\",\"text\":\"a\"},{\"type\":\"text\",\"text\":\"b\"}]}]},\"timestamp\":\"2026-09-06T10:01:00.000Z\"}\n";
        let messages = render(body);
        assert_eq!(
            messages.len(),
            1,
            "the result-only message pairs back and pops, leaving no empty section"
        );
        assert_eq!(messages[0].tool_calls.len(), 1);
        // The planted negative: joining with a blank line would still
        // contain both halves, so this pins the single newline.
        assert_eq!(messages[0].tool_calls[0].result, "a\nb");
    }

    #[test]
    fn an_orphan_result_is_kept_never_dropped() {
        let body = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"toolUseID\":\"toolu_missing\",\"content\":\"late output\"}]},\"timestamp\":\"2026-09-06T10:00:00.000Z\"}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tool_calls.len(), 1);
        assert_eq!(messages[0].tool_calls[0].result, "late output");
    }

    #[test]
    fn meta_snapshots_malformed_lines_and_unknown_roles_contribute_nothing() {
        let body = "not json at all\n\
            {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"snap\"}]},\"isMeta\":true,\"timestamp\":\"2026-09-06T10:00:00.000Z\"}\n\
            {\"type\":\"assistant\",\"message\":{\"role\":\"system\",\"content\":\"bg\"},\"timestamp\":\"2026-09-06T10:01:00.000Z\"}\n\
            {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":null},\"timestamp\":\"2026-09-06T10:02:00.000Z\"}\n";
        assert!(render(body).is_empty());
    }

    #[test]
    fn unknown_block_kinds_are_skipped_but_known_ones_still_render() {
        // A future block kind must not take the whole message down with it.
        let body = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"something_new\",\"data\":1},{\"type\":\"text\",\"text\":\"Still here\"}]},\"timestamp\":\"2026-09-06T10:00:00.000Z\"}\n";
        let messages = render(body);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "Still here");
    }
}
