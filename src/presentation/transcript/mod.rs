//! Session-transcript rendering behind `aub export transcript` (`aub-xpfl`
//! ships the command and the claude-code renderer; `aub-51wv` adds the codex
//! and pi renderers; the opencode renderer is the remaining sibling bead).
//!
//! One trait ([`TranscriptRenderer`]) turns a transcript file's raw text into
//! [`TranscriptMessage`] values, one file per harness, each registered by
//! `session.source` in [`renderer_for`], and
//! [`render_transcript_markdown`] writes the finished [`TranscriptDocument`]
//! as markdown. The document carries only already-typed values: the caller
//! resolved the session and read the files, so nothing here touches the
//! store, the clock, or the filesystem.
//!
//! May not depend on:
//! - provider adapters
//! - store or calibration (boundary rule 09)
//! - the system clock or the filesystem

pub mod claude_code;
pub mod codex;
pub mod pi;

use std::collections::BTreeMap;

use crate::domain::time::UtcTimestamp;

pub use claude_code::ClaudeCodeTranscriptRenderer;
pub use codex::CodexTranscriptRenderer;
pub use pi::PiTranscriptRenderer;

/// Who said a rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    User,
    Assistant,
}

impl TranscriptRole {
    fn heading(self) -> &'static str {
        match self {
            Self::User => "## User",
            Self::Assistant => "## Assistant",
        }
    }
}

/// One tool invocation inside a message: the full input and the full result
/// text, never truncated. A call whose result never arrived in the transcript
/// carries an empty result, and the writer then omits the result block rather
/// than printing an empty one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptToolCall {
    /// The tool name the transcript stated, or `(unknown)` when a result
    /// arrived without its call.
    pub name: String,
    /// The full input JSON the transcript stated.
    pub input: serde_json::Value,
    /// The full result text, possibly empty.
    pub result: String,
}

/// One conversation message: visible text plus, on request, tool calls and
/// thinking blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub text: String,
    pub tool_calls: Vec<TranscriptToolCall>,
    pub thinking: Vec<String>,
}

impl TranscriptMessage {
    /// Whether this message has anything to show under `options`: a message
    /// holding only tool traffic is invisible without `--include-tools`, and
    /// a thinking-only message is invisible without `--include-thinking`.
    pub fn is_visible(&self, options: &TranscriptRenderOptions) -> bool {
        if !self.text.is_empty() {
            return true;
        }
        if options.include_tools && !self.tool_calls.is_empty() {
            return true;
        }
        options.include_thinking && !self.thinking.is_empty()
    }
}

/// What the writer emits beyond plain conversation text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptRenderOptions {
    pub include_tools: bool,
    pub include_thinking: bool,
}

/// One transcript file's rendered messages: the parent conversation, or one
/// subagent transcript rendered under its own heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    /// The file name for the `## Subagent <file>` heading; empty for parent
    /// files, which render with no heading of their own.
    pub file_name: String,
    pub is_subagent: bool,
    pub messages: Vec<TranscriptMessage>,
}

/// Everything the markdown writer needs: the heading fields from the ledger's
/// session row and the rendered files in output order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDocument {
    /// The harness namespace, e.g. `claude-code`.
    pub harness: String,
    /// The logical project key, e.g. `unknown-project` when unmapped.
    pub project: String,
    /// The full native session id.
    pub session_id: String,
    pub started: UtcTimestamp,
    pub files: Vec<TranscriptFile>,
}

/// One harness's transcript reader: raw file text in, messages out, in file
/// order. A line the renderer does not understand is counted by type (see
/// [`render_file_with_skipped`]), never rendered as prose: a wrong number
/// here would read as a plausible session.
pub trait TranscriptRenderer {
    /// The `session.source` value this renderer reads, e.g. `claude-code`.
    fn harness(&self) -> &'static str;

    /// Reads one transcript file's whole text into messages. A line the
    /// renderer does not understand is skipped, never rendered as prose: a
    /// wrong number here would read as a plausible session.
    fn render_file(&self, body: &str) -> Vec<TranscriptMessage>;

    /// Reads one transcript file's whole text into messages plus the count
    /// of skipped lines by type. The default keeps the plain reading and
    /// reports nothing skipped, so a renderer with no unclassified lines
    /// (today: claude-code) implements only [`render_file`].
    fn render_file_with_skipped(
        &self,
        body: &str,
    ) -> (Vec<TranscriptMessage>, BTreeMap<String, usize>) {
        (self.render_file(body), BTreeMap::new())
    }
}

