//! Guards the module skeleton's dependency direction: the three lowest layers must not
//! depend on SQLite, HTTP, or terminal-formatting crates, and the conversion-owning
//! modules must document typed witnesses without implementing them.

use std::fs;

/// The three lowest layers from PLAN.md section 8.1.
const LOWEST_LAYERS: [&str; 3] = ["domain", "evidence", "config"];

/// The conversion-owning modules from PLAN.md section 9.
const CONVERSION_MODULES: [&str; 3] = ["cost_model", "calibration", "valuation"];

/// The clock module is the only place the system clock may be read.
const CLOCK_MODULE: &str = "src/domain/time.rs";

/// System-clock call sites that must live only in the clock module.
const SYSTEM_CLOCK_PATTERNS: &[&str] = &[
    "SystemTime::now",
    "Instant::now",
    "UNIX_EPOCH",
    "std::time::SystemTime",
    "std::time::Instant",
];

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

/// True when the `[dependencies]` or `[target.<target>.dependencies]` table of `manifest`
/// declares the crate named `name`, or when `manifest` cannot be parsed as valid TOML (failing
/// closed so an unreadable manifest is never silently treated as clean).
///
/// `[dev-dependencies]` and `[build-dependencies]` are deliberately out of scope:
/// - dev-dependencies are test-only harnesses and are not compiled into the shipping library.
/// - build-dependencies are host build-script tools and do not enter the runtime crate.
fn declares_dependency(manifest: &str, name: &str) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(manifest) else {
        return true;
    };

    if let Some(deps) = table.get("dependencies").and_then(toml::Value::as_table)
        && deps.contains_key(name)
    {
        return true;
    }

    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            if let Some(deps) = target
                .as_table()
                .and_then(|t| t.get("dependencies"))
                .and_then(toml::Value::as_table)
                && deps.contains_key(name)
            {
                return true;
            }
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
/// line comment, regardless of any visibility or qualifier prefix on the declaration.
fn has_code_item(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            return false;
        }
        is_code_item_keyword(strip_leading_modifiers(trimmed))
    })
}

/// Strips leading visibility and function-qualifier tokens so the remainder begins at
/// the item keyword. `const` is deliberately not stripped: it is itself an item keyword
/// (`const X: T = ...`), so it is matched below rather than removed.
fn strip_leading_modifiers(mut line: &str) -> &str {
    loop {
        let Some(rest) = strip_one_modifier(line) else {
            return line;
        };
        line = rest;
    }
}

/// Removes one leading modifier token, returning the remainder, or `None` when the line
/// does not begin with a modifier.
fn strip_one_modifier(line: &str) -> Option<&str> {
    // `pub(in path)` is a single visibility token: consume through the closing paren
    // before bare `pub` can match its prefix.
    if let Some(rest) = line.strip_prefix("pub(in")
        && let Some(close) = rest.find(')')
    {
        return Some(rest[close + 1..].trim_start());
    }
    // `extern "abi"` carries an optional ABI string literal before the item keyword.
    if let Some(rest) = line.strip_prefix("extern")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(strip_abi_string(rest.trim_start()));
    }
    // Single-token modifiers, longest first so `pub(crate)` is not eaten as bare `pub`.
    for modifier in [
        "pub(crate)",
        "pub(super)",
        "pub(self)",
        "pub",
        "crate",
        "async",
        "unsafe",
        "default",
    ] {
        let Some(rest) = line.strip_prefix(modifier) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some(rest.trim_start());
        }
    }
    None
}

/// Consumes a leading `"abi"` string literal, when present, returning the remainder.
fn strip_abi_string(line: &str) -> &str {
    let Some(after_open) = line.strip_prefix('"') else {
        return line;
    };
    match after_open.find('"') {
        Some(close) => after_open[close + 1..].trim_start(),
        None => line,
    }
}

fn is_code_item_keyword(line: &str) -> bool {
    line.starts_with("fn ")
        || line.starts_with("static ")
        || line.starts_with("const ")
        || line.starts_with("impl ")
        || line.starts_with("impl<")
}

/// All `.rs` source files under a module, whether it is a single file
/// (`src/{name}.rs`) or a directory (`src/{name}/`).
fn module_source_files(name: &str) -> Vec<String> {
    let root = concat!(env!("CARGO_MANIFEST_DIR"));
    let file = format!("{root}/src/{name}.rs");
    if std::path::Path::new(&file).exists() {
        return vec![file];
    }
    let mut files = Vec::new();
    collect_rs_files(&format!("{root}/src/{name}"), &mut files);
    files
}

/// Recursively collects `.rs` files under `dir` into `out`.
fn collect_rs_files(dir: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path.to_string_lossy(), out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}

/// Crates the design assigns to a specific non-lowest layer, which the manifest
/// legitimately declares: rusqlite for the store layer (PLAN.md section 11) and
/// ureq for the meter layer (PLAN.md section 5). The manifest-level check below
/// permits exactly these and no other forbidden crate; the lowest layers are
/// still guarded by `lowest_layers_reference_no_forbidden_crate`.
const MANIFEST_ALLOWED_FORBIDDEN: &[&str] = &["rusqlite", "ureq"];

