//! The style layer's width, measured live: the real [`Style::detect`] path
//! against a real pty of 120 columns and against a real pipe, the two
//! environments the resolution rule has to tell apart.
//!
//! The scenarios re-execute this test binary as a child: `cargo test` itself
//! always gives a test process a pipe, so a pty can only be observed by
//! running the probe under one. The child role is the same test body, selected
//! by an environment variable, printing the width it resolved on whatever
//! stdout it was given.

use std::process::{Command, Stdio};

/// The test name the scenario tests re-execute this binary with, so the child
/// runs exactly one test.
const PROBE_TEST: &str = "style_width_reports_the_width_of_its_own_stdout";

/// The marker the child prints its resolved width behind.
const WIDTH_MARKER: &str = "AUB_WIDTH=";

fn this_test_binary() -> std::path::PathBuf {
    std::env::current_exe().expect("the running test binary's own path")
}

/// The arguments that make a re-executed test binary run only the probe.
fn probe_args() -> [String; 3] {
    [
        PROBE_TEST.to_string(),
        "--exact".to_string(),
        "--nocapture".to_string(),
    ]
}

/// The environment the child runs with: the role marker, and no `NO_COLOR` or
/// `COLUMNS`, so the detection path sees the environment the scenarios intend.
fn probe_command() -> Command {
    let mut command = Command::new(this_test_binary());
    command
        .args(probe_args())
        .env("AUB_STYLE_WIDTH_PROBE", "1")
        .env_remove("NO_COLOR")
        .env_remove("COLUMNS");
    command
}

/// The width the child reported behind the marker, read from whichever stream
/// the scenario named.
fn reported_width(output: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(output);
    text.lines()
        .find_map(|line| line.trim().strip_prefix(WIDTH_MARKER))
        .and_then(|width| width.trim().parse::<u16>().ok())
}

#[test]
fn style_width_reports_the_width_of_its_own_stdout() {
    // The child role: one measurement, through the same detection path the
    // status command runs, on whatever stdout this process was handed. The
    // parent role has nothing to observe; the two scenario tests below are
    // the ones that drive this test as a child.
    if std::env::var("AUB_STYLE_WIDTH_PROBE").is_ok() {
        let width = agent_usage_book::presentation::Style::detect(false).width();
        println!("{WIDTH_MARKER}{width}");
    }
}

#[test]
fn style_width_resolves_the_pty_columns_under_a_pty_of_120() {
    if std::env::var("AUB_STYLE_WIDTH_PROBE").is_ok() {
        unreachable!("the pty scenario never runs in the child role");
    }

    let script = Command::new("script")
        .arg("--version")
        .output()
        .expect("script must be runnable: the pty scenario drives the probe through one");
    assert!(
        script.status.success(),
        "script must be available for the pty scenario; without it this criterion is not guardable here"
    );

    let output = Command::new("script")
        .args(["-q", "-e", "-c"])
        .arg(format!(
            "stty cols 120; exec '{}' {}",
            this_test_binary().display(),
            probe_args().join(" ")
        ))
        .arg("/dev/null")
        .env("AUB_STYLE_WIDTH_PROBE", "1")
        .env_remove("NO_COLOR")
        .env_remove("COLUMNS")
        .output()
        .expect("the pty scenario must be able to run the probe under script");

    let width = reported_width(&output.stdout).unwrap_or_else(|| {
        panic!(
            "the probe under a pty of 120 columns reported no width; exit {:?}, stdout {:?}, stderr {:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    assert_eq!(
        width, 120,
        "Style::width() under a pty of 120 columns must be the pty's column count"
    );
}

#[test]
fn style_width_defaults_to_80_when_stdout_is_a_pipe() {
    if std::env::var("AUB_STYLE_WIDTH_PROBE").is_ok() {
        unreachable!("the pipe scenario never runs in the child role");
    }

    let output = probe_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the pipe scenario must be able to run the probe as a child");

    let width = reported_width(&output.stdout).unwrap_or_else(|| {
        panic!(
            "the probe under a pipe reported no width; exit {:?}, stdout {:?}, stderr {:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    });
    assert_eq!(
        width, 80,
        "Style::width() when stdout is a pipe must be the default width"
    );
}