/// The renderer for one harness name, when this binary ships one. A harness
/// with no renderer yet fails in the caller with `no transcript renderer for
/// harness '<name>'`, never with an empty document.
pub fn renderer_for(harness: &str) -> Option<&'static dyn TranscriptRenderer> {
    match harness {
        "claude-code" => Some(claude_code_renderer()),
        "codex" => Some(codex_renderer()),
        "pi" => Some(pi_renderer()),
        _ => None,
    }
}

fn claude_code_renderer() -> &'static dyn TranscriptRenderer {
    static RENDERER: ClaudeCodeTranscriptRenderer = ClaudeCodeTranscriptRenderer;
    &RENDERER
}

fn codex_renderer() -> &'static dyn TranscriptRenderer {
    static RENDERER: CodexTranscriptRenderer = CodexTranscriptRenderer;
    &RENDERER
}

fn pi_renderer() -> &'static dyn TranscriptRenderer {
    static RENDERER: PiTranscriptRenderer = PiTranscriptRenderer;
    &RENDERER
}

/// One aggregated skipped-lines report for the end of an export:
/// `skipped: N lines of type X`. The wording is fixed so the operator can
/// grep it; the caller prints one line per type, never one per line.
pub fn format_transcript_skipped_line(skipped_type: &str, count: usize) -> String {
    format!("skipped: {count} lines of type {skipped_type}")
}

/// Whether a transcript path is a claude-code subagent transcript:
/// `<session>/subagents/agent-*.jsonl`. Only the path shape decides, so a
/// parent file that happens to mention subagents still renders as a parent.
pub fn is_subagent_transcript_path(path: &str) -> bool {
    let components: Vec<&str> = path.split('/').collect();
    let Some(file_name) = components.last() else {
        return false;
    };
    if !file_name.starts_with("agent-") || !file_name.ends_with(".jsonl") {
        return false;
    }
    components.contains(&"subagents")
}

/// The file name for the `## Subagent <file>` heading: the last path
/// component, or the whole path when it has none.
pub fn transcript_file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Orders transcript paths for output: parent files first, then subagent
/// files, each group in path order. Discovery order is filesystem order, so
/// without this a subagent file could render before the conversation it
/// belongs to.
pub fn order_transcript_files(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    let (subagents, parents): (Vec<String>, Vec<String>) = paths
        .into_iter()
        .partition(|path| is_subagent_transcript_path(path));
    parents.into_iter().chain(subagents).collect()
}

