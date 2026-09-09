//! End-to-end proof for `aub-w4rv`: a status-line observation that is newer
//! than a full one does not remove the model-scoped row from the rendered
//! `aub status` surface.
//!
//! This is the counterpart to
//! `window_anomaly_e2e::a_disappearing_model_specific_window_persists_its_typed_classification`,
//! which asserts only against the SQLite table. Here the release binary is
//! driven through the whole pipeline the operator saw fail: one `aub sample`
//! against a synthetic endpoint that reports a model-scoped window, the real
//! `aub statusline` tee writing a window-subset record, a second `aub sample`
//! that reads that record instead of the endpoint, and `aub status`, whose
//! stdout and JSON must still carry the model row.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

/// A full Anthropic usage body: the two required account-wide windows plus a
/// model-scoped `seven_day_sonnet`, the window the status-line payload never
/// carries. Resets are far in the future so every window renders a row.
const FULL_BODY: &str = concat!(
    r#"{"five_hour":{"utilization":50.0,"resets_at":"2027-01-01T00:00:00Z"},"#,
    r#""seven_day":{"utilization":20.0,"resets_at":"2027-01-01T00:00:00Z"},"#,
    r#""seven_day_sonnet":{"utilization":80.0,"resets_at":"2027-02-01T00:00:00Z"}}"#
);

/// The canonical status-line payload: two windows, no model-scoped one. The
/// tee stamps its own receive instant, so the record is fresh when the second
/// sample reads it.
const STATUSLINE_PAYLOAD: &[u8] = include_bytes!("fixtures/statusline/payload-five-seven.json");

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "aub-status-line-subset-e2e-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("creds")).unwrap();
        std::fs::write(
            root.join("creds/token.json"),
            r#"{"accessToken":"test-token"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-primary\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"{}\" }}\n",
                root.join("state").display(),
                root.join("creds/token.json").display(),
            ),
        )
        .unwrap();
        Self { root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("state").join("ledger.db")
    }

    fn record_path(&self) -> PathBuf {
        self.root
            .join("state")
            .join("statusline")
            .join("work-primary.jsonl")
    }

    fn base_env(&self, command: &mut Command) {
        command
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .env("AUB_STATE_DIR", self.root.join("state"));
    }

    fn sample(&self, server_url: &str) -> (i32, String) {
        let mut command = aub();
        self.base_env(&mut command);
        command.env("AUB_ANTHROPIC_ENDPOINT", server_url).args([
            "sample",
            "--account",
            "work-primary",
            "--require-success",
        ]);
        let output = command.output().expect("aub sample must run");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// Runs the real `aub statusline` tee with the payload on stdin, attributed
    /// to `work-primary` through `SHALLOW_PROFILE`, so the record file is
    /// written the way the installed status line writes it.
    fn tee_statusline(&self, payload: &[u8]) -> i32 {
        let mut command = aub();
        self.base_env(&mut command);
        let mut child = command
            .arg("statusline")
            .env("SHALLOW_PROFILE", "work-primary")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("aub statusline must spawn");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(payload)
            .expect("the payload must reach the tee");
        child
            .wait()
            .expect("aub statusline must finish")
            .code()
            .unwrap_or(-1)
    }

    fn status(&self, extra_args: &[&str]) -> (i32, String) {
        let mut command = aub();
        self.base_env(&mut command);
        command.arg("status").args(extra_args);
        let output = command.output().expect("aub status must run");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
        )
    }
}

#[test]
fn a_newer_status_line_subset_keeps_the_model_row_in_aub_status() {
    let env = Environment::new("keeps-model-row");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(FULL_BODY.as_bytes().to_vec()),
    )])
    .expect("synthetic server must start");

    // 1. The full observation: the endpoint reports the model-scoped window.
    let (status, stderr) = env.sample(&server.url());
    assert_eq!(status, 0, "the full sample must succeed; stderr: {stderr}");

    // 2. The status-line tee writes a window-subset record for the same account.
    assert_eq!(
        env.tee_statusline(STATUSLINE_PAYLOAD),
        0,
        "the tee never fails"
    );
    assert!(
        env.record_path().exists(),
        "the tee must have written the record file"
    );

    // 3. The second sample reads the fresh record, not the endpoint: the
    //    synthetic server's script has one response and it was already spent,
    //    so a fallback to the endpoint would fail --require-success here.
    let (status, stderr) = env.sample(&server.url());
    assert_eq!(
        status, 0,
        "the status-line sample must succeed off the record; stderr: {stderr}"
    );
    assert_eq!(
        server.request_count(),
        1,
        "the second sample must not have called the endpoint"
    );

    // The ledger holds one observation per source, the status-line one newest.
    let conn = rusqlite::Connection::open(env.db_path()).expect("ledger must open");
    let contracts: Vec<String> = conn
        .prepare("SELECT provider_contract_id FROM meter_observation ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        contracts,
        vec![
            "anthropic-oauth-usage-v1".to_string(),
            "anthropic-statusline-rate-limits-v1".to_string(),
        ],
        "the status-line observation is the newest one recorded"
    );

    // 4. The rendered surface: the model row survived the newer subset.
    let (status, stdout) = env.status(&[]);
    assert_eq!(status, 0, "aub status must render");
    let model_row = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("sonnet"));
    assert!(
        model_row.is_some(),
        "aub status must still render the sonnet row: {stdout}"
    );

    let (status, json) = env.status(&["--format", "json"]);
    assert_eq!(status, 0, "aub status --format json must render");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("status json parses");
    let scopes: Vec<String> = parsed["accounts"][0]["included_scopes"]
        .as_array()
        .expect("included_scopes is an array")
        .iter()
        .map(|scope| scope.as_str().unwrap().to_string())
        .collect();
    assert!(
        scopes.iter().any(|scope| scope == "model:sonnet"),
        "included_scopes still names the model scope: {scopes:?}"
    );
}
