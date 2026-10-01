//! Tests for transcript format drift detection (aub-lqe.17).

use std::fs;

use agent_usage_book::config::{FakeEnv, Overrides, resolve};
use agent_usage_book::domain::time::UtcTimestamp;
use agent_usage_book::logging::RunId;
use agent_usage_book::presentation::{
    doctor_drift_json, render_doctor_drift_report, validate_doctor_drift_report_json,
};
use agent_usage_book::transcripts::{FIXTURE_CAPTURE_PROCEDURE_DOC, detect_drift};
use proptest::prelude::*;
use test_support::StateDir;
use test_support::sanitization::matched_patterns;

fn config_from_toml(toml: &str) -> agent_usage_book::config::Config {
    let (cfg, _) = resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(toml),
        "/virtual/aub.toml",
    )
    .expect("resolve test config");
    cfg
}

/// Integration: a synthetic corpus containing a field no fixture covers,
/// asserting it is reported as uncovered naming the source and the field.
#[test]
fn integration_uncovered_field_detected_and_reported() {
    let tmp = StateDir::new();
    let claude_dir = tmp.path().join("claude-code");
    fs::create_dir_all(&claude_dir).expect("create claude dir");

    let transcript_file = claude_dir.join("session.jsonl");
    let content = r#"{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"uncovered_experimental_field":42}}}"#;
    fs::write(&transcript_file, content).expect("write transcript");

    let toml = format!(
        r#"
[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
        claude_dir.display()
    );
    let cfg = config_from_toml(&toml);

    let timestamp = UtcTimestamp::from_unix_nanos(1_000_000);
    let report = detect_drift(&cfg, None, timestamp, None).expect("drift detection succeeds");

    assert!(report.has_configured_roots);
    assert!(report.overall_drift_detected);
    assert_eq!(report.sources.len(), 1);

    let src = &report.sources[0];
    assert_eq!(src.source, "claude-code");
    assert!(src.drift_detected);
    assert!(
        src.uncovered_fields
            .contains("message.usage.uncovered_experimental_field"),
        "uncovered fields: {:?}",
        src.uncovered_fields
    );
    assert!(
        !src.uncovered_shapes.is_empty(),
        "uncovered shapes should not be empty"
    );
    assert!(src.remediation.is_some());
    assert!(
        src.remediation
            .as_ref()
            .unwrap()
            .contains(FIXTURE_CAPTURE_PROCEDURE_DOC)
    );

    let text = render_doctor_drift_report(&report);
    assert!(text.contains("UNCOVERED FORMAT DRIFT DETECTED"));
    assert!(text.contains("message.usage.uncovered_experimental_field"));
    assert!(text.contains(FIXTURE_CAPTURE_PROCEDURE_DOC));

    let json = doctor_drift_json(&report, RunId::from_string("run-test-1".to_string()));
    assert!(validate_doctor_drift_report_json(&json).is_ok());
    assert!(json.contains("message.usage.uncovered_experimental_field"));
}

/// Integration: a synthetic corpus that matches the fixture corpus exactly,
/// asserting the report is empty rather than noisy.
#[test]
fn integration_matching_corpus_produces_no_drift() {
    let tmp = StateDir::new();
    let claude_dir = tmp.path().join("claude-code");
    fs::create_dir_all(&claude_dir).expect("create claude dir");

    let transcript_file = claude_dir.join("session.jsonl");
    // Standard Claude Code shape matching committed fixtures
    let content = r#"{"type":"assistant","message":{"id":"msg_0001","model":"claude-opus-4","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}},"timestamp":"2026-08-25T10:00:00Z","uuid":"uuid-0001"}"#;
    fs::write(&transcript_file, content).expect("write transcript");

    let toml = format!(
        r#"
[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
        claude_dir.display()
    );
    let cfg = config_from_toml(&toml);

    let timestamp = UtcTimestamp::from_unix_nanos(1_000_000);
    let report = detect_drift(&cfg, None, timestamp, None).expect("drift detection succeeds");

    assert!(report.has_configured_roots);
    assert!(!report.overall_drift_detected);
    assert_eq!(report.sources.len(), 1);

    let src = &report.sources[0];
    assert!(!src.drift_detected);
    assert_eq!(src.shapes_seen.len(), 1);
    assert_eq!(src.shapes_seen[0].occurrence_count, 1);
    assert!(src.uncovered_fields.is_empty());
    assert!(src.uncovered_record_kinds.is_empty());
    assert!(src.uncovered_shapes.is_empty());
    assert_eq!(src.quarantined_records, 0);
    assert!(src.remediation.is_none());

    let text = render_doctor_drift_report(&report);
    assert!(!text.contains("UNCOVERED FORMAT DRIFT DETECTED"));
    assert!(text.contains("All record shapes covered by committed fixtures"));

    let json = doctor_drift_json(&report, RunId::from_string("run-test-2".to_string()));
    assert!(validate_doctor_drift_report_json(&json).is_ok());
    assert!(json.contains("\"overall_drift_detected\":false"));
}

/// Unit: no configured roots producing an explicit report of that fact
/// and exit zero, rather than a zero-drift claim.
#[test]
fn unit_no_configured_roots_reports_fact_and_clean_exit() {
    let cfg = config_from_toml("");
    let timestamp = UtcTimestamp::from_unix_nanos(1_000_000);
    let report = detect_drift(&cfg, None, timestamp, None).expect("drift detection succeeds");

    assert!(!report.has_configured_roots);
    assert!(!report.overall_drift_detected);
    assert!(report.sources.is_empty());

    let text = render_doctor_drift_report(&report);
    assert!(text.contains("No configured transcript roots"));
    assert!(!text.contains("All record shapes covered"));

    let json = doctor_drift_json(&report, RunId::from_string("run-test-3".to_string()));
    assert!(validate_doctor_drift_report_json(&json).is_ok());
    assert!(json.contains("\"has_configured_roots\":false"));
}

/// Unit: the quarantine counts reported per parser and failure class,
/// asserted against a seeded corpus with known failures.
#[test]
fn unit_quarantine_counts_reported_per_parser_and_failure_class() {
    let tmp = StateDir::new();
    let codex_dir = tmp.path().join("codex");
    fs::create_dir_all(&codex_dir).expect("create codex dir");

    let transcript_file = codex_dir.join("session.jsonl");
    // One line with wrong field type, one truncated line
    let content = "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":\"bad_type\"}}}}\n{truncated json line\n";
    fs::write(&transcript_file, content).expect("write transcript");

    let toml = format!(
        r#"
[[transcripts]]
name = "codex"
root = "{}"
pattern = "**/*.jsonl"
format = "codex"
"#,
        codex_dir.display()
    );
    let cfg = config_from_toml(&toml);

    let timestamp = UtcTimestamp::from_unix_nanos(1_000_000);
    let report = detect_drift(&cfg, None, timestamp, None).expect("drift detection succeeds");

    assert!(report.overall_drift_detected);
    let src = &report.sources[0];
    assert_eq!(src.quarantined_records, 2);
    assert_eq!(src.quarantine_by_class.get("wrong_field_type"), Some(&1));
    assert_eq!(src.quarantine_by_class.get("truncated_structure"), Some(&1));
}

/// Unit: the check is absent from cargo test and bin/ci as a gating requirement
/// on host live files, preserving the headless rule.
///
/// aub-lqe.17 requires the drift check to stay out of `cargo test` and
/// `bin/ci`, asserted by its invocation path. The property holds today but
/// nothing guards it, so this test enumerates every occurrence of the drift
/// flag across the gate paths and fails naming file and line on anything
/// outside the allowlist. One invocation is legitimate: the e2e case over
/// synthetic roots it builds itself, which never reads the host live
/// transcripts.
#[test]
fn unit_check_absent_from_headless_ci_and_default_test_suite() {
    let doctor = agent_usage_book::cli::Command::Doctor;
    assert_eq!(doctor.name(), "doctor");
    assert!(doctor.summary().is_some());
    let policy = doctor.flag_policy();
    assert_eq!(policy.format, agent_usage_book::cli::FlagSupport::Accepted);
    assert_eq!(
        policy.verbosity,
        agent_usage_book::cli::FlagSupport::Accepted
    );

    assert_no_unallowlisted_drift_flag_in_gate_paths();
    assert_no_unignored_default_root_drift_test();
}

/// The drift flag this test polices, written once as the single search needle.
const DRIFT_FLAG: &str = "transcript-format-drift";

/// The drift-check entry this test polices at the Rust level, built without
/// ever writing the call text contiguously so this file's own implementation
/// never reads as an invocation.
const DRIFT_INVOKE_NEEDLE: &str = "detect_drift";

/// Every gate path allowed to name the drift flag, each entry carrying the
/// reason it never reads the host live transcripts.
fn drift_flag_allowlist() -> Vec<(String, String)> {
    vec![
        (
            "tests/e2e/cases/012-doctor-transcript-format-drift.sh".to_string(),
            "the allowlisted e2e case invokes the drift check over synthetic roots it builds itself in case_preconditions, never the host live transcripts"
                .to_string(),
        ),
        (
            "tests/doctor_transcript_drift.rs".to_string(),
            "this exclusion test: the flag text here is the search needle and the allowlist itself, and every drift invocation in this file points at synthetic StateDir roots or an empty config"
                .to_string(),
        ),
    ]
}

/// The repository root the gate paths resolve against. `CARGO_MANIFEST_DIR` is
/// a compile-time constant, so the scan follows the checkout under test into
/// the guard-mutations scratch copy as well.
fn gate_scan_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively collects every file under `dir` into `out`, sorted for a stable
/// failure message. A missing directory contributes nothing.
fn collect_files_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_files_recursive(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Collects the top-level files with extension `extension` directly under `dir`.
fn collect_top_level_files_with_extension(
    dir: &std::path::Path,
    extension: &str,
    out: &mut Vec<std::path::PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| path.extension().and_then(|entry| entry.to_str()) == Some(extension))
        .collect();
    paths.sort();
    out.extend(paths);
}

/// Fails naming file and line on every drift-flag occurrence in the gate paths
/// that the allowlist does not cover. New gate checks other beads add under
/// `bin/checks/` fall inside this scan set by design.
fn assert_no_unallowlisted_drift_flag_in_gate_paths() {
    let root = gate_scan_root();
    let allowlist = drift_flag_allowlist();

    let mut scanned: Vec<std::path::PathBuf> = Vec::new();
    scanned.push(root.join("bin/ci"));
    collect_files_recursive(&root.join("bin/checks"), &mut scanned);
    collect_files_recursive(&root.join(".github/workflows"), &mut scanned);
    collect_top_level_files_with_extension(&root.join("tests"), "rs", &mut scanned);
    collect_top_level_files_with_extension(&root.join("tests/e2e/cases"), "sh", &mut scanned);
    // tests/e2e/runs/ is deliberately never scanned: run logs record past
    // output, not gate intent.

    let mut violations = Vec::new();
    for path in &scanned {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let allowed = allowlist
            .iter()
            .any(|(allowed_path, _)| allowed_path == &relative);
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                violations.push(format!("{relative}: unreadable ({error})"));
                continue;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            if line.contains(DRIFT_FLAG) && !allowed {
                violations.push(format!("{}:{}: {}", relative, index + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "drift flag invoked outside the allowlist; aub-lqe.17 keeps the drift check out of the headless gate:\n{}",
        violations.join("\n")
    );
}

/// One function item found in an integration test file: its name, the 1-based
/// line defining it, whether it carries the test attribute, whether it is
/// ignored, and its body text.
struct TestFileFunction {
    name: String,
    line: u64,
    is_test: bool,
    has_ignore: bool,
    body: String,
}

/// Splits `text` into function items. A function starts at a line whose first
/// non-whitespace is a function definition and ends at the first later line
/// that is a lone closing brace at or above the definition indent, which
/// closes top-level items at column zero and proptest inner items at their own
/// indent alike.
fn test_file_functions(text: &str) -> Vec<TestFileFunction> {
    let lines: Vec<&str> = text.lines().collect();
    let mut functions = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        let stripped = trimmed
            .strip_prefix("pub(crate) fn ")
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("fn "));
        let Some(rest) = stripped else {
            index += 1;
            continue;
        };
        let indent = lines[index].len() - trimmed.len();
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // Attribute lines directly above the definition, skipping blanks and
        // comments.
        let mut is_test = false;
        let mut has_ignore = false;
        let mut cursor = index;
        while cursor > 0 {
            cursor -= 1;
            let above = lines[cursor].trim();
            if above.is_empty() || above.starts_with("//") {
                continue;
            }
            if above.starts_with("#[") {
                if above.contains("#[test]") {
                    is_test = true;
                }
                if above.contains("ignore") {
                    has_ignore = true;
                }
                continue;
            }
            break;
        }
        // The body runs to the lone closing brace at or above the definition
        // indent. A lone brace inside a multi-line string literal would end
        // the scan early, which can only drop markers and therefore only fail
        // louder, never pass quieter.
        let mut end = lines.len();
        for (offset, line) in lines.iter().enumerate().skip(index + 1) {
            let body_trimmed = line.trim();
            if body_trimmed == "}" && line.len() - body_trimmed.len() <= indent {
                end = offset + 1;
                break;
            }
        }
        functions.push(TestFileFunction {
            name,
            line: (index + 1) as u64,
            is_test,
            has_ignore,
            body: lines[index..end].join("\n"),
        });
        index = end;
    }
    functions
}

/// A body invokes the drift check when it calls the entry by name. The call
/// text is assembled from the needle so this file's own implementation never
/// contains it contiguously.
fn invokes_drift_check(body: &str) -> bool {
    body.contains(&format!("{}(", DRIFT_INVOKE_NEEDLE))
}

/// A body carries an explicit opt-in environment variable gate.
fn has_env_opt_in(body: &str) -> bool {
    body.contains("std::env::") || body.contains("env::var") || body.contains("option_env!")
}

/// A body shows its roots are synthetic: temporary directories it built itself
/// rather than the operator defaults.
fn uses_synthetic_roots(body: &str) -> bool {
    [
        "StateDir",
        "FakeEnv",
        "tempfile",
        "tempdir",
        "config_from_toml",
        "synthetic",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

/// Fails on every integration test that invokes the drift check against the
/// default roots while still running in the default suite: without `#[ignore]`
/// or an explicit opt-in environment variable. Tests over synthetic roots they
/// built themselves are not against the defaults and pass. A same-file helper
/// that wraps the check lends its body to the test that calls it, so routing
/// the invocation through a helper neither passes nor fails on its own.
fn assert_no_unignored_default_root_drift_test() {
    let root = gate_scan_root();
    let tests_dir = root.join("tests");
    let entries = std::fs::read_dir(&tests_dir).expect("tests dir must be readable");
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| path.extension().and_then(|entry| entry.to_str()) == Some("rs"))
        .collect();
    paths.sort();

    let mut violations = Vec::new();
    for path in &paths {
        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(path).expect("test file must be readable");
        let text = String::from_utf8_lossy(&bytes);
        let functions = test_file_functions(&text);
        let helpers: Vec<&TestFileFunction> = functions
            .iter()
            .filter(|function| !function.is_test && invokes_drift_check(&function.body))
            .collect();
        for function in functions.iter().filter(|function| function.is_test) {
            let calls_helper = helpers
                .iter()
                .any(|helper| function.body.contains(&format!("{}(", helper.name)));
            if !invokes_drift_check(&function.body) && !calls_helper {
                continue;
            }
            let mut effective = function.body.clone();
            for helper in &helpers {
                if function.body.contains(&format!("{}(", helper.name)) {
                    effective.push_str(&helper.body);
                }
            }
            if function.has_ignore {
                continue;
            }
            if has_env_opt_in(&effective) {
                continue;
            }
            if uses_synthetic_roots(&effective) {
                continue;
            }
            violations.push(format!(
                "{}:{}: test `{}` invokes the drift check without #[ignore] or an explicit opt-in environment variable; point it at synthetic roots it builds itself",
                relative, function.line, function.name
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "drift check reachable from the default test suite against default roots:\n{}",
        violations.join("\n")
    );
}

// Property: over generated corpora seeded with transcript-like content,
// the report contains field names and counts and no content substring.
proptest! {
    #[test]
    fn property_report_contains_no_transcript_content_substring(
        prompt_content in "[a-zA-Z0-9_-]{20,50}",
        response_content in "[a-zA-Z0-9_-]{20,50}",
        user_name in "[a-z]{5,15}",
    ) {
        let tmp = StateDir::new();
        let root = tmp.path().join("transcripts");
        fs::create_dir_all(&root).expect("create root");

        let file = root.join("session.jsonl");
        let line = format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"s-{user_name}\",\"timestamp\":\"2026-08-25T10:00:00.000Z\",\"user_prompt\":\"{prompt_content}\",\"model_response\":\"{response_content}\",\"message\":{{\"id\":\"m1\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n"
        );
        fs::write(&file, line).expect("write file");

        let toml = format!(
            r#"
[[transcripts]]
name = "claude-code"
root = "{}"
pattern = "**/*.jsonl"
format = "claude-code"
"#,
            root.display()
        );
        let cfg = config_from_toml(&toml);

        let timestamp = UtcTimestamp::from_unix_nanos(1_000_000);
        let report = detect_drift(&cfg, None, timestamp, None).expect("drift succeeds");

        let text = render_doctor_drift_report(&report);
        let json = doctor_drift_json(&report, RunId::from_string("run-prop".to_string()));

        prop_assert!(!text.contains(&prompt_content), "prompt content leaked into text: {text}");
        prop_assert!(!text.contains(&response_content), "response content leaked into text: {text}");
        prop_assert!(!json.contains(&prompt_content), "prompt content leaked into json: {json}");
        prop_assert!(!json.contains(&response_content), "response content leaked into json: {json}");

        let text_hits = matched_patterns(&text);
        let json_hits = matched_patterns(&json);
        prop_assert!(text_hits.is_empty(), "forbidden patterns in text: {text_hits:?}");
        prop_assert!(json_hits.is_empty(), "forbidden patterns in json: {json_hits:?}");
    }
}
