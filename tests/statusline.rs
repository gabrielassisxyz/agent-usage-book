//! The `aub statusline` verb, end to end against the release binary.
//!
//! The tee's contract is unusual and the unit tests in `src/statusline.rs`
//! cannot carry it alone: the status line pipeline runs the real binary with
//! the payload on stdin and the renderer on the other end of the pipe, so
//! what has to hold is that the bytes that go in come out unchanged, the
//! exit code is 0 in every failure case on aub's own side, and the record
//! file gets exactly the line the shape decision promises. Every test here
//! runs the binary the test harness built, under a scratch state directory
//! and a synthetic home, the way the e2e suite isolates state.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// The canonical status-line payload fixture: two windows, a cost field the
/// record must never carry, and epoch-string `resets_at`.
const PAYLOAD_FIVE_SEVEN: &[u8] = include_bytes!("fixtures/statusline/payload-five-seven.json");
/// The same session with `seven_day.used_percentage` moved.
const PAYLOAD_SEVEN_DAY_MOVED: &[u8] =
    include_bytes!("fixtures/statusline/payload-seven-day-moved.json");
/// A payload carrying a model-scoped window the shape rule admits.
const PAYLOAD_EXTRA_WINDOW: &[u8] = include_bytes!("fixtures/statusline/payload-extra-window.json");
/// Payload material that is not JSON at all.
const PAYLOAD_MALFORMED: &[u8] = b"{not json at all";

/// A config naming one anthropic account (`gmail`), the way the operator's
/// file names theirs.
fn config_toml(state_dir: &Path) -> String {
    format!(
        "[state]\ndir = {}\n\n[[accounts]]\nname = \"gmail\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = {}}}\n",
        serde_json::to_string(state_dir.to_str().unwrap()).unwrap(),
        serde_json::to_string(
            &state_dir
                .join("credential.json")
                .to_str()
                .unwrap()
                .to_string()
        )
        .unwrap(),
    )
}

