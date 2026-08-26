//! Guards the module skeleton's dependency direction: the three lowest layers must not
//! depend on SQLite, HTTP, or terminal-formatting crates, and the conversion-owning
//! modules must document typed witnesses without implementing them.

use std::fs;

/// The three lowest layers from PLAN.md section 8.1.
const LOWEST_LAYERS: [&str; 3] = ["domain", "evidence", "config"];

/// The conversion-owning modules from PLAN.md section 9.
const CONVERSION_MODULES: [&str; 3] = ["cost_model", "calibration", "valuation"];

/// Crate families the lowest layers must not depend on (PLAN.md section 8.1).
const FORBIDDEN_CRATES: &[&str] = &[
    // SQLite
    "rusqlite",
    "libsqlite3-sys",
    "sqlx",
    "diesel",
    "sqlite",
    // HTTP
    "ureq",
    "reqwest",
    "http",
    "hyper",
    "curl",
    "isahc",
    "attohttpc",
    // terminal formatting
    "crossterm",
    "ratatui",
    "termion",
    "ansi_term",
    "anstyle",
    "colored",
    "console",
    "owo-colors",
    "yansi",
    "termcolor",
];

/// True when the `[dependencies]` table of `manifest` declares the crate named `name`.
fn declares_dependency(manifest: &str, name: &str) -> bool {
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]";
            continue;
        }
        if !in_dependencies || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((dep, _)) = trimmed.split_once('=')
            && dep.trim() == name
        {
            return true;
        }
    }
    false
}

/// True when `source` references the crate named `name` outside a line comment.
fn references_crate(source: &str, name: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            return false;
        }
        trimmed.contains(&format!("{name}::"))
            || trimmed.starts_with(&format!("extern crate {name}"))
    })
}

/// True when `source` declares a code item (function, impl, static, or const) outside a
/// line comment.
fn has_code_item(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            return false;
        }
        ["fn ", "impl ", "static ", "const "]
            .iter()
            .any(|kw| trimmed.starts_with(kw))
    })
}

#[test]
fn manifest_declares_no_forbidden_crate() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("Cargo.toml must exist");
    for name in FORBIDDEN_CRATES {
        assert!(
            !declares_dependency(&manifest, name),
            "Cargo.toml declares a forbidden crate: {name}"
        );
    }
}

#[test]
fn lowest_layers_reference_no_forbidden_crate() {
    for layer in LOWEST_LAYERS {
        let path = format!("{}/src/{layer}.rs", env!("CARGO_MANIFEST_DIR"));
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("src/{layer}.rs must exist: {e}"));
        for name in FORBIDDEN_CRATES {
            assert!(
                !references_crate(&source, name),
                "src/{layer}.rs references forbidden crate {name}"
            );
        }
    }
}

#[test]
fn audit_rejects_a_planted_forbidden_dependency() {
    assert!(!references_crate(
        "//! header\n\npub fn ok() {}\n",
        "rusqlite"
    ));
    assert!(references_crate("use rusqlite::Connection;\n", "rusqlite"));
    assert!(references_crate("use ureq::get;\n", "ureq"));
    assert!(references_crate("use crossterm::style;\n", "crossterm"));
    assert!(references_crate("extern crate reqwest;\n", "reqwest"));
}

#[test]
fn dependency_check_rejects_a_planted_forbidden_dependency() {
    assert!(!declares_dependency("[dependencies]\n", "rusqlite"));
    assert!(declares_dependency(
        "[dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    // A forbidden crate in dev-dependencies is out of scope for the library's tree.
    assert!(!declares_dependency(
        "[dependencies]\n\n[dev-dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
}

#[test]
fn code_item_check_rejects_a_planted_implementation() {
    assert!(!has_code_item("//! header only\n"));
    assert!(has_code_item("fn convert() {}\n"));
    assert!(has_code_item("static WITNESS: u64 = 1;\n"));
    assert!(has_code_item("const WITNESS: u64 = 1;\n"));
    assert!(has_code_item("impl CostModel {}\n"));
}

#[test]
fn conversion_module_headers_document_typed_witnesses() {
    for module in CONVERSION_MODULES {
        let path = format!("{}/src/{module}.rs", env!("CARGO_MANIFEST_DIR"));
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("src/{module}.rs must exist: {e}"));
        assert!(
            source.contains("requires a typed"),
            "src/{module}.rs header does not document that conversions require a typed witness"
        );
        assert!(
            source.contains("owned by this module"),
            "src/{module}.rs header does not identify the owning module"
        );
    }
}

#[test]
fn conversion_modules_introduce_no_implementation_or_global_witness() {
    for module in CONVERSION_MODULES {
        let path = format!("{}/src/{module}.rs", env!("CARGO_MANIFEST_DIR"));
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("src/{module}.rs must exist: {e}"));
        assert!(
            !has_code_item(&source),
            "src/{module}.rs introduces a conversion implementation or global witness"
        );
    }
}
