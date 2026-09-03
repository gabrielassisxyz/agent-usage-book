//! Integration tests for `aub sample` exit-status semantics (aub-eun.6).
//!
//! Covers:
//! - Scheduled `--due` exits zero when every attempt outcome was durably recorded
//!   (success, auth failure, transport failure).
//! - Local persistence viability failure exits non-zero with ExitClass::Store (5)
//!   before any network request is issued.
//! - `--require-success` records the evidence first, then exits with AuthRequired (3)
//!   or RemoteUnavailable (4).
//! - `--require-success` exits 0 on success.
//! - Invalid flag combinations exit with ExitClass::Usage (2).

use std::path::PathBuf;
use std::process::Command;

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

const ANTHROPIC_SUCCESS_BODY: &[u8] = br#"{
    "five_hour": { "utilization": 0.25, "resets_at": "2026-08-30T16:00:00Z" },
    "seven_day": { "utilization": 0.50, "resets_at": "2026-09-06T00:00:00Z" }
}"#;

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-sample-exit-{tag}-{}", std::process::id()));
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

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn db_path(&self) -> PathBuf {
        self.state_dir().join("ledger.db")
    }

    fn command(&self, server_url: &str, args: &[&str]) -> Command {
        let mut command = aub();
        command
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .env("AUB_ANTHROPIC_ENDPOINT", server_url)
            .args(args);
        command
    }

    fn run(&self, server_url: &str, args: &[&str]) -> (i32, String, String) {
        let output = self
            .command(server_url, args)
            .output()
            .expect("aub must run");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    fn query_attempts_and_results(&self) -> (i64, i64) {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open db");
        let attempts: i64 = conn
            .query_row("SELECT count(*) FROM meter_attempt", [], |r| r.get(0))
            .unwrap_or(0);
        let results: i64 = conn
            .query_row("SELECT count(*) FROM meter_attempt_result", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        (attempts, results)
    }

    fn query_latest_result_outcome(&self) -> Option<String> {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open db");
        conn.query_row(
            "SELECT outcome FROM meter_attempt_result ORDER BY attempt_id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok()
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn scheduled_due_exits_zero_on_recorded_success() {
    let env = Environment::new("due-success");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();

    let (status, stdout, stderr) = env.run(&server.url(), &["sample", "--due"]);
    assert_eq!(status, 0, "stderr was: {stderr}");
    assert!(stdout.contains("sample: account=work-primary outcome=success attempt=1"));

    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(attempts, 1, "attempt start row must be persisted");
    assert_eq!(results, 1, "attempt result row must be persisted");
    assert_eq!(
        env.query_latest_result_outcome().as_deref(),
        Some("success")
    );
}

#[test]
fn scheduled_due_exits_zero_on_recorded_auth_failure() {
    let env = Environment::new("due-auth-fail");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();

    let (status, stdout, stderr) = env.run(&server.url(), &["sample", "--due"]);
    assert_eq!(
        status, 0,
        "scheduled --due exits 0 on recorded auth failure; stderr: {stderr}"
    );
    assert!(stdout.contains("sample: account=work-primary outcome=auth_required attempt=1"));

    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(attempts, 1, "attempt start row must be persisted");
    assert_eq!(results, 1, "attempt result row must be persisted");
    assert_eq!(
        env.query_latest_result_outcome().as_deref(),
        Some("auth_required")
    );
}

#[test]
fn scheduled_due_exits_zero_on_recorded_transport_failure() {
    let env = Environment::new("due-transport-fail");
    let server = SyntheticServer::start(vec![ScriptedOutcome::InternalServerError500]).unwrap();

    let (status, stdout, stderr) = env.run(&server.url(), &["sample", "--due"]);
    assert_eq!(
        status, 0,
        "scheduled --due exits 0 on recorded transport failure; stderr: {stderr}"
    );
    assert!(stdout.contains("sample: account=work-primary outcome=unreachable attempt=1"));

    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(attempts, 1, "attempt start row must be persisted");
    assert_eq!(results, 1, "attempt result row must be persisted");
    assert_eq!(
        env.query_latest_result_outcome().as_deref(),
        Some("unreachable")
    );
}

#[test]
fn persistence_viability_failure_exits_store_class_5_without_request() {
    let env = Environment::new("store-unviable");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();

    // Replace state dir with a symlink (rejected by ensure_state_dir_ready)
    let real_dir = env.root.join("real_state");
    std::fs::create_dir_all(&real_dir).unwrap();
    let _ = std::fs::remove_dir_all(env.state_dir());
    std::os::unix::fs::symlink(&real_dir, env.state_dir()).unwrap();

    let (status, _stdout, stderr) = env.run(&server.url(), &["sample", "--due"]);
    assert_eq!(
        status, 5,
        "must exit with ExitClass::Store (5); stderr: {stderr}"
    );
    assert!(stderr.contains("symlink"));
    assert_eq!(server.request_count(), 0, "no network request must be made");
}

#[test]
fn require_success_exits_auth_required_class_3_after_recording_evidence() {
    let env = Environment::new("require-success-auth");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Unauthorized401]).unwrap();

    let (status, stdout, stderr) =
        env.run(&server.url(), &["sample", "--due", "--require-success"]);
    assert_eq!(
        status, 3,
        "must exit with ExitClass::AuthRequired (3); stderr: {stderr}"
    );
    assert!(stdout.contains("sample: account=work-primary outcome=auth_required attempt=1"));

    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(attempts, 1, "attempt start row must be persisted");
    assert_eq!(results, 1, "attempt result row must be persisted");
    assert_eq!(
        env.query_latest_result_outcome().as_deref(),
        Some("auth_required")
    );
}

#[test]
fn require_success_exits_remote_unavailable_class_4_after_recording_evidence() {
    let env = Environment::new("require-success-unreach");
    let server = SyntheticServer::start(vec![ScriptedOutcome::InternalServerError500]).unwrap();

    let (status, stdout, stderr) =
        env.run(&server.url(), &["sample", "--due", "--require-success"]);
    assert_eq!(
        status, 4,
        "must exit with ExitClass::RemoteUnavailable (4); stderr: {stderr}"
    );
    assert!(stdout.contains("sample: account=work-primary outcome=unreachable attempt=1"));

    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(attempts, 1, "attempt start row must be persisted");
    assert_eq!(results, 1, "attempt result row must be persisted");
    assert_eq!(
        env.query_latest_result_outcome().as_deref(),
        Some("unreachable")
    );
}

#[test]
fn require_success_exits_zero_on_success() {
    let env = Environment::new("require-success-ok");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();

    let (status, stdout, stderr) =
        env.run(&server.url(), &["sample", "--due", "--require-success"]);
    assert_eq!(status, 0, "must exit 0 on success; stderr: {stderr}");
    assert!(stdout.contains("sample: account=work-primary outcome=success attempt=1"));

    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(attempts, 1);
    assert_eq!(results, 1);
}

#[test]
fn usage_flag_validation_errors_exit_2() {
    let env = Environment::new("usage-errors");

    // Bare aub sample (no selector)
    let (status, _, stderr) = env.run("http://127.0.0.1:0", &["sample"]);
    assert_eq!(status, 2);
    assert!(stderr.contains("sample requires --due, --account, or --all"));

    // Conflicting --all and --account
    let (status, _, stderr) = env.run(
        "http://127.0.0.1:0",
        &["sample", "--all", "--account", "work-primary"],
    );
    assert_eq!(status, 2);
    assert!(stderr.contains("--all and --account cannot be used together"));

    // --session-id without --account
    let (status, _, stderr) = env.run(
        "http://127.0.0.1:0",
        &["sample", "--session-id", "s1", "--due"],
    );
    assert_eq!(status, 2);
    assert!(stderr.contains("--session-id requires --account NAME"));

    // --run-id without --session-id
    let (status, _, stderr) = env.run(
        "http://127.0.0.1:0",
        &["sample", "--account", "work-primary", "--run-id", "r1"],
    );
    assert_eq!(status, 2);
    assert!(stderr.contains("--run-id requires --session-id"));

    // Unknown account
    let (status, _, stderr) = env.run(
        "http://127.0.0.1:0",
        &["sample", "--account", "nonexistent-account"],
    );
    assert_eq!(status, 2);
    assert!(stderr.contains("unknown account 'nonexistent-account'"));
}

#[test]
fn credential_resolution_boundary_never_leaks_or_mixes_ambient_token() {
    let env = Environment::new("cred-boundary");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();

    let ambient_oauth = "ambient-oauth-secret-99999";
    let ambient_env_value = "ambient-secondary-marker-88888";
    let explicit_token = "test-token";

    let mut cmd = env.command(&server.url(), &["sample", "--account", "work-primary"]);
    cmd.env("CLAUDE_CODE_OAUTH_TOKEN", ambient_oauth)
        .env("ANTHROPIC_API_KEY", ambient_env_value);

    let output = cmd.output().expect("aub must run");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");

    // Server must have received exactly the explicit token
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].authorization(),
        Some(format!("Bearer {explicit_token}").as_str())
    );

    // Assert neither secret appears anywhere in stdout, stderr, or SQLite database
    assert!(!stdout.contains(ambient_oauth));
    assert!(!stdout.contains(ambient_env_value));
    assert!(!stdout.contains(explicit_token));

    assert!(!stderr.contains(ambient_oauth));
    assert!(!stderr.contains(ambient_env_value));
    assert!(!stderr.contains(explicit_token));

    let db_bytes = std::fs::read(env.db_path()).unwrap();
    let db_str = String::from_utf8_lossy(&db_bytes);
    assert!(!db_str.contains(ambient_oauth));
    assert!(!db_str.contains(ambient_env_value));
    assert!(!db_str.contains(explicit_token));
}

