//! Compile-fail harness. The project's central correctness claim is that dangerous
//! mistakes fail to compile, and a claim is only evidence if the failures are tested:
//! a type that was supposed to prevent an operation and quietly permits it looks
//! exactly like a type that works. Each `tests/compile_fail/*.rs` fixture must fail to
//! compile, and its expected compiler output is captured in the same-named `.stderr`
//! file, so a case failing for a new and unrelated reason is caught rather than
//! counted as a pass.

use std::fs;
use std::path::Path;

#[test]
fn compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}

/// Every fixture must carry its captured output, and that output must be non-empty. An
/// empty `.stderr` is the failure mode this harness exists to prevent: a case that
/// "fails to compile" with no captured reason passes when the file has a typo, and a
/// typo is not the invariant.
#[test]
fn every_fixture_has_captured_output() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile_fail");
    let mut fixtures = 0;
    for entry in fs::read_dir(&dir).expect("fixture directory must exist") {
        let path = entry.expect("fixture directory must be readable").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        fixtures += 1;
        let stderr = path.with_extension("stderr");
        let captured = fs::read_to_string(&stderr)
            .unwrap_or_else(|e| panic!("fixture {} has no captured output: {e}", path.display()));
        assert!(
            !captured.trim().is_empty(),
            "fixture {} has an empty .stderr; capture the expected output, not merely the failure",
            path.display()
        );
    }
    assert!(fixtures > 0, "the harness must have at least one fixture");
}

/// The regeneration procedure must be documented next to the fixtures, so a future
/// agent can refresh the captured output after changing a fixture or the toolchain
/// without reverse-engineering the harness.
#[test]
fn regeneration_procedure_is_documented() {
    let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile_fail/README.md");
    let contents = fs::read_to_string(&readme)
        .expect("tests/compile_fail/README.md must document the regeneration procedure");
    assert!(
        contents.contains("TRYBUILD=overwrite"),
        "the regeneration procedure must name the TRYBUILD=overwrite command"
    );
}

/// The coverage list must name every case in design section 34.1, so a case cannot be
/// dropped from the document while remaining in the design.
#[test]
fn coverage_list_names_every_section_34_1_case() {
    const CASES: [&str; 12] = [
        "TokenCount + Credits",
        "QuotaUsed + Money",
        "Credits passed to formatter",
        "QuotaRemaining passed as QuotaUsed",
        "USD added to another currency",
        "unwrap_or_default",
        "bare Display",
        "WindowCalibration outside",
        "CostModel without an observed TokenKind",
        "combine Measured and Estimated",
        "Derivation::Unavailable",
        "exhaustive model construction",
    ];
    let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/compile-fail-coverage.md");
    let contents =
        fs::read_to_string(&doc).expect("docs/compile-fail-coverage.md must be readable");
    for case in CASES {
        assert!(
            contents.contains(case),
            "coverage list does not name case: {case}"
        );
    }
}
