//! One ledger for LLM consumption: token spend read from local agent transcripts, joined
//! with quota measured at the provider's own endpoints.

// Core matches over Freshness and AttemptOutcome (aub-rif.9) use no wildcard arm.
// Crate-wide because no lint scoped to two enums exists in clippy and a hand-written
// test is not a lint; settled as an orchestrator ruling on aub-rif.9 after the
// alternative (a lint scoped to just those two types) was confirmed not to exist.
#![deny(clippy::wildcard_enum_match_arm)]

pub mod build_info;

pub mod advice;
pub mod attribution;
pub mod auth;
pub mod backup;
pub mod calibration;
pub mod cli;
pub mod config;
pub mod cost_model;
pub mod coverage;
pub mod dedup;
pub mod domain;
pub mod error;
pub mod error_output;
pub mod evidence;
pub mod ingest;
pub mod logging;
pub mod meter;
pub mod presentation;
pub mod problem_code;
pub mod projection;
pub mod rate_book;
pub mod report;
pub mod sessions;
pub mod store;
pub mod transcripts;
pub mod valuation;

#[cfg(test)]
mod tests {
    use std::fs;

    /// The 21 modules from the design's module table plus diagnostics.
    const DESIGN_MODULES: [&str; 21] = [
        "domain",
        "evidence",
        "config",
        "store",
        "meter",
        "logging",
        "auth",
        "projection",
        "transcripts",
        "dedup",
        "sessions",
        "attribution",
        "cost_model",
        "calibration",
        "valuation",
        "advice",
        "coverage",
        "backup",
        "report",
        "cli",
        "presentation",
    ];

    fn has_header_comment(source: &str) -> bool {
        source.trim_start().starts_with("//!")
    }

    #[test]
    fn every_design_module_is_declared_and_carries_a_header_comment() {
        let lib_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs");
        let lib_source = fs::read_to_string(lib_path).expect("src/lib.rs must exist");

        for name in DESIGN_MODULES {
            let declaration = format!("pub mod {name};");
            assert!(
                lib_source
                    .lines()
                    .any(|line| line.trim() == declaration.as_str()),
                "module {name} is not declared in src/lib.rs"
            );

            let file_path = format!("{}/src/{name}.rs", env!("CARGO_MANIFEST_DIR"));
            let dir_path = format!("{}/src/{name}/mod.rs", env!("CARGO_MANIFEST_DIR"));
            let source = fs::read_to_string(&file_path)
                .or_else(|_| fs::read_to_string(&dir_path))
                .unwrap_or_else(|e| panic!("module {name} is missing: {e}"));
            assert!(
                has_header_comment(&source),
                "module {name} has no //! header comment"
            );
        }
    }

    #[test]
    fn header_check_rejects_a_module_without_a_header() {
        assert!(has_header_comment("//! responsibility\n"));
        assert!(!has_header_comment("pub fn f() {}\n"));
        assert!(!has_header_comment(""));
    }

