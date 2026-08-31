//! The transcript fixture corpus audit (aub-lqe.14).
//!
//! One test file owns the properties no individual parser bead can assert:
//! across every supported format, all nine catalog shapes are present (each
//! with a fixture or a machine-readable not-applicable rationale), every
//! fixture states the input-format version it represents, the whole directory
//! passes the shared sanitization scan, and the nested-only discovery fixture
//! stays inside the audited corpus.
//!
//! The audit reads rather than restates: the shape catalog comes from
//! `FIXTURE_CATALOG`, the forbidden-pattern list from
//! `test_support::sanitization`, and the per-format coverage from
//! `native/MANIFEST.json`. A shape added to the catalog fails the audit until
//! the manifest covers it, and a fixture that matches a forbidden pattern
//! fails the scan naming the file.

use std::collections::BTreeSet;
use std::path::Path;

use agent_usage_book::transcripts::native::fixture_coverage;
use agent_usage_book::transcripts::{
    FIXTURE_CATALOG, FixtureCoverage, FixtureShape, ParserAdapter, parser_for_format,
};
use serde_json::Value;
use test_support::sanitization::matched_patterns;

/// The corpus root, relative to the crate root.
const CORPUS_ROOT: &str = "tests/fixtures/transcripts";
/// The parser fixture directory.
const NATIVE_DIR: &str = "tests/fixtures/transcripts/native";
/// The per-format coverage declaration.
const MANIFEST: &str = "tests/fixtures/transcripts/native/MANIFEST.json";
/// The golden expected-output directory.
const EXPECTED_DIR: &str = "tests/fixtures/transcripts/native/expected";
/// The documented capture and sanitization procedure.
const PROCEDURE_DOC: &str = "tests/fixtures/transcripts/README.md";
/// The nested-only discovery fixture directory.
const NESTED_ONLY_DIR: &str = "tests/fixtures/transcripts/nested-only";
/// A string longer than this in a fixture is content the parser does not
/// require. The longest string the corpus legitimately needs is the nested
/// subagent path reference (62 characters).
const MAX_FIXTURE_STRING_LEN: usize = 120;

fn crate_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    let path = crate_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} unreadable: {e}", path.display()))
}

fn load_manifest() -> Value {
    let text = read(MANIFEST);
    serde_json::from_str(&text).expect("MANIFEST.json must parse as JSON")
}

/// The catalog shape a manifest shape name denotes, or `None` for a name the
/// catalog does not know. An unknown name is a manifest defect, not a shape.
fn shape_from_name(name: &str) -> Option<FixtureShape> {
    match name {
        "SimpleSession" => Some(FixtureShape::SimpleSession),
        "NestedSubagentPaths" => Some(FixtureShape::NestedSubagentPaths),
        "TruncatedFile" => Some(FixtureShape::TruncatedFile),
        "PartiallyWrittenFinalRecord" => Some(FixtureShape::PartiallyWrittenFinalRecord),
        "FileRotation" => Some(FixtureShape::FileRotation),
        "MalformedRecords" => Some(FixtureShape::MalformedRecords),
        "ModelChangeMidSession" => Some(FixtureShape::ModelChangeMidSession),
        "CacheReadsAndWrites" => Some(FixtureShape::CacheReadsAndWrites),
        "NoNativeUsageField" => Some(FixtureShape::NoNativeUsageField),
        _ => None,
    }
}

/// The formats the manifest declares, in manifest order.
fn manifest_formats(manifest: &Value) -> Vec<(String, &Value)> {
    let Some(formats) = manifest.get("formats").and_then(Value::as_object) else {
        panic!("MANIFEST.json must have a formats object");
    };
    formats
        .iter()
        .map(|(name, format)| (name.clone(), format))
        .collect()
}

/// The shape entries one format declares, in manifest order.
fn manifest_shapes(format: &Value) -> Vec<(String, &Value)> {
    let Some(shapes) = format.get("shapes").and_then(Value::as_object) else {
        panic!("a format entry must have a shapes object");
    };
    shapes
        .iter()
        .map(|(name, entry)| (name.clone(), entry))
        .collect()
}