#[test]
fn process_termination_durability_attempt_start_survives_without_result() {
    let env = Environment::new("process-term-durability");
    let server = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: vec![],
    }])
    .unwrap();

    let mut cmd = env.command(&server.url(), &["sample", "--due"]);
    let mut child = cmd.spawn().expect("spawn aub child process");

    // Wait until synthetic server sees the inbound request
    let start = std::time::Instant::now();
    while server.request_count() == 0 && start.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        server.request_count(),
        1,
        "synthetic server must have received the request"
    );

    // Send kill signal to child process while it is stalling on response
    let _ = child.kill();
    let _ = child.wait();

    // Verify in SQLite: 1 attempt start was committed before the request was sent, 0 results
    let (attempts, results) = env.query_attempts_and_results();
    assert_eq!(
        attempts, 1,
        "attempt start row must be durable before response or kill"
    );
    assert_eq!(
        results, 0,
        "no result row must exist since process was terminated mid-request"
    );
}

#[test]
fn store_failure_on_due_lookup_exits_store_class_5() {
    let env = Environment::new("due-lookup-fail");
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(ANTHROPIC_SUCCESS_BODY.to_vec()),
    )])
    .unwrap();

    // Run once to create ledger.db
    let (status, _, _) = env.run(&server.url(), &["sample", "--due"]);
    assert_eq!(status, 0);

    // Corrupt schema by dropping account table
    let conn = rusqlite::Connection::open(env.db_path()).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE account;")
        .unwrap();
    drop(conn);

    let (status, _, stderr) = env.run(&server.url(), &["sample", "--due"]);
    assert_eq!(
        status, 5,
        "must exit with ExitClass::Store (5); stderr: {stderr}"
    );
    assert!(stderr.contains("sampling due lookup failed"));
}
