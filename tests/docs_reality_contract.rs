//! Contract tests for the three documents a reader takes at their word before
//! running anything: `README.md`, the header of `docs/PLAN.md`, and
//! `docs/operations.md` (aub-ngtu).
//!
//! These assert the claims that went stale once and would go stale again the
//! same way: a status section that describes an unimplemented system, a scope
//! list that contradicts a shipped command, a plan header still calling itself
//! pre-implementation, and an operations document with no import procedure on a
//! machine whose pre-`aub` history is the one thing no provider will re-answer.
//! Prose is not asserted word for word; each assertion names the claim, not the
//! sentence that carries it.

fn readme() -> String {
    std::fs::read_to_string("README.md").expect("README.md must exist")
}

fn plan() -> String {
    std::fs::read_to_string("docs/PLAN.md").expect("docs/PLAN.md must exist")
}

fn operations() -> String {
    std::fs::read_to_string("docs/operations.md").expect("docs/operations.md must exist")
}

/// The status section must not describe quota as unmeasured, and must name the
/// commands whose behaviour a reader predicts from it.
#[test]
fn readme_status_describes_the_implemented_system() {
    let doc = readme();
    let start = doc.find("## Status").expect("README has a Status section");
    let end = doc[start..]
        .find("## Why it exists")
        .expect("Status is followed by Why it exists")
        + start;
    let status = &doc[start..end];

    for retired in [
        "Quota is not measured yet",
        "until the sampler lands",
        "never observed",
    ] {
        assert!(
            !status.contains(retired),
            "README status still claims {retired:?}, which the sampler has made false"
        );
    }
    for command in ["aub status", "aub spend", "aub can-run"] {
        assert!(
            status.contains(command),
            "README status must let a reader predict {command}, and does not mention it"
        );
    }
    assert!(
        status.contains("What is not live:"),
        "README status must separate what is live from what is not"
    );
    for unlive in ["calibration", "No release"] {
        assert!(
            status.contains(unlive),
            "README status must name {unlive:?} among what is not live"
        );
    }
}

/// The scope list reads as a contradiction of `aub can-run` unless it
/// reconciles the two, and the reconciliation is only checkable against the
/// plan section that owns the distinction.
#[test]
fn readme_scope_reconciles_can_run_with_no_forecasting() {
    let doc = readme();
    let start = doc
        .find("## Not in scope")
        .expect("README has a Not in scope section");
    let end = doc[start..]
        .find("## Install")
        .expect("Not in scope is followed by Install")
        + start;
    let scope = &doc[start..end];

    assert!(
        scope.contains("Cost forecasting and budget enforcement"),
        "the forecasting exclusion must stay in scope's list"
    );
    assert!(
        scope.contains("can-run"),
        "scope excludes forecasting while can-run ships, so it must name can-run"
    );
    assert!(
        scope.contains("section 26"),
        "the reconciling sentence must cite the plan section that owns the distinction"
    );
    assert!(
        scope.contains("no duration forecasting"),
        "the reconciling sentence must quote the plan's own wording"
    );
    assert!(
        plan().contains("no duration forecasting"),
        "the plan wording the README quotes must still be in the plan"
    );
}

/// The plan header is the first thing read and was the last thing reconciled.
#[test]
fn plan_header_status_is_an_implemented_design_with_its_reconciliation_date() {
    let doc = plan();
    let line = doc
        .lines()
        .find(|line| line.starts_with("**Status:**"))
        .expect("PLAN carries a Status line in its header");

    assert!(
        !line.contains("pre-implementation"),
        "PLAN status still reads as pre-implementation: {line}"
    );
    assert!(
        line.contains("implemented"),
        "PLAN status must say the design is implemented: {line}"
    );
    // An ISO date, matched by shape rather than by a year this test would have to
    // be edited to outlive.
    let has_date = line.split_whitespace().any(|word| {
        let word = word.trim_end_matches(['.', ',']);
        let bytes = word.as_bytes();
        word.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && word.chars().enumerate().all(|(i, c)| {
                if i == 4 || i == 7 {
                    c == '-'
                } else {
                    c.is_ascii_digit()
                }
            })
    });
    assert!(
        has_date,
        "PLAN status must carry the date it was last reconciled: {line}"
    );
}

/// The import procedure is the one step a machine with pre-`aub` history cannot
/// reconstruct later, and it was missing from the operator's entry point.
#[test]
fn operations_doc_carries_the_pre_aub_import_procedure() {
    let doc = operations();
    let start = doc
        .find("## 6. Importing pre-aub history")
        .expect("operations doc must carry an import section for pre-aub history");
    let end = doc[start..]
        .find("## 7.")
        .expect("the import section is followed by the next numbered step")
        + start;
    let section = &doc[start..end];

    for command in ["aub import legacy-meter", "aub import seed-archive"] {
        assert!(
            section.contains(command),
            "the import section must name `{command}`"
        );
    }
    assert!(
        section.contains("--backup"),
        "the import section must show the verified-archive flag both importers refuse to write without"
    );
    assert!(
        section.contains("--vendor-account"),
        "the seed import cannot be run without its vendor mapping, so the section must show it"
    );
    for count in ["superseded_by_native", "quarantined"] {
        assert!(
            section.contains(count),
            "the import section must say what {count} count to expect"
        );
    }
    assert!(
        section.contains("aub-n27.8"),
        "the import section must point at the retirement of the two legacy writers"
    );
}