/// The fixture a shape entry names, when it is an applicable fixture.
fn entry_fixture(entry: &Value) -> Option<String> {
    entry
        .get("fixture")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The not-applicable rationale a shape entry carries, when it has one.
fn entry_rationale(entry: &Value) -> Option<String> {
    entry
        .get("not_applicable")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The completeness audit: every format-shape pair must carry an applicable
/// fixture that exists on disk, or a non-empty not-applicable rationale.
/// Returns one failure string per defect, each naming the format and the shape.
fn completeness_failures(manifest: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (format, format_entry) in manifest_formats(manifest) {
        if parser_for_format(&format).is_none() {
            failures.push(format!("format {format} is not a supported format"));
            continue;
        }
        let real_capture = format_entry
            .get("real_capture")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("format {format} must declare a real_capture fixture"));
        let real_path = crate_root().join(NATIVE_DIR).join(real_capture);
        if !real_path.exists() {
            failures.push(format!(
                "{format}: real-capture fixture {real_capture} does not exist"
            ));
        }
        let mut covered: BTreeSet<FixtureShape> = BTreeSet::new();
        for (shape_name, entry) in manifest_shapes(format_entry) {
            let Some(shape) = shape_from_name(&shape_name) else {
                failures.push(format!("{format}: {shape_name} is not a catalog shape"));
                continue;
            };
            covered.insert(shape);
            match (entry_fixture(entry), entry_rationale(entry)) {
                (Some(fixture), _) => {
                    let path = crate_root().join(NATIVE_DIR).join(&fixture);
                    if !path.exists() {
                        failures.push(format!(
                            "{format}: {shape_name} names fixture {fixture} which does not exist"
                        ));
                    }
                }
                (None, Some(rationale)) => {
                    if rationale.trim().is_empty() {
                        failures.push(format!(
                            "{format}: {shape_name} has an empty not-applicable rationale"
                        ));
                    }
                }
                (None, None) => failures.push(format!(
                    "{format}: {shape_name} has neither a fixture nor a not-applicable rationale"
                )),
            }
        }
        for shape in FIXTURE_CATALOG {
            if !covered.contains(&shape) {
                failures.push(format!("{format}: {shape:?} is missing from the manifest"));
            }
        }
    }
    failures
}

/// Every fixture file in the native directory must be declared in the
/// manifest, so a stray file beside the corpus is a defect rather than a
/// silent addition.
fn undeclared_fixture_failures(manifest: &Value) -> Vec<String> {
    let mut declared: BTreeSet<String> = manifest_formats(manifest)
        .into_iter()
        .flat_map(|(_, format)| manifest_shapes(format))
        .filter_map(|(_, entry)| entry_fixture(entry))
        .collect();
    for (_, format) in manifest_formats(manifest) {
        if let Some(real_capture) = format.get("real_capture").and_then(Value::as_str) {
            declared.insert(real_capture.to_string());
        }
    }
    let mut failures = Vec::new();
    let dir = crate_root().join(NATIVE_DIR);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} unreadable: {e}", dir.display()))
        .map(|entry| entry.expect("read_dir entry"))
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".jsonl") && !declared.contains(&name) {
            failures.push(format!("fixture {name} is not declared in the manifest"));
        }
    }
    failures
}

/// The golden audit: every applicable fixture parses to its expected-output
/// file. Returns one failure string per mismatch, naming the fixture.
fn golden_failures(manifest: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (format, format_entry) in manifest_formats(manifest) {
        let Some(parser) = parser_for_format(&format) else {
            continue;
        };
        for (shape_name, entry) in manifest_shapes(format_entry) {
            let Some(fixture) = entry_fixture(entry) else {
                continue;
            };
            let input = read(&format!("{NATIVE_DIR}/{fixture}"));
            let output = parser.parse(
                &input,
                &agent_usage_book::transcripts::SourceLocation::new(&fixture, 1),
            );
            let actual: Vec<(u64, u64, u64, u64)> = output
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
            let expected_stem = fixture.strip_suffix(".jsonl").unwrap_or(&fixture);
            let expected: Value =
                serde_json::from_str(&read(&format!("{EXPECTED_DIR}/{expected_stem}.json")))
                    .unwrap_or_else(|e| panic!("expected output for {fixture} must parse: {e}"));
            let expected_events: Vec<(u64, u64, u64, u64)> = expected
                .get("events")
                .and_then(Value::as_array)
                .expect("expected output must have an events array")
                .iter()
                .map(|event| {
                    let get = |key: &str| {
                        event.get(key).and_then(Value::as_u64).unwrap_or_else(|| {
                            panic!("expected event for {fixture} must have {key}")
                        })
                    };
                    (
                        get("input"),
                        get("output"),
                        get("cache_read"),
                        get("cache_write"),
                    )
                })
                .collect();
            let expected_quarantined = expected
                .get("quarantined")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| panic!("expected output for {fixture} must have quarantined"));
            if actual != expected_events {
                failures.push(format!(
                    "{fixture} ({format}, {shape_name}): events {actual:?} do not match expected {expected_events:?}"
                ));
            }
            if output.quarantined().len() as u64 != expected_quarantined {
                failures.push(format!(
                    "{fixture} ({format}, {shape_name}): quarantine count {} does not match expected {expected_quarantined}",
                    output.quarantined().len()
                ));
            }
        }
    }
    failures
}