#[test]
fn manifest_declares_no_forbidden_crate() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("Cargo.toml must exist");
    for name in FORBIDDEN_CRATES {
        if MANIFEST_ALLOWED_FORBIDDEN.contains(name) {
            continue;
        }
        assert!(
            !declares_dependency(&manifest, name),
            "Cargo.toml declares a forbidden crate: {name}"
        );
    }
}

#[test]
fn lowest_layers_reference_no_forbidden_crate() {
    for layer in LOWEST_LAYERS {
        for path in module_source_files(layer) {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{path} must be readable: {e}"));
            for name in FORBIDDEN_CRATES {
                assert!(
                    !references_crate(&source, name),
                    "{path} references forbidden crate {name}"
                );
            }
        }
    }
}

#[test]
fn system_clock_calls_only_in_the_clock_module() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rs_files(&format!("{root}/src"), &mut files);
    for path in files {
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path} must be readable: {e}"));
        for pattern in SYSTEM_CLOCK_PATTERNS {
            assert!(
                !source.contains(pattern) || path.ends_with(CLOCK_MODULE),
                "{path} reads the system clock ({pattern}); only {CLOCK_MODULE} may"
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
    // Empty dependencies table.
    assert!(!declares_dependency("[dependencies]\n", "rusqlite"));

    // Inline dependency under [dependencies].
    assert!(declares_dependency(
        "[dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));

    // Sub-table dependency declaration under [dependencies.<name>].
    assert!(declares_dependency(
        "[dependencies.rusqlite]\nversion = \"0.31\"\n",
        "rusqlite"
    ));

    // A forbidden crate in dev-dependencies is out of scope for the library's tree.
    assert!(!declares_dependency(
        "[dev-dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(!declares_dependency(
        "[dev-dependencies.rusqlite]\nversion = \"0.31\"\n",
        "rusqlite"
    ));

    // A forbidden crate in build-dependencies is out of scope (host build tooling only).
    assert!(!declares_dependency(
        "[build-dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(!declares_dependency(
        "[build-dependencies.rusqlite]\nversion = \"0.31\"\n",
        "rusqlite"
    ));

    // Target-specific dependencies are in scope for library code.
    assert!(declares_dependency(
        "[target.'cfg(unix)'.dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(declares_dependency(
        "[target.'cfg(unix)'.dependencies.rusqlite]\nversion = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(!declares_dependency(
        "[target.'cfg(unix)'.dev-dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(!declares_dependency(
        "[target.'cfg(unix)'.dev-dependencies.rusqlite]\nversion = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(!declares_dependency(
        "[target.'cfg(unix)'.build-dependencies]\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(!declares_dependency(
        "[target.'cfg(unix)'.build-dependencies.rusqlite]\nversion = \"0.31\"\n",
        "rusqlite"
    ));

    // Malformed manifest fails closed.
    assert!(declares_dependency(
        "[dependencies\nrusqlite = \"0.31\"\n",
        "rusqlite"
    ));
    assert!(declares_dependency("not valid toml [[[", "rusqlite"));
}

#[test]
fn code_item_check_rejects_a_planted_implementation() {
    // A comment-only module is clean.
    assert!(!has_code_item("//! header only\n"));

    // Bare forms.
    assert!(has_code_item("fn convert() {}\n"));
    assert!(has_code_item("static WITNESS: u64 = 1;\n"));
    assert!(has_code_item("const WITNESS: u64 = 1;\n"));
    assert!(has_code_item("impl CostModel {}\n"));

    // Visibility prefixes, which a usable conversion must carry.
    assert!(has_code_item("pub fn planted() {}\n"));
    assert!(has_code_item("pub(crate) fn planted() {}\n"));
    assert!(has_code_item("pub(super) fn planted() {}\n"));
    assert!(has_code_item("pub const WITNESS: u64 = 1;\n"));
    assert!(has_code_item("pub static WITNESS: u64 = 1;\n"));

    // Function qualifiers.
    assert!(has_code_item("async fn planted() {}\n"));
    assert!(has_code_item("unsafe fn planted() {}\n"));
    assert!(has_code_item("const fn planted() {}\n"));
    assert!(has_code_item("unsafe impl CostModel {}\n"));
    assert!(has_code_item("extern \"C\" fn planted() {}\n"));

    // A type definition is not a conversion implementation or a global witness.
    assert!(!has_code_item("pub struct CostModel;\n"));
}

#[test]
fn a_public_item_planted_in_a_real_conversion_module_is_detected() {
    // The real check reads the module file from disk and feeds its whole contents to
    // has_code_item; feed that same file with a public conversion planted, to prove the
    // wiring catches a violation in a real module rather than only in a string literal.
    let path = format!("{}/src/cost_model.rs", env!("CARGO_MANIFEST_DIR"));
    let real = fs::read_to_string(&path).expect("src/cost_model.rs must exist");
    let planted = format!("{real}\npub fn planted_conversion() {{}}\n");
    assert!(
        has_code_item(&planted),
        "a public conversion planted into src/cost_model.rs must be detected"
    );
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
