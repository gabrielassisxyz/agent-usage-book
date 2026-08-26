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
pub mod evidence;
pub mod logging;
pub mod meter;
pub mod presentation;
pub mod projection;
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
    struct InvariantRow {
        number: usize,
        enforcer: String,
        check: String,
    }

    /// Parses the invariants table out of `docs/INVARIANTS.md`. The table is
    /// the only Markdown table in the document; its header names the
    /// `Enforcing module` column, which is what locates it.
    fn parse_invariant_rows(docs: &str) -> Vec<InvariantRow> {
        let lines: Vec<&str> = docs.lines().collect();
        let mut rows = Vec::new();
        let mut i = 0;
        while i < lines.len() && !lines[i].contains("Enforcing module") {
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
            // cells = ["", "#", "invariant", "module", "check", ""]
            if cells.len() >= 5 {
                rows.push(InvariantRow {
                    number: cells[1].parse().unwrap_or(0),
                    enforcer: cells[3].to_string(),
                    check: cells[4].to_string(),
                });
            }
            i += 1;
        }
        rows
    }

    /// The document carries all 27 invariants, numbered 1..=27 in the same
    /// order as PLAN.md section 42, so an invariant dropped in transcription
    /// fails this test.
    #[test]
    fn invariants_document_lists_all_27_numbered_invariants() {
        let docs = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/INVARIANTS.md"))
            .expect("docs/INVARIANTS.md must be readable");
        let rows = parse_invariant_rows(&docs);
        assert_eq!(
            rows.len(),
            27,
            "expected 27 invariant rows, found {}",
            rows.len()
        );
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                row.number,
                i + 1,
                "invariant numbering must be 1..=27 in order"
            );
        }
    }

    /// Every invariant row names an enforcer (a module plus a test or check)
    /// or the explicit word `unenforced`. A row with neither fails this test.
    #[test]
    fn every_invariant_names_an_enforcer_or_unenforced() {
        let docs = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/INVARIANTS.md"))
            .expect("docs/INVARIANTS.md must be readable");
        let rows = parse_invariant_rows(&docs);
        for row in &rows {
            let names_enforcer = !row.enforcer.is_empty() && !row.check.is_empty();
            let marked_unenforced =
                row.enforcer.contains("unenforced") || row.check.contains("unenforced");
            assert!(
                names_enforcer || marked_unenforced,
                "invariant {} names neither an enforcer nor 'unenforced' (module={:?}, check={:?})",
                row.number,
                row.enforcer,
                row.check
            );
        }
    }
}