/// The real-capture audit: every format's real-capture fixture parses with
/// zero quarantines, so the corpus always pins the shape real sources write.
fn real_capture_failures(manifest: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (format, format_entry) in manifest_formats(manifest) {
        let Some(parser) = parser_for_format(&format) else {
            continue;
        };
        let Some(real_capture) = format_entry.get("real_capture").and_then(Value::as_str) else {
            continue;
        };
        let input = read(&format!("{NATIVE_DIR}/{real_capture}"));
        let output = parser.parse(
            &input,
            &agent_usage_book::transcripts::SourceLocation::new(real_capture, 1),
        );
        if !output.quarantined().is_empty() {
            failures.push(format!(
                "{format}: real-capture fixture {real_capture} quarantines {} records",
                output.quarantined().len()
            ));
        }
    }
    failures
}

/// The version audit: each format's declared input-format version must match
/// what its parser declares.
fn version_failures(manifest: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (format, format_entry) in manifest_formats(manifest) {
        let declared = format_entry
            .get("input_format_version")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("format {format} must declare input_format_version"));
        let Some(parser) = parser_for_format(&format) else {
            continue;
        };
        let actual = parser.input_format_version();
        if declared != actual.as_str() {
            failures.push(format!(
                "{format}: manifest declares input format {declared}, parser declares {}",
                actual.as_str()
            ));
        }
    }
    failures
}

/// The sanitization audit: no file under the corpus root matches any shared
/// forbidden pattern.
fn sanitization_failures() -> Vec<String> {
    let mut failures = Vec::new();
    let root = crate_root().join(CORPUS_ROOT);
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{} unreadable: {e}", dir.display()))
            .map(|entry| entry.expect("read_dir entry"))
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{} unreadable: {e}", path.display()));
                let hits = matched_patterns(&text);
                if !hits.is_empty() {
                    failures.push(format!(
                        "{} matches forbidden patterns {hits:?}",
                        path.strip_prefix(crate_root()).unwrap_or(&path).display()
                    ));
                }
            }
        }
    }
    failures
}

/// The minimality audit: no string value in any fixture is longer than the
/// parser behaviour can require. A pasted text blob is the failure this
/// catches; the longest legitimate string is the nested subagent path
/// reference.
fn minimality_failures() -> Vec<String> {
    let mut failures = Vec::new();
    let dir = crate_root().join(NATIVE_DIR);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} unreadable: {e}", dir.display()))
        .map(|entry| entry.expect("read_dir entry"))
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if !path.extension().is_some_and(|ext| ext == "jsonl") {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        for (index, line) in read(&format!("{NATIVE_DIR}/{name}")).lines().enumerate() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue; // a truncated line is the shape, not content
            };
            let mut stack = vec![value];
            while let Some(value) = stack.pop() {
                match value {
                    Value::String(s) if s.len() > MAX_FIXTURE_STRING_LEN => failures.push(format!(
                        "{name} line {} has a {}-character string; fixtures retain only parser-required content",
                        index + 1,
                        s.len()
                    )),
                    Value::Array(items) => stack.extend(items),
                    Value::Object(map) => stack.extend(map.into_values()),
                    _ => {}
                }
            }
        }
    }
    failures
}

