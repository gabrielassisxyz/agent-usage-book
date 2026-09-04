//! Integration, contract and unit tests for `aub now` (aub-eun.7).
//!
//! `aub now` forces a persisted sampling attempt for the selected accounts and
//! renders the resulting current state through the same freshness function and
//! report models `aub status` uses. These tests hold that contract:
//!
//! - Criterion 1: `now` and `now --account NAME` force an attempt and persist it
//!   before rendering.
//! - Criterion 2: no flag or environment variable produces an unrecorded fetch.
//! - Criterion 3: rendering runs through the shared freshness function and models.
//! - Criterion 4: a persistence failure is reported with the store class and no
//!   reading is rendered.
//! - Criterion 5: human and JSON output carry exactly one freshness variant per
//!   account.
//! - Criterion 6: the command publishes a projection, so a following `status`
//!   agrees with it.
//! - Criterion 7: a `now` immediately followed by a `status` reports the same
//!   reading and the same freshness.

use std::path::PathBuf;
use std::process::Command;

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

use agent_usage_book::cli::{Command as AubCommand, FlagSupport};
use agent_usage_book::domain::attempt::AttemptId;
use agent_usage_book::domain::freshness::{Freshness, Observed, StaleReason};
use agent_usage_book::domain::quota::{QuotaFractionPpm, QuotaRemaining};
use agent_usage_book::domain::time::{
    ClockSkewEnvelope, MeasurementBasis, MonotonicDuration, ReceivedAt, UtcTimestamp,
};
use agent_usage_book::domain::window::{NominalWindowDuration, WindowResetState, WindowScope};
use agent_usage_book::logging::{LogicalName, RunId};
use agent_usage_book::presentation::json::{
    now_json, now_json_with_explain, validate_now_report_json,
};
use agent_usage_book::presentation::render::{
    ExplainMode, render_now_report, render_now_report_with_explain,
};
use agent_usage_book::report::{
    LedgerGeneration, LimitingWindow, MeterAccount, NowReport, ReportMetadata,
};

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

/// A valid Anthropic usage body: `utilization` is a percentage, so the seven-day
/// window is 50% used and is the limiting window (five-hour is 25% used). The
/// reset instants are far in the future so the reading is never at a reset edge.
const ANTHROPIC_SUCCESS_BODY: &[u8] = br#"{
    "five_hour": { "utilization": 25.0, "resets_at": "2030-01-01T00:00:00Z" },
    "seven_day": { "utilization": 50.0, "resets_at": "2030-01-01T00:00:00Z" }
}"#;

/// The fresh line `now` and `status` both render for a 50%-used limiting window.
const FRESH_SEVEN_DAY_LINE: &str = "50% left · 7d";

struct Environment {
    root: PathBuf,
}