/// Runs the built `aub statusline` with `payload` on stdin and `envs` layered
/// onto a cleaned environment, so a variable the test process happens to
/// carry cannot decide what the command does. Returns the raw output.
fn run_statusline(payload: &[u8], envs: &[(&str, String)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_aub"));
    command.arg("statusline").env_clear();
    for (name, value) in envs {
        command.env(name, value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("aub binary must spawn");
    child
        .stdin
        .take()
        .expect("stdin must be piped")
        .write_all(payload)
        .expect("the payload must reach the tee's stdin");
    child
        .wait_with_output()
        .expect("aub must run to completion")
}

/// The scratch state directory and config file one test runs against.
struct Scratch {
    state: test_support::StateDir,
}

impl Scratch {
    fn new() -> Self {
        let state = test_support::StateDir::new();
        std::fs::write(state.path().join("aub.toml"), config_toml(state.path()))
            .expect("scratch config must write");
        Self { state }
    }

    fn config_path(&self) -> String {
        self.state
            .path()
            .join("aub.toml")
            .to_str()
            .unwrap()
            .to_string()
    }

    fn state_dir(&self) -> String {
        self.state.path().to_str().unwrap().to_string()
    }

    /// The env every status-line invocation needs: the scratch home, the
    /// scratch config, the scratch state directory. `profile` adds
    /// `SHALLOW_PROFILE`; `None` leaves the variable unset.
    fn env(&self, profile: Option<&str>) -> Vec<(&'static str, String)> {
        let mut envs = vec![
            ("HOME", self.state.path().to_str().unwrap().to_string()),
            ("AUB_CONFIG_FILE", self.config_path()),
            ("AUB_STATE_DIR", self.state_dir()),
        ];
        if let Some(profile) = profile {
            envs.push(("SHALLOW_PROFILE", profile.to_string()));
        }
        envs
    }

    fn record_lines(&self, account: &str) -> Vec<String> {
        let path = self
            .state
            .path()
            .join("statusline")
            .join(format!("{account}.jsonl"));
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn a_profiled_payload_passes_through_and_is_recorded_once() {
    let scratch = Scratch::new();

    let output = run_statusline(PAYLOAD_FIVE_SEVEN, &scratch.env(Some("gmail")));
    assert_eq!(output.status.code(), Some(0), "the tee never fails");
    assert_eq!(
        output.stdout, PAYLOAD_FIVE_SEVEN,
        "the payload passes through byte-for-byte"
    );

    let lines = scratch.record_lines("gmail");
    assert_eq!(lines.len(), 1, "one render with a moved meter: one line");
    let line: serde_json::Value =
        serde_json::from_str(&lines[0]).expect("the record is one JSON object");
    assert_eq!(
        line["session_id"],
        serde_json::json!("8cd9c60a-e10a-4d44-857e-6b2b931b4d9d")
    );
    assert_eq!(line["cwd"], serde_json::json!("/tmp/worktree/project"));
    assert!(
        line.get("received_at")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|received_at| received_at.len() == 20 && received_at.ends_with('Z')),
        "received_at is an RFC 3339 UTC instant: {:?}",
        line.get("received_at")
    );
    let five = &line["windows"]["five_hour"];
    assert_eq!(five["used_percentage"], serde_json::json!(40));
    assert_eq!(
        five["resets_at"],
        serde_json::json!(1_786_834_200),
        "resets_at lands as an integer epoch second"
    );
    assert_eq!(
        line["windows"]["seven_day"]["used_percentage"],
        serde_json::json!(12)
    );
}

#[test]
fn an_identical_rerender_appends_nothing_and_a_moved_meter_appends_one_line() {
    let scratch = Scratch::new();

    run_statusline(PAYLOAD_FIVE_SEVEN, &scratch.env(Some("gmail")));
    run_statusline(PAYLOAD_FIVE_SEVEN, &scratch.env(Some("gmail")));
    assert_eq!(
        scratch.record_lines("gmail").len(),
        1,
        "the same meter state for the same session renders many times and records once"
    );

    run_statusline(PAYLOAD_SEVEN_DAY_MOVED, &scratch.env(Some("gmail")));
    let lines = scratch.record_lines("gmail");
    assert_eq!(lines.len(), 2, "a moved window earns exactly one more line");
    let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(
        second["windows"]["seven_day"]["used_percentage"],
        serde_json::json!(13)
    );

    // And the new state holds too.
    run_statusline(PAYLOAD_SEVEN_DAY_MOVED, &scratch.env(Some("gmail")));
    assert_eq!(scratch.record_lines("gmail").len(), 2);
}

#[test]
fn no_profile_and_an_unknown_profile_pass_through_without_a_file() {
    let scratch = Scratch::new();

    // No SHALLOW_PROFILE at all: env without the variable.
    let output = run_statusline(PAYLOAD_FIVE_SEVEN, &scratch.env(None));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, PAYLOAD_FIVE_SEVEN, "still passes through");
    assert!(
        !scratch.state.path().join("statusline").exists(),
        "no record directory is created without a profile"
    );

    // A profile no configured account answers to.
    let output = run_statusline(PAYLOAD_FIVE_SEVEN, &scratch.env(Some("nobody")));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, PAYLOAD_FIVE_SEVEN);
    assert!(
        !scratch.state.path().join("statusline").exists(),
        "an unknown profile writes nothing"
    );
}

#[test]
fn malformed_json_passes_through_and_writes_nothing() {
    let scratch = Scratch::new();

    let output = run_statusline(PAYLOAD_MALFORMED, &scratch.env(Some("gmail")));
    assert_eq!(
        output.status.code(),
        Some(0),
        "even a malformed payload exits 0"
    );
    assert_eq!(
        output.stdout, PAYLOAD_MALFORMED,
        "the raw bytes pass through unchanged"
    );
    assert!(
        !scratch.state.path().join("statusline").exists(),
        "a malformed payload records nothing"
    );
}

#[test]
fn a_missing_config_file_passes_through_and_writes_nothing() {
    let state = test_support::StateDir::new();
    let envs = vec![
        ("HOME", state.path().to_str().unwrap().to_string()),
        // A config path that does not exist: resolution falls back to
        // defaults, which name no accounts, so nothing can be attributed.
        (
            "AUB_CONFIG_FILE",
            state
                .path()
                .join("absent.toml")
                .to_str()
                .unwrap()
                .to_string(),
        ),
        ("SHALLOW_PROFILE", "gmail".to_string()),
    ];

    let output = run_statusline(PAYLOAD_FIVE_SEVEN, &envs);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a missing config never fails the bar"
    );
    assert_eq!(output.stdout, PAYLOAD_FIVE_SEVEN);
    assert!(
        !state.path().join(".local/state/aub/statusline").exists(),
        "no record lands under the default state directory either"
    );
    assert!(
        !state.path().join(".local/state/aub").exists(),
        "a missing config must not even begin creating a state tree"
    );
}

#[test]
fn an_unwritable_state_directory_passes_through_and_writes_nothing() {
    let state = test_support::StateDir::new();
    // The would-be state directory's parent is made untraversable, so no
    // child directory can be created under it: the same shape the e2e
    // isolation case uses, because a leaf-only restriction would be repaired
    // by the tee's own directory creation.
    let blocked = state.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(
        state.path().join("aub.toml"),
        config_toml(&blocked.join("aub")),
    )
    .unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let envs = vec![
        ("HOME", state.path().to_str().unwrap().to_string()),
        (
            "AUB_CONFIG_FILE",
            state.path().join("aub.toml").to_str().unwrap().to_string(),
        ),
        (
            "AUB_STATE_DIR",
            blocked.join("aub").to_str().unwrap().to_string(),
        ),
        ("SHALLOW_PROFILE", "gmail".to_string()),
    ];

    let output = run_statusline(PAYLOAD_FIVE_SEVEN, &envs);
    assert_eq!(
        output.status.code(),
        Some(0),
        "an unwritable state dir never fails the bar"
    );
    assert_eq!(output.stdout, PAYLOAD_FIVE_SEVEN);

    // Restore removability before the scratch directory is dropped.
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !blocked.join("aub").exists(),
        "nothing is created under an unwritable state directory"
    );
}