/// The documented capture and sanitization procedure, applied to a synthetic
/// capture. Mirrors the steps in `tests/fixtures/transcripts/README.md`:
/// trim to the fields the parser reads, drop text content, replace
/// identifying values, and remove credential-shaped content.
fn sanitize_capture(raw: &str) -> String {
    const CREDENTIAL_FIELD_NAMES: [&str; 6] = [
        "api_key",
        "apikey",
        "api-key",
        "authorization",
        "password",
        "secret",
    ];
    let mut out = Vec::new();
    for line in raw.lines() {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            out.push(line.to_string());
            continue;
        };
        let Some(message) = value.get_mut("message").and_then(Value::as_object_mut) else {
            out.push(line.to_string());
            continue;
        };
        message.remove("content");
        message.remove("role");
        if let Some(usage) = message.get_mut("usage").and_then(Value::as_object_mut) {
            usage.retain(|key, _| {
                matches!(
                    key.as_str(),
                    "input_tokens"
                        | "output_tokens"
                        | "cache_read_input_tokens"
                        | "cache_creation_input_tokens"
                )
            });
        }
        if let Some(record) = value.as_object_mut() {
            record.retain(|key, _| !CREDENTIAL_FIELD_NAMES.contains(&key.as_str()));
        }
        out.push(serde_json::to_string(&value).expect("sanitized record must serialize"));
    }
    out.join("\n")
}

// --- the audit tests --------------------------------------------------------

/// The corpus is complete: every supported format covers every catalog shape
/// with a fixture that exists or a non-empty rationale, and no fixture file
/// sits undeclared beside the corpus.
#[test]
fn corpus_audit_is_complete_for_every_format_and_shape() {
    let manifest = load_manifest();
    let failures = completeness_failures(&manifest);
    assert!(
        failures.is_empty(),
        "corpus audit found defects:\n{}",
        failures.join("\n")
    );
    let undeclared = undeclared_fixture_failures(&manifest);
    assert!(
        undeclared.is_empty(),
        "corpus audit found undeclared fixtures:\n{}",
        undeclared.join("\n")
    );
    let real_capture = real_capture_failures(&manifest);
    assert!(
        real_capture.is_empty(),
        "corpus audit found real-capture defects:\n{}",
        real_capture.join("\n")
    );
}

/// Removing one shape from one format makes the audit fail naming both.
#[test]
fn removing_one_shape_from_one_format_fails_naming_both() {
    let mut manifest = load_manifest();
    let formats = manifest
        .get_mut("formats")
        .and_then(Value::as_object_mut)
        .expect("formats object");
    let claude = formats.get_mut("claude-code").expect("claude-code format");
    let shapes = claude
        .get_mut("shapes")
        .and_then(Value::as_object_mut)
        .expect("shapes object");
    shapes.remove("TruncatedFile");

    let failures = completeness_failures(&manifest);
    let joined = failures.join("\n");
    assert!(
        joined.contains("claude-code") && joined.contains("TruncatedFile"),
        "the failure must name the format and the shape, got:\n{joined}"
    );
}

/// A format-shape pair with neither a fixture nor a rationale fails the audit
/// naming both, even when every other pair is covered.
#[test]
fn a_pair_with_neither_fixture_nor_rationale_fails_naming_both() {
    let mut manifest = load_manifest();
    let formats = manifest
        .get_mut("formats")
        .and_then(Value::as_object_mut)
        .expect("formats object");
    let pi = formats.get_mut("pi").expect("pi format");
    let shapes = pi
        .get_mut("shapes")
        .and_then(Value::as_object_mut)
        .expect("shapes object");
    let cache = shapes.get_mut("CacheReadsAndWrites").expect("cache entry");
    *cache = serde_json::json!({});

    let failures = completeness_failures(&manifest);
    let joined = failures.join("\n");
    assert!(
        joined.contains("pi") && joined.contains("CacheReadsAndWrites"),
        "the failure must name the format and the shape, got:\n{joined}"
    );
}

/// The nested-only discovery fixture is part of the audited corpus: the audit
/// fails when it is moved out, because the directory is asserted here and the
/// sanitization scan covers the whole corpus root.
#[test]
fn nested_subagent_fixture_is_part_of_the_audited_corpus() {
    let dir = crate_root().join(NESTED_ONLY_DIR);
    assert!(dir.is_dir(), "nested-only fixture directory must exist");
    let mut stack = vec![dir.clone()];
    let mut files = 0usize;
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("nested-only must be readable") {
            let entry = entry.expect("read_dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files += 1;
            }
        }
    }
    assert!(
        files > 0,
        "nested-only fixture must contain transcript files"
    );
    // The corpus scan covers the nested-only directory too, so a file moved
    // out of the corpus root is no longer scanned and this test fails.
    let root = crate_root().join(CORPUS_ROOT);
    assert!(
        dir.starts_with(&root),
        "nested-only must live inside the corpus root"
    );
}

