//! Keeps the thin-binary rule mechanical instead of remembered: `main` holds no domain
//! arithmetic, so a line-count ceiling is a fair proxy for "still just parses, calls the
//! library, prints".

use std::fs;

#[test]
fn main_rs_stays_under_forty_lines() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs");
    let contents = fs::read_to_string(path).expect("src/main.rs must exist");
    let line_count = contents.lines().count();
    assert!(
        line_count < 40,
        "src/main.rs has {line_count} lines, expected under 40"
    );
}
