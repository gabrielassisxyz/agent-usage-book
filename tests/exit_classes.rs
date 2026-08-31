//! Process-level exit-class tests: each class is exercised through a real
//! subprocess invocation of the `aub` binary, asserting the observed exit
//! status rather than a returned value.

use std::process::Command;

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

#[test]
fn exit_class_0_success() {
    let status = aub().status().expect("aub must run");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn exit_class_1_internal() {
    let status = aub().args(["__exit-class", "1"]).status().unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn exit_class_2_usage_via_unknown_flag() {
    let status = aub().arg("--definitely-not-a-flag").status().unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn exit_class_3_auth_required() {
    let status = aub().args(["__exit-class", "3"]).status().unwrap();
    assert_eq!(status.code(), Some(3));
}

#[test]
fn exit_class_4_remote_unavailable() {
    let status = aub().args(["__exit-class", "4"]).status().unwrap();
    assert_eq!(status.code(), Some(4));
}

#[test]
fn exit_class_5_store() {
    let status = aub().args(["__exit-class", "5"]).status().unwrap();
    assert_eq!(status.code(), Some(5));
}

#[test]
fn exit_class_6_insufficient_evidence() {
    let status = aub().args(["__exit-class", "6"]).status().unwrap();
    assert_eq!(status.code(), Some(6));
}

#[test]
fn exit_class_7_threshold_not_met() {
    let status = aub().args(["__exit-class", "7"]).status().unwrap();
    assert_eq!(status.code(), Some(7));
}

#[test]
fn exit_class_8_ingest_incomplete() {
    let status = aub().args(["__exit-class", "8"]).status().unwrap();
    assert_eq!(status.code(), Some(8));
}

/// The exit-code table the `__exit-class` hook is driven from is the upstream
/// section of the design itself (`docs/PLAN.md` section 40), so a row added
/// upstream changes the set this test runs instead of silently passing over a
/// hardcoded loop bound (aub-knw7).
fn documented_exit_codes() -> Vec<i32> {
    let plan = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/PLAN.md"))
        .expect("docs/PLAN.md must be readable");
    let start = plan
        .find("# 40. Exit codes and scripting contract")
        .expect("PLAN.md must contain the exit-codes section");
    let rest = &plan[start..];
    let end = rest[1..].find("\n# ").map(|i| i + 1).unwrap_or(rest.len());
    let section = &rest[..end];

    let codes: Vec<i32> = section
        .lines()
        .filter_map(|line| {
            let row = line.trim_start().strip_prefix("| ")?;
            let first_cell = row.split('|').next()?.trim();
            first_cell.parse::<i32>().ok()
        })
        .collect();
    assert!(
        codes.len() >= 9,
        "the exit-code table parse lost rows: only {codes:?}"
    );
    codes
}

#[test]
fn exit_class_hook_covers_every_class() {
    for class in documented_exit_codes() {
        let status = aub()
            .arg("__exit-class")
            .arg(class.to_string())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(class), "class {class}");
    }
}

#[test]
fn out_of_range_class_is_a_usage_error() {
    let status = aub().args(["__exit-class", "9"]).status().unwrap();
    assert_eq!(status.code(), Some(2));
}