    /// One parsed row of the invariants table in `docs/INVARIANTS.md`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct InvariantRow {
        number: usize,
        invariant: String,
        enforcer: String,
        check: String,
    }

    /// Parses the invariants table out of `docs/INVARIANTS.md`.
    fn parse_invariant_rows(docs: &str) -> Vec<InvariantRow> {
        let lines: Vec<&str> = docs.lines().collect();
        let mut rows = Vec::new();
        let mut i = 0;
        while i < lines.len()
            && !lines[i].contains("Enforcing path")
            && !lines[i].contains("Enforcing module")
        {
            i += 1;
        }
        // Skip the header row and the `|---|` separator row.
        i += 2;
        while i < lines.len() {
            let line = lines[i].trim();
            if !line.starts_with('|') {
                break;
            }
            let cells: Vec<&str> = line.split('|').map(|c| c.trim()).collect();
            // cells = ["", "#", "Invariant", "Enforcing path", "Test or constraint", ""]
            if cells.len() >= 5
                && let Ok(num) = cells[1].parse::<usize>()
            {
                rows.push(InvariantRow {
                    number: num,
                    invariant: cells[2].to_string(),
                    enforcer: cells[3].to_string(),
                    check: cells[4].to_string(),
                });
            }
            i += 1;
        }
        rows
    }

    fn normalize_whitespace(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn extract_plan_invariants(plan_text: &str) -> Vec<String> {
        let start_marker = "# 42. Important invariants to document in the source tree";
        let section_start = plan_text
            .find(start_marker)
            .expect("PLAN.md section 42 must exist");
        let after_start = &plan_text[section_start..];
        let section_end = after_start.find("\n---").unwrap_or(after_start.len());
        let section = &after_start[..section_end];

        let mut invariants = Vec::new();
        let lines: Vec<&str> = section.lines().collect();
        let mut current_item = String::new();
        let mut current_num = 1;

        for line in lines {
            let trimmed = line.trim();
            let prefix = format!("{current_num}.");
            if trimmed.starts_with(&prefix) {
                if !current_item.is_empty() {
                    invariants.push(normalize_whitespace(&current_item));
                    current_item.clear();
                }
                current_num += 1;
                let text = trimmed.strip_prefix(&prefix).unwrap().trim();
                current_item.push_str(text);
            } else if !current_item.is_empty() && !trimmed.is_empty() && !trimmed.starts_with('#') {
                if trimmed == "These are more important than most implementation choices." {
                    continue;
                }
                current_item.push(' ');
                current_item.push_str(trimmed);
            }
        }
        if !current_item.is_empty() {
            invariants.push(normalize_whitespace(&current_item));
        }
        invariants
    }

    const INVARIANT_TRACKER_MEMBERSHIP_SKIPPED_MARKER: &str =
        "[INVARIANT_TRACKER_MEMBERSHIP_SKIPPED]";

    #[derive(Debug, PartialEq, Eq)]
    enum InvariantRowValidation {
        Enforced,
        Unenforced,
        TrackerMembershipSkipped,
    }

    fn validate_invariant_row(
        row: &InvariantRow,
        base_dir: &str,
    ) -> Result<InvariantRowValidation, String> {
        if row.enforcer.starts_with("unenforced") {
            let parts: Vec<&str> = row.enforcer.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(format!(
                    "invariant {} marked unenforced with no bead id: {:?}",
                    row.number, row.enforcer
                ));
            }
            let bead_id = parts[1];
            let issues_path = format!("{base_dir}/.beads/issues.jsonl");
            let issues_content = match fs::read_to_string(&issues_path) {
                Ok(content) => content,
                // `.beads/` is gitignored machine-local state (AGENTS.md), so it is
                // absent in a clean clone, including the isolation clone
                // bin/bead-close-verified uses. Skip the tracker-membership check
                // for this row rather than fail a clone for lacking machine state;
                // wherever the file exists, this check still runs and still fails
                // hard (validation_fails_when_a_named_unenforced_bead_is_absent_from_a_present_tracker
                // proves it against a fixture tracker, independent of whether this
                // machine happens to have a live one).
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(InvariantRowValidation::TrackerMembershipSkipped);
                }
                Err(e) => return Err(format!("cannot read .beads/issues.jsonl: {e}")),
            };
            let mut found = false;
            let mut is_open = false;
            for line in issues_content.lines() {
                if line.contains(&format!("\"id\":\"{bead_id}\"")) {
                    found = true;
                    if !line.contains("\"status\":\"closed\"") {
                        is_open = true;
                    }
                    break;
                }
            }
            if !found {
                return Err(format!(
                    "invariant {} names unenforced bead {bead_id} which does not exist in .beads/issues.jsonl",
                    row.number
                ));
            }
            if !is_open {
                return Err(format!(
                    "invariant {} names unenforced bead {bead_id} which is closed in tracker",
                    row.number
                ));
            }
            Ok(InvariantRowValidation::Unenforced)
        } else {
            let path = format!("{base_dir}/{}", row.enforcer);
            let file_path = std::path::Path::new(&path);
            if !file_path.exists() {
                return Err(format!(
                    "invariant {} names enforcer path {:?} which does not exist",
                    row.number, row.enforcer
                ));
            }
            let content = fs::read_to_string(file_path)
                .map_err(|e| format!("cannot read {:?}: {e}", row.enforcer))?;
            let test_identifier = row.check.split("::").last().unwrap_or(&row.check).trim();
            if !content.contains(test_identifier) {
                return Err(format!(
                    "invariant {} names check {:?} not found in {:?}",
                    row.number, test_identifier, row.enforcer
                ));
            }
            Ok(InvariantRowValidation::Enforced)
        }
    }

    /// Compares the table's invariant list against docs/PLAN.md section 42.
    #[test]
    fn invariants_document_matches_plan_section_42() {
        let docs = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/INVARIANTS.md"))
            .expect("docs/INVARIANTS.md must be readable");
        let plan = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/PLAN.md"))
            .expect("docs/PLAN.md must be readable");

        let rows = parse_invariant_rows(&docs);
        let plan_invariants = extract_plan_invariants(&plan);

        assert_eq!(
            rows.len(),
            plan_invariants.len(),
            "expected {} invariant rows from PLAN.md section 42, found {}",
            plan_invariants.len(),
            rows.len()
        );

        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                row.number,
                i + 1,
                "invariant numbering must be 1..=27 in order"
            );
            assert_eq!(
                row.invariant, plan_invariants[i],
                "invariant {} in docs/INVARIANTS.md does not match PLAN.md section 42",
                row.number
            );
        }
    }

    /// Every invariant row names an existing file path and test/check, or an open bead in the tracker.
    /// The document's summary counts are kept in sync by this test.
    #[test]
    fn every_invariant_names_existing_file_and_test_or_open_tracker_bead() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let docs = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/INVARIANTS.md"))
            .expect("docs/INVARIANTS.md must be readable");
        let rows = parse_invariant_rows(&docs);

        let mut enforced_count = 0;
        let mut unenforced_count = 0;
        let mut tracker_membership_skipped = false;

        for row in &rows {
            match validate_invariant_row(row, manifest_dir) {
                Ok(InvariantRowValidation::Enforced) => enforced_count += 1,
                Ok(InvariantRowValidation::Unenforced) => unenforced_count += 1,
                Ok(InvariantRowValidation::TrackerMembershipSkipped) => {
                    tracker_membership_skipped = true;
                    unenforced_count += 1;
                }
                Err(err) => panic!("{err}"),
            }
        }

        if tracker_membership_skipped {
            let tracker_path = format!("{manifest_dir}/.beads/issues.jsonl");
            eprintln!(
                "{INVARIANT_TRACKER_MEMBERSHIP_SKIPPED_MARKER} \
                 every_invariant_names_existing_file_and_test_or_open_tracker_bead: \
                 {tracker_path} absent (gitignored, machine-local); skipping the \
                 tracker-membership check for unenforced rows in this run"
            );
        }

        assert_eq!(enforced_count + unenforced_count, rows.len());
        assert!(
            docs.contains(&format!("{enforced_count} are enforced")),
            "docs/INVARIANTS.md summary count mismatch: expected {enforced_count} enforced"
        );
        assert!(
            docs.contains(&format!("{unenforced_count} are unenforced")),
            "docs/INVARIANTS.md summary count mismatch: expected {unenforced_count} unenforced"
        );
    }

    /// A scratch directory holding nothing but a minimal `.beads/issues.jsonl`, so a test
    /// can prove the tracker-membership check against a fixture without depending on this
    /// machine's live tracker state.
    struct ScratchTrackerDir(std::path::PathBuf);

    impl ScratchTrackerDir {
        fn with_issues(jsonl: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let suffix = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "aub-lib-invariant-tracker-test-{}-{suffix}",
                std::process::id()
            ));
            let beads_dir = dir.join(".beads");
            fs::create_dir_all(&beads_dir).expect("scratch .beads dir must be creatable");
            fs::write(beads_dir.join("issues.jsonl"), jsonl)
                .expect("scratch issues.jsonl must be writable");
            Self(dir)
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("scratch path must be valid UTF-8")
        }
    }

    impl Drop for ScratchTrackerDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A clean clone has no `.beads/` at all (gitignored, machine-local per AGENTS.md).
    /// An unenforced row must not error there: the `NotFound` arm classifies it
    /// unenforced without asserting tracker membership, which is exactly what lets
    /// `bin/ci` reach green on a fresh clone.
    #[test]
    fn validation_skips_the_tracker_check_when_no_beads_directory_exists_at_all() {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let suffix = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aub-lib-invariant-no-tracker-test-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir must be creatable");
        let row = InvariantRow {
            number: 99,
            invariant: "Fake invariant".to_string(),
            enforcer: "unenforced aub-anything".to_string(),
            check: "(some test)".to_string(),
        };
        let result = validate_invariant_row(&row, dir.to_str().unwrap());
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(result, Ok(InvariantRowValidation::TrackerMembershipSkipped));
    }

    /// Proves absence-tolerance for a missing tracker file (see the `NotFound` arm in
    /// `validate_invariant_row`) cannot silently disable the check: wherever
    /// `.beads/issues.jsonl` does exist, a row naming a bead id absent from it still
    /// fails, with the id in the message.
    #[test]
    fn validation_fails_when_a_named_unenforced_bead_is_absent_from_a_present_tracker() {
        let scratch = ScratchTrackerDir::with_issues("{\"id\":\"aub-real\",\"status\":\"open\"}\n");
        let row = InvariantRow {
            number: 99,
            invariant: "Fake invariant".to_string(),
            enforcer: "unenforced aub-does-not-exist".to_string(),
            check: "(some test)".to_string(),
        };
        let result = validate_invariant_row(&row, scratch.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("aub-does-not-exist"), "{err}");
    }

    /// Regression pin for the closed-bead row that made a previously verified HEAD red.
    #[test]
    fn validation_rejects_the_closed_lqe_7_fixture_with_the_regression_message() {
        let scratch =
            ScratchTrackerDir::with_issues("{\"id\":\"aub-lqe.7\",\"status\":\"closed\"}\n");
        let row = InvariantRow {
            number: 9,
            invariant: "Fake invariant".to_string(),
            enforcer: "unenforced aub-lqe.7".to_string(),
            check: "(some test)".to_string(),
        };
        assert_eq!(
            validate_invariant_row(&row, scratch.path()),
            Err(
                "invariant 9 names unenforced bead aub-lqe.7 which is closed in tracker"
                    .to_string()
            )
        );
    }

    /// The positive case for the same fixture shape: an open, present bead validates.
    #[test]
    fn validation_passes_when_a_named_unenforced_bead_is_open_in_a_present_tracker() {
        let scratch = ScratchTrackerDir::with_issues("{\"id\":\"aub-real\",\"status\":\"open\"}\n");
        let row = InvariantRow {
            number: 99,
            invariant: "Fake invariant".to_string(),
            enforcer: "unenforced aub-real".to_string(),
            check: "(some test)".to_string(),
        };
        assert_eq!(
            validate_invariant_row(&row, scratch.path()),
            Ok(InvariantRowValidation::Unenforced)
        );
    }

    /// Planted negative: changing an enforcer path to a non-existent file causes validation to fail.
    #[test]
    fn validation_fails_when_enforcer_path_does_not_exist() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let fake_row = InvariantRow {
            number: 99,
            invariant: "Fake invariant".to_string(),
            enforcer: "src/domain/nonexistent_file_for_negative_test.rs".to_string(),
            check: "tests::fake_test".to_string(),
        };
        let result = validate_invariant_row(&fake_row, manifest_dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }
}