/// The whole corpus passes the shared sanitization scan.
#[test]
fn sanitization_scan_finds_no_forbidden_patterns_in_the_corpus() {
    let failures = sanitization_failures();
    assert!(
        failures.is_empty(),
        "sanitization scan found forbidden content:\n{}",
        failures.join("\n")
    );
}

/// Every fixture states the input-format version it represents, and the
/// declared version matches the parser that reads it.
#[test]
fn every_fixture_states_its_input_format_version() {
    let manifest = load_manifest();
    let failures = version_failures(&manifest);
    assert!(
        failures.is_empty(),
        "version audit found mismatches:\n{}",
        failures.join("\n")
    );
}

/// Every applicable fixture parses to its golden expected-output file.
#[test]
fn golden_outputs_match_every_applicable_fixture() {
    let manifest = load_manifest();
    let failures = golden_failures(&manifest);
    assert!(
        failures.is_empty(),
        "golden audit found mismatches:\n{}",
        failures.join("\n")
    );
}

/// The parser bead's own coverage map is a subset of the manifest: a fixture
/// the parser contract names must be declared in the audited corpus.
#[test]
fn parser_contract_fixtures_are_declared_in_the_manifest() {
    let manifest = load_manifest();
    let declared: BTreeSet<String> = manifest_formats(&manifest)
        .into_iter()
        .flat_map(|(_, format)| manifest_shapes(format))
        .filter_map(|(_, entry)| entry_fixture(entry))
        .collect();
    for (shape, coverage) in fixture_coverage() {
        if let FixtureCoverage::Applicable { fixture } = coverage {
            assert!(
                declared.contains(&fixture),
                "parser contract fixture {fixture} for {shape:?} is not declared in the manifest"
            );
        }
    }
}

/// Fixtures retain only as much content as the parser behaviour requires.
#[test]
fn fixtures_retain_only_parser_required_content() {
    let failures = minimality_failures();
    assert!(
        failures.is_empty(),
        "minimality audit found content blobs:\n{}",
        failures.join("\n")
    );
}

/// The documented capture and sanitization procedure, exercised on a synthetic
/// capture: the sanitized result passes the scan, parses to the expected
/// events, and retains no text content.
#[test]
fn capture_and_sanitization_procedure_produces_a_clean_fixture() {
    let doc = read(PROCEDURE_DOC);
    assert!(
        doc.contains("Capturing a new fixture"),
        "the procedure must be documented in one place"
    );

    let raw = r#"{"type":"assistant","message":{"id":"msg_0001","model":"claude-opus-4","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}},"timestamp":"2026-08-25T10:00:00Z","uuid":"uuid-0001","api_key":"sk-ant-1234567890abcdef"}"#
        .to_string()
        + "\n"
        + r#"{"type":"assistant","message":{"id":"msg_0002","model":"claude-opus-4","content":[{"type":"text","text":"a long pasted conversation body that the parser never reads and the fixture must not retain"}],"usage":{"input_tokens":200,"output_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":20}},"timestamp":"2026-08-25T10:01:00Z","uuid":"uuid-0002"}"#;

    // The raw capture is not publishable: it carries a credential and text.
    assert!(
        !matched_patterns(&raw).is_empty(),
        "the synthetic capture must contain a forbidden pattern to be a real test"
    );

    let sanitized = sanitize_capture(&raw);
    assert!(
        matched_patterns(&sanitized).is_empty(),
        "the sanitized fixture must pass the scan"
    );
    assert!(
        !sanitized.contains("content"),
        "the sanitized fixture must not retain text content"
    );

    let parser = agent_usage_book::transcripts::ClaudeCodeParser;
    let output = parser.parse(
        &sanitized,
        &agent_usage_book::transcripts::SourceLocation::new("synthetic.jsonl", 1),
    );
    let events: Vec<(u64, u64, u64, u64)> = output
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
    assert_eq!(events, vec![(100, 50, 20, 10), (200, 100, 40, 20)]);
    assert_eq!(output.quarantined().len(), 0);
}