/// Drops one injected span kind from display text: `<name>…</name>` pairs are
/// removed wherever they appear, and an unclosed `<name>` drops the rest of
/// the block, so a truncated transcript cannot leak the injected text through
/// a missing close tag. The surrounding prose is kept: only the injected
/// material goes, the way `cass export` drops system reminders and skill
/// text rather than the message around them.
fn strip_tagged_spans(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open.as_str()) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        match after_open.find(close.as_str()) {
            Some(end) => rest = &after_open[end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Drops system reminders and injected skill text from one text block, the
/// way `cass export` strips them (its `--include-skills` flag exists because
/// the default strips). Returns `None` when nothing displayable remains, so
/// the caller drops the block rather than rendering an empty one.
pub fn clean_transcript_text(text: &str) -> Option<String> {
    let stripped = strip_tagged_spans(&strip_tagged_spans(text, "system-reminder"), "skill");
    let trimmed = stripped.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Renders the document as markdown: `# <harness> · <project> · <uuid>`,
/// then `*Started: <UTC>*`, then per message `## User` or `## Assistant`
/// with the text; `--include-tools` adds `**Tool: <name>**` with the full
/// JSON input in a fenced block and `**Tool result:**` with the full result
/// text, never truncated; `--include-thinking` adds `> ` blockquotes.
/// Nothing is summarised.
pub fn render_transcript_markdown(
    document: &TranscriptDocument,
    options: &TranscriptRenderOptions,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "# {} · {} · {}",
        document.harness, document.project, document.session_id
    ));
    lines.push(String::new());
    lines.push(format!("*Started: {}*", document.started.to_rfc3339()));
    for file in &document.files {
        if file.is_subagent {
            lines.push(String::new());
            lines.push(format!("## Subagent {}", file.file_name));
        }
        for message in &file.messages {
            if !message.is_visible(options) {
                continue;
            }
            lines.push(String::new());
            lines.push(message.role.heading().to_string());
            if !message.text.is_empty() {
                lines.push(message.text.clone());
            }
            if options.include_tools {
                for call in &message.tool_calls {
                    lines.push(format!("**Tool: {}**", call.name));
                    lines.push("```json".to_string());
                    lines.push(pretty_tool_input(&call.input));
                    lines.push("```".to_string());
                    if !call.result.is_empty() {
                        lines.push("**Tool result:**".to_string());
                        lines.push(call.result.clone());
                    }
                }
            }
            if options.include_thinking {
                for block in &message.thinking {
                    for line in block.lines() {
                        lines.push(format!("> {line}"));
                    }
                }
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// The full tool input as indented JSON. Pretty rather than compact: the
/// input is evidence the operator reads, and the 3000-character case in the
/// bead stays byte-identical either way.
fn pretty_tool_input(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(include_tools: bool, include_thinking: bool) -> TranscriptRenderOptions {
        TranscriptRenderOptions {
            include_tools,
            include_thinking,
        }
    }

    fn document() -> TranscriptDocument {
        TranscriptDocument {
            harness: "claude-code".to_string(),
            project: "llm-workflow".to_string(),
            session_id: "190c2fb2-1111-4222-8333-444444444444".to_string(),
            started: UtcTimestamp::from_unix_nanos(1_786_000_000_000_000_000),
            files: vec![TranscriptFile {
                file_name: String::new(),
                is_subagent: false,
                messages: vec![
                    TranscriptMessage {
                        role: TranscriptRole::User,
                        text: "Do the thing".to_string(),
                        tool_calls: Vec::new(),
                        thinking: Vec::new(),
                    },
                    TranscriptMessage {
                        role: TranscriptRole::Assistant,
                        text: "Done".to_string(),
                        tool_calls: Vec::new(),
                        thinking: vec!["considered it".to_string()],
                    },
                ],
            }],
        }
    }

    #[test]
    fn plain_output_has_two_sections_and_no_tool_or_thinking() {
        let rendered = render_transcript_markdown(&document(), &options(false, false));
        assert!(
            rendered.starts_with(
                "# claude-code · llm-workflow · 190c2fb2-1111-4222-8333-444444444444\n"
            )
        );
        assert!(rendered.contains("*Started: "));
        assert_eq!(rendered.matches("## User").count(), 1);
        assert_eq!(rendered.matches("## Assistant").count(), 1);
        assert!(
            !rendered.contains("**Tool:"),
            "no tool traffic without the flag"
        );
        assert!(
            !rendered.contains("considered it"),
            "no thinking without the flag"
        );
    }

    #[test]
    fn thinking_appears_as_blockquotes_only_with_the_flag() {
        let rendered = render_transcript_markdown(&document(), &options(false, true));
        assert!(rendered.contains("> considered it"));
    }

    #[test]
    fn tool_calls_render_name_input_and_result_only_with_the_flag() {
        let mut doc = document();
        doc.files[0].messages[1]
            .tool_calls
            .push(TranscriptToolCall {
                name: "Read".to_string(),
                input: serde_json::json!({"file_path": "/work/README.md"}),
                result: "README contents".to_string(),
            });
        let plain = render_transcript_markdown(&doc, &options(false, false));
        assert!(!plain.contains("**Tool: Read**"));
        let with_tools = render_transcript_markdown(&doc, &options(true, false));
        assert!(with_tools.contains("**Tool: Read**"));
        assert!(with_tools.contains("\"file_path\": \"/work/README.md\""));
        assert!(with_tools.contains("**Tool result:**"));
        assert!(with_tools.contains("README contents"));
    }

    #[test]
    fn a_tool_call_without_a_result_omits_the_result_block() {
        let mut doc = document();
        doc.files[0].messages[1]
            .tool_calls
            .push(TranscriptToolCall {
                name: "Bash".to_string(),
                input: serde_json::json!({"command": "ls"}),
                result: String::new(),
            });
        let rendered = render_transcript_markdown(&doc, &options(true, false));
        assert!(rendered.contains("**Tool: Bash**"));
        assert!(
            !rendered.contains("**Tool result:**"),
            "an empty result prints no result block"
        );
    }

    #[test]
    fn subagent_files_render_after_the_parent_under_a_heading() {
        let mut doc = document();
        doc.files.push(TranscriptFile {
            file_name: "agent-x.jsonl".to_string(),
            is_subagent: true,
            messages: vec![TranscriptMessage {
                role: TranscriptRole::Assistant,
                text: "subagent reply".to_string(),
                tool_calls: Vec::new(),
                thinking: Vec::new(),
            }],
        });
        let rendered = render_transcript_markdown(&doc, &options(false, false));
        let parent = rendered.find("## Assistant\nDone").expect("parent renders");
        let heading = rendered
            .find("## Subagent agent-x.jsonl")
            .expect("subagent heading renders");
        let reply = rendered.find("subagent reply").expect("subagent renders");
        assert!(parent < heading && heading < reply);
    }

    #[test]
    fn invisible_messages_render_nothing() {
        let mut doc = document();
        // A tools-only message is invisible without the flag.
        doc.files[0].messages.push(TranscriptMessage {
            role: TranscriptRole::Assistant,
            text: String::new(),
            tool_calls: vec![TranscriptToolCall {
                name: "Read".to_string(),
                input: serde_json::json!({}),
                result: String::new(),
            }],
            thinking: Vec::new(),
        });
        let plain = render_transcript_markdown(&doc, &options(false, false));
        assert_eq!(plain.matches("## Assistant").count(), 1);
        let with_tools = render_transcript_markdown(&doc, &options(true, false));
        assert_eq!(with_tools.matches("## Assistant").count(), 2);
    }

    #[test]
    fn subagent_detection_needs_the_directory_and_the_prefix() {
        assert!(is_subagent_transcript_path(
            "/root/sess/subagents/agent-x.jsonl"
        ));
        assert!(!is_subagent_transcript_path("/root/sess.jsonl"));
        assert!(!is_subagent_transcript_path(
            "/root/sess/subagents/notes.jsonl"
        ));
        assert!(!is_subagent_transcript_path("/root/subagents.jsonl"));
        assert_eq!(
            transcript_file_name("/root/sess/subagents/agent-x.jsonl"),
            "agent-x.jsonl"
        );
    }

    #[test]
    fn parents_render_before_subagents_whatever_order_they_arrive_in() {
        let ordered = order_transcript_files(vec![
            "/root/sess/subagents/agent-b.jsonl".to_string(),
            "/root/sess.jsonl".to_string(),
            "/root/sess/subagents/agent-a.jsonl".to_string(),
        ]);
        assert_eq!(
            ordered,
            vec![
                "/root/sess.jsonl",
                "/root/sess/subagents/agent-a.jsonl",
                "/root/sess/subagents/agent-b.jsonl",
            ]
        );
    }

    #[test]
    fn system_reminders_and_skill_text_are_dropped_but_prose_is_kept() {
        let kept = clean_transcript_text(
            "Keep this. <system-reminder>drop this</system-reminder> And this.",
        )
        .unwrap();
        assert_eq!(kept, "Keep this.  And this.");
        let skill =
            clean_transcript_text("<skill><name>foo</name>Use foo</skill>").unwrap_or_default();
        assert_eq!(skill, "");
        assert_eq!(
            clean_transcript_text("   "),
            None,
            "whitespace-only blocks vanish"
        );
        assert_eq!(
            clean_transcript_text("plain text"),
            Some("plain text".to_string())
        );
    }

    #[test]
    fn an_unclosed_tag_drops_the_rest_of_the_block() {
        // The planted negative: without the unclosed-tag arm, the reminder
        // text after the tag would survive the strip.
        let dropped = clean_transcript_text("Visible. <system-reminder>never closed");
        assert_eq!(dropped, Some("Visible.".to_string()));
    }

    #[test]
    fn an_unknown_harness_has_no_renderer() {
        assert_eq!(
            renderer_for("claude-code").unwrap().harness(),
            "claude-code"
        );
        assert_eq!(renderer_for("codex").unwrap().harness(), "codex");
        assert_eq!(renderer_for("pi").unwrap().harness(), "pi");
        assert!(renderer_for("opencode").is_none());
        assert!(renderer_for("future-harness").is_none());
    }

    #[test]
    fn the_skipped_line_report_spells_the_count_and_the_type() {
        // The planted negative: a free-form message would still contain both
        // halves, so this pins the exact wording the operator greps for.
        assert_eq!(
            format_transcript_skipped_line("mystery_widget", 2),
            "skipped: 2 lines of type mystery_widget"
        );
    }

    #[test]
    fn the_default_skipped_reading_reports_nothing() {
        // The claude-code renderer implements only `render_file`, so the
        // default arm must keep its reading and report no skipped lines:
        // adding counting there would change its export output.
        let body = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"},\"timestamp\":\"2026-09-06T10:00:00.000Z\"}\n";
        let renderer = renderer_for("claude-code").unwrap();
        let (messages, skipped) = renderer.render_file_with_skipped(body);
        assert_eq!(messages.len(), 1);
        assert!(skipped.is_empty());
    }
}
