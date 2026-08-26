use std::process::Command;

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

fn json_string_field(line: &str, field: &str) -> String {
    let needle = format!("\"{field}\":\"");
    let remainder = line.split_once(&needle).expect("field must exist").1;
    remainder
        .split_once('"')
        .expect("field must terminate")
        .0
        .to_owned()
}

#[test]
fn fixture_keeps_report_on_stdout_and_typed_events_on_stderr_with_one_run() {
    let output = aub()
        .args(["-v", "__logging-fixture"])
        .output()
        .expect("fixture must run");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert_eq!(stderr.lines().count(), 2);
    assert!(stdout.contains("\"run\":\""));
    assert!(!stdout.contains("\"event\""));

    let lines: Vec<_> = stderr.lines().collect();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with('{') && line.ends_with('}'))
    );
    assert_eq!(json_string_field(lines[0], "event"), "run_started");
    assert_eq!(json_string_field(lines[1], "event"), "report_rendered");
    assert_eq!(json_string_field(lines[0], "level"), "info");
    assert_eq!(
        json_string_field(lines[0], "run"),
        json_string_field(lines[1], "run")
    );
    assert_eq!(
        json_string_field(lines[0], "run"),
        json_string_field(&stdout, "run")
    );
    for line in lines {
        assert!(line.contains("\"ts\":"));
    }
}

#[test]
fn status_is_quiet_by_default_and_logging_does_not_open_its_projection() {
    let default = aub().arg("status").output().expect("status must run");
    assert!(default.status.success());
    assert!(default.stderr.is_empty());

    let raised = aub()
        .args(["-v", "status"])
        .output()
        .expect("status must run");
    assert!(raised.status.success());
    let stderr = String::from_utf8(raised.stderr).unwrap();
    assert!(stderr.contains("\"event\":\"run_started\""));
    assert!(stderr.contains("\"command\":\"status\""));
}

#[test]
fn raised_status_logging_has_no_file_access_path() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli.rs"))
        .expect("CLI source must be readable");
    let status = source
        .split("fn status")
        .nth(1)
        .expect("status workflow must exist")
        .split("fn logging_fixture")
        .next()
        .expect("status workflow must end before fixture");
    for forbidden in ["std::fs", "File::", "read_to_string", "projection"] {
        assert!(
            !status.contains(forbidden),
            "status logging must not add file access through {forbidden}"
        );
    }
}