impl Environment {
    /// Two anthropic accounts under an isolated `HOME`, state and credential
    /// tree. `state_dir_parent` holds the state directory so a test can make it
    /// read-only to force a persistence failure.
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("aub-now-{tag}-{}", std::process::id()));
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

    fn state_dir_parent(&self) -> PathBuf {
        self.root.join("state-parent")
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

impl Drop for Environment {
    fn drop(&mut self) {
        // Restore write permission so the tree can be removed even after the
        // persistence-failure test locked the state directory's parent.
        let _ = std::fs::set_permissions(
            self.state_dir_parent(),
            std::fs::Permissions::from_mode(0o755),
        );
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

use std::os::unix::fs::PermissionsExt;

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

// -----------------------------------------------------------------------------
// Criteria 1, 5, 6, 7: forced persisted sampling, projection publish, agreement
// -----------------------------------------------------------------------------

#[test]
fn now_forces_persistence_then_agrees_with_an_immediate_status() {
    let env = Environment::new("agree");
    let server = success_server(2);

    let now = env.run(&server.url(), &["-v", "now"]);
    assert_eq!(now.code, 0, "aub now must exit 0: {}", now.stderr);
    assert!(
        now.stdout
            .contains(&format!("aub work-a {FRESH_SEVEN_DAY_LINE}")),
        "now stdout: {}",
        now.stdout
    );
    assert!(
        now.stdout
            .contains(&format!("aub work-b {FRESH_SEVEN_DAY_LINE}")),
        "now stdout: {}",
        now.stdout
    );

    // Criterion 1: the forced attempt, its result and its evidence are durable
    // in the store the moment the process has exited.
    let (attempts, results, evidence) = env.store_rows();
    assert_eq!(attempts, 2, "one attempt start per account");
    assert_eq!(results, 2, "one terminal result per account");
    assert_eq!(evidence, 2, "one response-evidence row per account");

    // The structured log shows the report was emitted after the request was
    // attempted, which was after the run started. `now` never emits a
    // report before it has attempted the sample.
    let order: Vec<&str> = ["run_started", "request_attempted", "report_rendered"]
        .into_iter()
        .filter(|event| now.stderr.contains(event))
        .collect();
    assert_eq!(
        order,
        vec!["run_started", "request_attempted", "report_rendered"],
        "stderr must carry the three events in order: {}",
        now.stderr
    );
    let started = now.stderr.find("run_started").unwrap();
    let attempted = now.stderr.find("request_attempted").unwrap();
    let rendered = now.stderr.find("report_rendered").unwrap();
    assert!(
        started < attempted && attempted < rendered,
        "{}",
        now.stderr
    );

    // Criteria 6 and 7: an immediate status reads the projection `now` just
    // published and renders byte-for-byte the same account lines.
    let status = env.run("http://127.0.0.1:9", &["status"]);
    assert_eq!(status.code, 0, "aub status must exit 0: {}", status.stderr);
    assert_eq!(
        now_account_lines(&now.stdout),
        status.stdout.trim(),
        "now and an immediate status must render identical account lines"
    );

    // The planted negative for criterion 1: an implementation that rendered
    // before it persisted and published would show the never-observed reading
    // here while status a moment later shows the fresh one.
    assert!(
        !now.stdout.contains("no successful sample"),
        "now rendered a never-observed reading despite a successful forced sample: {}",
        now.stdout
    );
}

/// The account lines of `now`'s text output, with the diagnostic lines (which
/// `-v` sends to stderr, not stdout) and trailing whitespace removed.
fn now_account_lines(stdout: &str) -> String {
    stdout.trim().to_string()
}

#[test]
fn now_json_carries_exactly_one_freshness_variant_per_account() {
    let env = Environment::new("json");
    let server = success_server(2);

    let out = env.run(&server.url(), &["now", "--format", "json"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);

    let parsed = validate_now_report_json(out.stdout.trim())
        .expect("now --format json must conform to schema v1");
    assert_eq!(parsed.command, "now");

    let doc: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
    let accounts = doc["accounts"].as_array().expect("accounts array");
    assert_eq!(accounts.len(), 2, "one object per configured account");
    for account in accounts {
        let obj = account.as_object().unwrap();
        let freshness_keys = obj.keys().filter(|k| *k == "freshness").count();
        assert_eq!(
            freshness_keys, 1,
            "exactly one freshness field per account: {account}"
        );
        assert!(
            matches!(
                obj["freshness"].as_str(),
                Some("fresh") | Some("stale") | Some("auth_required")
            ),
            "freshness is one of the three variants: {account}"
        );
    }
}

// -----------------------------------------------------------------------------
// Criterion 1 / 4: --account scopes the forced sample
// -----------------------------------------------------------------------------

#[test]
fn account_flag_forces_and_renders_only_the_named_account() {
    let env = Environment::new("scope");
    let server = success_server(1);

    let out = env.run(&server.url(), &["now", "--account", "work-a"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(out.stdout.contains("aub work-a"), "stdout: {}", out.stdout);
    assert!(
        !out.stdout.contains("aub work-b"),
        "work-b must not be sampled or rendered when scoped: {}",
        out.stdout
    );

    let (attempts, results, evidence) = env.store_rows();
    assert_eq!(
        (attempts, results, evidence),
        (1, 1, 1),
        "only work-a sampled"
    );
}

#[test]
fn now_rejects_an_unknown_account_as_a_usage_error() {
    let env = Environment::new("unknown-account");
    let out = env.run("http://127.0.0.1:9", &["now", "--account", "nope"]);
    assert_eq!(
        out.code, 2,
        "unknown account is a usage error: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("unknown account 'nope'"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "nothing is rendered: {}", out.stdout);
}

// -----------------------------------------------------------------------------
// Criterion 2: no flag or environment variable produces an unrecorded fetch
// -----------------------------------------------------------------------------

#[test]
fn now_accepts_only_format_explain_account_verbosity_and_no_bypass_flag() {
    // The only flags `now` accepts are format, explain, account and verbosity.
    // None of them changes what is fetched or whether it is recorded. `--model`
    // and `--no-color` are refused with their reasons; there is deliberately no
    // flag that fetches without recording.
    let now = AubCommand::Now.flag_policy();
    assert_eq!(now.format, FlagSupport::Accepted);
    assert_eq!(now.explain, FlagSupport::Accepted);
    assert_eq!(now.account, FlagSupport::Accepted);
    assert_eq!(now.verbosity, FlagSupport::Accepted);
    assert!(matches!(now.model, FlagSupport::Rejected { .. }));
    assert!(matches!(now.no_color, FlagSupport::Rejected { .. }));

    // The planted negative: enumerate the bypass-shaped flags a caller might
    // reach for to skip recording or skip the network, and assert every one is
    // rejected. A `now` that grew any of them would exit 0 here.
    let env = Environment::new("flag-matrix");
    for bypass in [
        "--no-record",
        "--dry-run",
        "--offline",
        "--no-persist",
        "--peek",
        "--cached",
        "--stale-ok",
        "--no-network",
    ] {
        let out = env.run("http://127.0.0.1:9", &["now", bypass]);
        assert_ne!(
            out.code, 0,
            "`now {bypass}` must be rejected, not accepted: {}",
            out.stdout
        );
        assert!(
            out.stderr.contains("unknown") || out.stderr.contains("unrecognized"),
            "`now {bypass}` must fail as an unknown flag: {}",
            out.stderr
        );
    }
}

// -----------------------------------------------------------------------------
// Criterion 4: a persistence failure is reported with the store class
// -----------------------------------------------------------------------------

#[test]
fn a_persistence_failure_is_reported_with_the_store_class_and_renders_nothing() {
    let env = Environment::new("persist-fail");

    // Point the state directory at a path whose parent cannot be written, so
    // the state-directory readiness check fails before any network request.
    let blocked_state = env.state_dir_parent().join("locked/state");
    std::fs::write(
        env.root.join("aub.toml"),
        format!(
            "state.dir = \"{}\"\n\n[[accounts]]\nname = \"work-a\"\nprovider = \"anthropic\"\n\
             credential = {{ kind = \"file\", path = \"{}\" }}\n",
            blocked_state.display(),
            env.root.join("creds/a.json").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        env.state_dir_parent(),
        std::fs::Permissions::from_mode(0o555),
    )
    .unwrap();

    let out = env.run("http://127.0.0.1:9", &["now"]);

    std::fs::set_permissions(
        env.state_dir_parent(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    // ExitClass::Store is 5.
    assert_eq!(
        out.code, 5,
        "a store failure must exit with the store class: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("state directory"),
        "the failure names the store, not a bare error: {}",
        out.stderr
    );
    // The planted negative: no reading is printed. A value the code could not
    // record must never be rendered as though it had been.
    assert!(
        out.stdout.is_empty() && !out.stdout.contains("aub work-a"),
        "nothing is rendered when persistence failed: {}",
        out.stdout
    );
}

// -----------------------------------------------------------------------------
// Criteria 3 and 5: the shared freshness function and one variant per account
// -----------------------------------------------------------------------------

fn observed(ppm: u32) -> Observed<QuotaRemaining> {
    Observed::new(
        QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap()),
        None,
        ReceivedAt::new(UtcTimestamp::from_unix_nanos(1_000)),
        MeasurementBasis::ProviderObserved,
    )
}

#[test]
fn the_now_renderers_carry_one_freshness_variant_per_account_in_both_formats() {
    let now = UtcTimestamp::from_unix_nanos(2_000);
    let envelope = ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60));
    let metadata = ReportMetadata::new(now, now, LedgerGeneration::new(1), None);
    let run = RunId::new(now);

    let fresh = MeterAccount::from_projection(
        LogicalName::new("work-a"),
        Freshness::Fresh {
            observed: observed(250_000),
            latest_attempt: AttemptId::new(1),
        },
        Some(LimitingWindow {
            scope: WindowScope::AccountWide,
            nominal_duration: NominalWindowDuration::from_nanos(18_000_000_000_000),
            reset_state: WindowResetState::Known(now),
        }),
        vec![WindowScope::AccountWide],
        None,
    );
    let stale = MeterAccount::from_projection(
        LogicalName::new("work-b"),
        Freshness::Stale {
            reason: StaleReason::AgeExceeded,
            last_good: Some(observed(500_000)),
            latest_attempt: AttemptId::new(2),
        },
        None,
        vec![WindowScope::AccountWide],
        None,
    );
    let auth = MeterAccount::from_projection(
        LogicalName::new("work-c"),
        Freshness::AuthRequired {
            last_good: None,
            latest_attempt: AttemptId::new(3),
        },
        None,
        vec![WindowScope::AccountWide],
        None,
    );

    let report = NowReport::new(metadata, vec![fresh, stale, auth], Vec::new());

    // Text: exactly one line per account, each carrying its one variant, in the
    // wording the shared meter-reading renderer produces.
    let text = render_now_report(&report, now, envelope);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "one line per account: {text}");
    assert_eq!(lines[0], "aub work-a 25% left · 5h");
    assert_eq!(lines[1], "aub work-b ~50% · stale 0s · age exceeded");
    assert_eq!(lines[2], "aub work-c auth!");

    // JSON: validates against schema v1 and every account carries one freshness.
    let doc: serde_json::Value = serde_json::from_str(&now_json(&report, run.clone())).unwrap();
    validate_now_report_json(&doc.to_string()).expect("now JSON conforms to schema v1");
    for account in doc["accounts"].as_array().unwrap() {
        assert_eq!(
            account
                .as_object()
                .unwrap()
                .keys()
                .filter(|k| *k == "freshness")
                .count(),
            1
        );
    }

    // Explain travels through the shared explain block, in both formats.
    let explained = render_now_report_with_explain(&report, now, envelope, ExplainMode::Summary);
    assert!(
        explained.starts_with("aub work-a 25% left · 5h"),
        "{explained}"
    );
    let json_explain = now_json_with_explain(&report, run, ExplainMode::Summary);
    assert_eq!(
        validate_now_report_json(&json_explain).unwrap().command,
        "now"
    );
}
