//! End-to-end integration tests for `aub status --refresh` (aub-yg2q).
//!
//! `--refresh` asks `aub status` to take one forced sampling attempt per
//! selected account through the same sampling path `aub now` uses, then
//! render the grid from the projection that attempt published. These tests
//! hold the contract against the real binary and the synthetic provider:
//!
//! - `--refresh` takes exactly one attempt per selected account and renders
//!   the values from it; the unselected accounts are neither sampled nor
//!   rendered.
//! - Without `--refresh` the command takes no sampling attempt at all: the
//!   default stays a read of the ledger.
//! - A refresh whose attempt fails (unreachable endpoint) does not error the
//!   command: the account renders its last known reading with its age.
//! - The JSON document carries the observation's age as a machine-readable
//!   field beside the freshness variant.

use std::path::PathBuf;
use std::process::Command;

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

/// A valid Anthropic usage body: `utilization` is a percentage, so the
/// seven-day window is 50% used and is the limiting window (five-hour is 25%
/// used). The reset instants are far in the future so the reading is never at
/// a reset edge.
const ANTHROPIC_SUCCESS_BODY: &[u8] = br#"{
    "five_hour": { "utilization": 25.0, "resets_at": "2030-01-01T00:00:00Z" },
    "seven_day": { "utilization": 50.0, "resets_at": "2030-01-01T00:00:00Z" }
}"#;

struct Environment {
    root: PathBuf,
}

impl Environment {
    /// Two anthropic accounts under an isolated `HOME`, state and credential
    /// tree, so account selection can be observed in the attempt count.
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-status-refresh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("state-parent/state")).unwrap();
        std::fs::create_dir_all(root.join("creds")).unwrap();

        std::fs::write(root.join("creds/a.json"), r#"{"accessToken":"token-a"}"#).unwrap();
        std::fs::write(root.join("creds/b.json"), r#"{"accessToken":"token-b"}"#).unwrap();

        std::fs::write(
            root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n\
                 [[accounts]]\nname = \"work-a\"\nprovider = \"anthropic\"\n\
                 credential = {{ kind = \"file\", path = \"{}\" }}\n\n\
                 [[accounts]]\nname = \"work-b\"\nprovider = \"anthropic\"\n\
                 credential = {{ kind = \"file\", path = \"{}\" }}\n",
                root.join("state-parent/state").display(),
                root.join("creds/a.json").display(),
                root.join("creds/b.json").display(),
            ),
        )
        .unwrap();
        Self { root }
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state-parent/state")
    }

    fn db_path(&self) -> PathBuf {
        self.state_dir().join("ledger.db")
    }

    fn command(&self, server_url: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aub"));
        command
            .env("HOME", self.root.join("home"))
            .env("AUB_CONFIG_FILE", self.root.join("aub.toml"))
            .env("AUB_ANTHROPIC_ENDPOINT", server_url)
            .args(args);
        command
    }

    fn run(&self, server_url: &str, args: &[&str]) -> Output {
        let out = self
            .command(server_url, args)
            .output()
            .expect("aub must run");
        Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// `(attempt starts, terminal results, response-evidence rows)`.
    fn store_rows(&self) -> (i64, i64, i64) {
        let conn = rusqlite::Connection::open(self.db_path()).expect("open ledger");
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        (
            count("SELECT count(*) FROM meter_attempt"),
            count("SELECT count(*) FROM meter_attempt_result"),
            count("SELECT count(*) FROM meter_response_evidence"),
        )
    }
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

fn success_server(responses: usize) -> SyntheticServer {
    let script = (0..responses)
        .map(|_| {
            ScriptedOutcome::Success(ScriptedResponseBody::json_ok(
                ANTHROPIC_SUCCESS_BODY.to_vec(),
            ))
        })
        .collect();
    SyntheticServer::start(script).unwrap()
}

/// The loopback port nothing listens on: every dial to it fails immediately.
const UNREACHABLE: &str = "http://127.0.0.1:9";

/// `--refresh` takes exactly one attempt for the selected account only, and
/// the grid renders the values that attempt produced. The planted negative is
/// the unselected account: a refresh that sampled every configured account
/// would leave two attempts in the store, and a render that fell back to the
/// ledger instead of the fresh reading would show no `50%` row.
#[test]
fn refresh_takes_exactly_one_attempt_for_the_selected_account_and_renders_it() {
    let env = Environment::new("selected");
    // Two scripted responses: one for the text-grid refresh, one for the JSON
    // refresh below. Each invocation takes its own attempt, so the attempt
    // count is asserted after the first one only.
    let server = success_server(2);

    let refreshed = env.run(
        &server.url(),
        &["status", "--refresh", "--account", "work-a"],
    );
    assert_eq!(
        refreshed.code, 0,
        "status --refresh must exit 0: {}",
        refreshed.stderr
    );

    let (attempts, results, evidence) = env.store_rows();
    assert_eq!(attempts, 1, "exactly one attempt start: {attempts}");
    assert_eq!(results, 1, "exactly one terminal result: {results}");
    assert_eq!(evidence, 1, "one response-evidence row: {evidence}");

    let account_block = refreshed
        .stdout
        .lines()
        .skip_while(|line| !line.contains("  work-a  "))
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        account_block.contains("· observed "),
        "the refreshed block names the reading's age at a glance: {}",
        refreshed.stdout
    );
    assert!(
        account_block.contains(" 50% "),
        "the grid renders the value the forced attempt produced: {}",
        refreshed.stdout
    );
    assert!(
        !refreshed.stdout.contains("  work-b  "),
        "an unselected account is neither sampled nor rendered: {}",
        refreshed.stdout
    );

    // The JSON document carries the age machine-readably beside the freshness
    // variant, and the schema moved with the field set.
    let json = env.run(
        &server.url(),
        &[
            "status",
            "--refresh",
            "--account",
            "work-a",
            "--format",
            "json",
        ],
    );
    assert_eq!(
        json.code, 0,
        "status --refresh --format json: {}",
        json.stderr
    );
    let parsed: serde_json::Value = serde_json::from_str(json.stdout.trim()).unwrap();
    assert_eq!(parsed["schema"], 5);
    let account = &parsed["accounts"][0];
    assert_eq!(account["account"], "work-a");
    assert_eq!(account["freshness"], "fresh");
    assert!(
        account["observation_age_nanos"].is_u64(),
        "the age is machine-readable: {account}"
    );
}