#[test]
fn a_window_of_any_name_is_recorded_under_its_own_name() {
    let scratch = Scratch::new();

    let output = run_statusline(PAYLOAD_EXTRA_WINDOW, &scratch.env(Some("gmail")));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, PAYLOAD_EXTRA_WINDOW);

    let lines = scratch.record_lines("gmail");
    assert_eq!(lines.len(), 1);
    let line: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    let opus = &line["windows"]["seven_day_opus"];
    assert_eq!(opus["used_percentage"], serde_json::json!(9));
    assert_eq!(opus["resets_at"], serde_json::json!(1_786_920_001));
}

#[test]
fn the_recorded_line_never_carries_the_payloads_other_fields() {
    let scratch = Scratch::new();

    run_statusline(PAYLOAD_FIVE_SEVEN, &scratch.env(Some("gmail")));
    let line = &scratch.record_lines("gmail")[0];
    // The fixture's cost value, which the payload carries and the record
    // must not: the value grep catches a field copied wholesale, which a
    // key-set comparison would let through.
    assert!(
        !line.contains("12.345678"),
        "the payload's cost value leaked into the record: {line}"
    );
    assert!(
        !line.contains("cost"),
        "the cost key leaked into the record: {line}"
    );
}

#[test]
fn statusline_refuses_positional_arguments_like_every_other_command() {
    // The pipeline never passes arguments; an operator who does gets the
    // ordinary usage error rather than a silently ignored one.
    let mut command = Command::new(env!("CARGO_BIN_EXE_aub"));
    command
        .args(["statusline", "unexpected"])
        .env("HOME", "/nonexistent")
        .stdin(Stdio::null());
    let output = command.output().expect("aub binary must run");
    assert_ne!(
        output.status.code(),
        Some(0),
        "a positional argument is a usage error"
    );
}
