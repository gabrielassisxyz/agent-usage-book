//! One ledger for LLM consumption: token spend read from local agent transcripts, joined
//! with quota measured at the provider's own endpoints.

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

    /// The 20 modules from the design's module table (PLAN.md section 8).
    const DESIGN_MODULES: [&str; 20] = [
        "domain",
        "evidence",
        "config",
        "store",
        "meter",
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

            let path = format!("{}/src/{name}.rs", env!("CARGO_MANIFEST_DIR"));
            let source = fs::read_to_string(&path)
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
}