/// Without `--refresh` the command takes no sampling attempt: the default
/// stays a read of the ledger. The planted negative is the attempt count: a
/// status that sampled on its own would grow it.
#[test]
fn plain_status_takes_no_sampling_attempt() {
    let env = Environment::new("plain");
    // One scripted success per account for the seeding `now`; the later plain
    // status must not consume any of them.
    let server = success_server(2);

    let now = env.run(&server.url(), &["now"]);
    assert_eq!(now.code, 0, "aub now seeds the ledger: {}", now.stderr);
    let (attempts, results, evidence) = env.store_rows();
    // One seeding attempt per configured account, each reaching a terminal
    // result and carrying its response evidence.
    assert_eq!(attempts, 2, "the seeding attempts: {attempts}");
    assert_eq!(results, 2);
    assert_eq!(evidence, 2);

    let status = env.run(UNREACHABLE, &["status"]);
    assert_eq!(
        status.code, 0,
        "plain status must exit 0: {}",
        status.stderr
    );
    assert!(
        status.stdout.contains("  work-a  "),
        "plain status renders the stored reading: {}",
        status.stdout
    );
    let (after, results_after, evidence_after) = env.store_rows();
    assert_eq!(after, 2, "plain status took no attempt: {after}");
    assert_eq!(results_after, 2, "no terminal result was added");
    assert_eq!(evidence_after, 2, "no response evidence was added");
}

/// A refresh whose attempt fails does not error the whole command: the
/// account renders its last known reading with its age, and the failed
/// attempt is still recorded so the ledger tells the truth about what was
/// tried.
#[test]
fn a_failed_refresh_falls_back_to_the_stored_reading() {
    let env = Environment::new("fallback");
    // One scripted success per account for the seeding `now`; the refresh
    // below dials the unreachable endpoint and must not consume any of them.
    let server = success_server(2);

    let now = env.run(&server.url(), &["now"]);
    assert_eq!(now.code, 0, "aub now seeds a good reading: {}", now.stderr);
    let (attempts, _, _) = env.store_rows();
    assert_eq!(attempts, 2, "one seeding attempt per account: {attempts}");

    let refreshed = env.run(UNREACHABLE, &["status", "--refresh"]);
    assert_eq!(
        refreshed.code, 0,
        "a failed refresh is an answer, not an error: {}",
        refreshed.stderr
    );

    let (after, results_after, _) = env.store_rows();
    assert_eq!(
        after, 4,
        "the failed refresh still recorded its attempt: {after}"
    );
    assert_eq!(
        results_after, 4,
        "the failed attempt reached a terminal result: {results_after}"
    );

    let account_block = refreshed
        .stdout
        .lines()
        .skip_while(|line| !line.contains("  work-a  "))
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        account_block.contains("· observed "),
        "the stored reading renders with its age: {}",
        refreshed.stdout
    );
    assert!(
        account_block.contains(" 50% "),
        "the stored reading is the one rendered, not a disappearance: {}",
        refreshed.stdout
    );
    assert!(
        !refreshed.stdout.contains("no successful sample"),
        "a stored reading never degrades to the never-observed form: {}",
        refreshed.stdout
    );
}
