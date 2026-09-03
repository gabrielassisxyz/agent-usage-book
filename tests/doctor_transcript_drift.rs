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
