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
/// without reverse-engineering the harness. The procedure must name the checked
/// guard and the error-code rule, because a bare overwrite is how a fixture that
/// now fails for a different reason gets blessed (aub-tojp).
#[test]
fn regeneration_procedure_is_documented() {
    let readme = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compile_fail/README.md");
    let contents = fs::read_to_string(&readme)
        .expect("tests/compile_fail/README.md must document the regeneration procedure");
    assert!(
        contents.contains("TRYBUILD=overwrite"),
        "the regeneration procedure must name the TRYBUILD=overwrite command"
    );
    assert!(
        contents.contains("compile_fail_regenerate"),
        "the regeneration procedure must name the checked guard command"
    );
    assert!(
        contents.contains("error code"),
        "the regeneration procedure must state the error-code rule"
    );

    // The same rule must be stated in AGENTS.md, where a pane reads testing doctrine
    // before it ever reaches for a regeneration command (aub-tojp).
    let agents = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("AGENTS.md"))
        .expect("AGENTS.md must be readable");
    assert!(
        agents.contains("compile_fail_regenerate"),
        "AGENTS.md must name the checked regeneration guard"
    );
    assert!(
        agents.contains("never with a bare `TRYBUILD=overwrite`"),
        "AGENTS.md must forbid the bare overwrite"
    );
}

/// The example lines the design states inside `text` fences of section 34.1,
/// parsed from the upstream document itself so a case added upstream changes
/// the set this test demands instead of disappearing against a hardcoded copy
/// (aub-knw7). The exhaustiveness consequence is prose, not a fenced example;
/// its coverage is anchored to `aub-ai3.2` in the coverage document rather
/// than matched by text.
fn section_34_1_example_lines() -> Vec<String> {
    let plan = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/PLAN.md"))
        .expect("docs/PLAN.md must be readable");
    let start = plan
        .find("## 34.1")
        .expect("PLAN.md must contain design section 34.1");
    let rest = &plan[start..];
    let end = rest[1..].find("\n## ").map(|i| i + 1).unwrap_or(rest.len());
    let section = &rest[..end];

    let mut inside_fence = false;
    let mut cases = Vec::new();
    for line in section.lines() {
        if line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            continue;
        }
        if inside_fence {
            let case = line.trim();
            if !case.is_empty() {
                cases.push(case.to_string());
            }
        }
    }
    assert!(
        cases.len() >= 11,
        "the section 34.1 parse lost example rows: only {cases:?}"
    );
    cases
}

/// The coverage list must name every case the design section states, including
/// a case the design adds later: the checked set comes from parsing the section,
/// not from a hand copy.
#[test]
fn coverage_list_names_every_section_34_1_case() {
    let doc = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/compile-fail-coverage.md");
    let contents =
        fs::read_to_string(&doc).expect("docs/compile-fail-coverage.md must be readable");
    for case in section_34_1_example_lines() {
        assert!(
            contents.contains(&case),
            "coverage list does not name case: {case}"
        );
    }
}
