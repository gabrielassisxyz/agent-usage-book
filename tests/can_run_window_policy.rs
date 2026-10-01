//! End-to-end proof for `aub-igbq`: the two directions of the window policy
//! `aub-eun.15` decided, as seen by `aub can-run`.
//!
//! A reading with a required window absent makes can-run refuse quantitative
//! advice with its typed reason; an optional window disappearing, or
//! reappearing, changes nothing in the verdict and writes no anomaly.
//!
//! Every ledger below is produced by the production write path: a real
//! `aub sample` against a scripted stub endpoint (the adapter's parse of the
//! provider body, persisted by the sampler's store functions), then a real
//! `aub can-run --cached` over the result. No test inserts meter rows by
//! hand: a hand-built ledger would prove only the report layer, which is the
//! gap this bead closes. The transcript, tracker, calibration and cost-model
//! scaffolding each test needs goes through the same binary commands
//! `tests/e2e/cases/026-can-run.sh` uses (`ingest transcripts`, `task
//! ingest`, `cost-model activate`, `__calibration-fixture`).
//!
//! Bead-text correction (firstmate decision on `aub-igbq`, 2026-09-30): the
//! bead text names the refusal reason
//! `StaleReason::SourceUnreachable(FailureClass::MissingRequiredField)`.
//! Production maps `FailureClass::MissingRequiredField` to
//! `StaleReason::MalformedProviderResponse` instead
//! (`src/domain/failure.rs:86-88`, pinned by
//! `every_failure_class_maps_to_exactly_one_stale_reason`). The first test
//! therefore asserts `MalformedProviderResponse` (JSON reason
//! `malformed_provider_response`, text `the provider's response could not be
//! parsed`) together with the ledger's stored `missing_required_field`
//! classification, and refuses to change production code to match the bead
//! text.

use std::process::{Command, Output};

use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::time::{MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{self, AccessMode, PragmaPolicy};
use agent_usage_book::store::session_account_marker::{
    EvidenceDesignation, MarkerSource, NewSessionAccountMarker, insert_marker,
};
use test_support::{ScriptedOutcome, ScriptedResponseBody, StateDir, SyntheticServer};

const ACCOUNT: &str = "work-primary";
const TASK_MODEL: &str = "sonnet";

fn aub(state: &StateDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aub"));
    cmd.env("HOME", state.path().join("home"))
        .env("AUB_STATE_DIR", state.path())
        .env("AUB_CONFIG_FILE", state.path().join("aub.toml"))
        .env("AUB_LOG_LEVEL", "off");
    cmd
}

/// Runs one `aub` invocation to completion and returns its stdout, panicking
/// with both streams on a non-zero exit so a failing step names itself
/// instead of surfacing as a confusing later assertion failure.
fn run(state: &StateDir, args: &[&str]) -> String {
    let output = aub(state)
        .args(args)
        .output()
        .expect("the aub binary must be spawnable");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "aub {args:?} must succeed, got {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    stdout
}

fn run_with_endpoint(state: &StateDir, endpoint: &str, args: &[&str]) -> Output {
    aub(state)
        .env("AUB_ANTHROPIC_ENDPOINT", endpoint)
        .args(args)
        .output()
        .expect("the aub binary must be spawnable")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Seeds everything `can-run --cached` needs except the meter reading under
/// test, through the same binary commands the `026-can-run` e2e case uses:
/// three completed tasks with markers on `work-primary`, the published cost
/// model, and current calibrations for the legacy `five_hour`, `seven_day`
/// and `seven_day_sonnet` windows.
fn seed_history(state: &StateDir) {
    std::fs::create_dir_all(state.path().join("home")).unwrap();
    std::fs::create_dir_all(state.path().join("creds")).unwrap();
    std::fs::write(
        state.path().join("creds/token.json"),
        r#"{"accessToken":"test-token"}"#,
    )
    .unwrap();

    let corpus = state.path().join("transcripts/claude-code/project");
    std::fs::create_dir_all(&corpus).unwrap();
    let mut body = String::new();
    for (session, input, output, at) in [
        ("s1", 1000, 500_000, "2026-08-25T01:00:00.000Z"),
        ("s2", 1000, 800_000, "2026-08-25T03:00:00.000Z"),
        ("s3", 1000, 1_100_000, "2026-08-25T05:00:00.000Z"),
    ] {
        body.push_str(&format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"sessionId\":\"{session}\",\"message\":{{\"id\":\"m-{session}\",\"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n",
        ));
    }
    std::fs::write(corpus.join("sessions.jsonl"), body).unwrap();

    let config = format!(
        "state.dir = \"{}\"\n\n[[accounts]]\nname = \"{ACCOUNT}\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"{}/creds/token.json\" }}\n\n[task_distribution]\nmin_samples = 3\n\n[[transcripts]]\nname = \"claude-code\"\nroot = \"{}\"\npattern = \"**/*.jsonl\"\nformat = \"claude-code\"\n\n[[trackers]]\nname = \"beads\"\nkind = \"local\"\npath = \"{}/tracker\"\n",
        state.path().display(),
        state.path().display(),
        state.path().join("transcripts/claude-code").display(),
        state.path().display(),
    );
    std::fs::write(state.path().join("aub.toml"), config).unwrap();

    let tracker = state.path().join("tracker/beads.db");
    std::fs::create_dir_all(tracker.parent().unwrap()).unwrap();
    let tracker_status = Command::new("sqlite3")
        .arg(&tracker)
        .arg(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT,
                old_value TEXT,
                new_value TEXT,
                created_at TEXT NOT NULL
            );
            INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at) VALUES
             (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T00:30:00Z'),
             (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T02:00:00Z'),
             (3, 'aub-2', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T02:30:00Z'),
             (4, 'aub-2', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T04:00:00Z'),
             (5, 'aub-3', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T04:30:00Z'),
             (6, 'aub-3', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T06:00:00Z');",
        )
        .output()
        .expect("sqlite3 must be spawnable for the tracker seed");
    assert!(
        tracker_status.status.success(),
        "the tracker seed must apply: {}",
        String::from_utf8_lossy(&tracker_status.stderr)
    );

    run(state, &["ingest", "transcripts"]);
    run(state, &["task", "ingest"]);

    let path = state.path().join(connection::LEDGER_DATABASE_FILE);
    let conn = connection::open(
        &path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .expect("the ledger must already exist and open");
    conn.execute_batch(
        "INSERT INTO task_identity (
            task_source, task_native, state, kind, winner_origin, evidence,
            normalization_version, size_state, size, size_evidence,
            difficulty_state, difficulty, difficulty_evidence
        ) VALUES
         ('beads', 'aub-1', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-2', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-3', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}');",
    )
    .expect("the task kinds must insert");
    for session in ["s1", "s2", "s3"] {
        insert_marker(
            &conn,
            &NewSessionAccountMarker {
                session_id: SessionId::new(
                    SourceNamespace::new("claude-code"),
                    NativeSessionId::new(session),
                ),
                observed_at: UtcTimestamp::parse_rfc3339("2026-08-25T00:30:00Z")
                    .expect("the marker timestamp must parse"),
                source_ordering_key: None,
                logical_account: ACCOUNT.to_string(),
                resolved_account_id: None,
                marker_source: MarkerSource::new("hook"),
                run_id: None,
                evidence_designation: EvidenceDesignation::ExplicitLauncherOrHook,
            },
        )
        .expect("the account marker must insert");
    }
    drop(conn);

    run(
        state,
        &["cost-model", "activate", "anthropic_claude_messages_v1"],
    );
    run(state, &["__calibration-fixture", "five_hour", "100"]);
    run(state, &["__calibration-fixture", "seven_day", "100"]);
    run(state, &["__calibration-fixture", "seven_day_sonnet", "40"]);
}

/// One legacy-shape Anthropic usage body: the required `five_hour` and
/// `seven_day` windows plus the optional model-specific window only when
/// `sonnet_utilization` is `Some`. Resets sit in 2099 so no reset edge is
/// ever due while the test runs.
fn legacy_body(sonnet_utilization: Option<f64>) -> Vec<u8> {
    const RESETS_AT: &str = "2099-01-01T00:00:00.000Z";
    let sonnet_field = sonnet_utilization.map_or(String::new(), |utilization| {
        format!(r#","seven_day_sonnet":{{"utilization":{utilization},"resets_at":"{RESETS_AT}"}}"#)
    });
    format!(
        r#"{{"five_hour":{{"utilization":62.0,"resets_at":"{RESETS_AT}"}},"seven_day":{{"utilization":30.0,"resets_at":"{RESETS_AT}"}}{sonnet_field}}}"#
    )
    .into_bytes()
}

/// One limits-contract body holding exactly the given required kinds: `None`
/// drops that kind from the array. The optional `weekly_scoped` entry always
/// carries the sonnet model identity when present.
fn limits_body(session: Option<f64>, weekly_all: f64, sonnet: Option<f64>) -> Vec<u8> {
    const RESETS_AT: &str = "2099-01-01T00:00:00.000Z";
    let mut limits = Vec::new();
    if let Some(percent) = session {
        limits.push(format!(
            r#"{{"kind":"session","percent":{percent},"severity":"normal","resets_at":"{RESETS_AT}","scope":null,"is_active":true}}"#
        ));
    }
    limits.push(format!(
        r#"{{"kind":"weekly_all","percent":{weekly_all},"severity":"warning","resets_at":"{RESETS_AT}","scope":null,"is_active":true}}"#
    ));
    if let Some(percent) = sonnet {
        limits.push(format!(
            r#"{{"kind":"weekly_scoped","percent":{percent},"severity":"critical","resets_at":"{RESETS_AT}","scope":{{"model":"sonnet"}},"is_active":true}}"#
        ));
    }
    format!("{{\"limits\":[{}]}}", limits.join(",")).into_bytes()
}

fn scalar(db: &std::path::Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(db).expect("the ledger must open");
    conn.query_row(sql, [], |row| row.get(0))
        .expect("the aggregate must read")
}

fn db_path(state: &StateDir) -> std::path::PathBuf {
    state.path().join(connection::LEDGER_DATABASE_FILE)
}

/// Runs `can-run --cached` against an unreachable endpoint so the test proves
/// the verdict comes from the persisted ledger and no live fetch happens,
/// and returns the parsed JSON report.
fn can_run_json(state: &StateDir) -> serde_json::Value {
    let output = run_with_endpoint(
        state,
        "http://127.0.0.1:9",
        &[
            "can-run",
            "--task-kind",
            "task",
            "--account",
            ACCOUNT,
            "--task-model",
            TASK_MODEL,
            "--cached",
            "--format",
            "json",
        ],
    );
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert!(
        output.status.success(),
        "account={ACCOUNT} can-run --cached must exit 0, got {:?}.\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    serde_json::from_str(&stdout).expect("can-run must emit valid JSON")
}

/// A refused Anthropic reading whose limits body lacks the required `session`
/// kind makes can-run refuse quantitative advice: the stored attempt is
/// unreachable with the `missing_required_field` classification, and the
/// report refuses with the typed `malformed_provider_response` reason rather
/// than any quantitative verdict.
#[test]
fn missing_required_window_makes_can_run_refuse_with_malformed_response() {
    let kinds_present = "[weekly_all, weekly_scoped_sonnet]";
    let kinds_absent = "[session]";
    let state = StateDir::new();
    seed_history(&state);
    let server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(limits_body(None, 21.0, Some(24.0))),
    )])
    .expect("the stub endpoint must start");

    let sample = run_with_endpoint(&state, &server.url(), &["sample", "--account", ACCOUNT]);
    let sample_stdout = stdout_of(&sample);
    let sample_stderr = stderr_of(&sample);
    assert!(
        sample.status.success(),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the refused sample must still exit 0, got {:?}.\nstdout: {sample_stdout}\nstderr: {sample_stderr}",
        sample.status.code()
    );
    assert!(
        sample_stdout.contains("outcome=unreachable"),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the sample must report the refused attempt, got:\n{sample_stdout}"
    );

    let db = db_path(&state);
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM meter_observation"),
        0,
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         a refused reading persists no observation"
    );
    let (outcome, failure_class, classification): (String, Option<String>, Option<String>) =
        rusqlite::Connection::open(&db)
            .expect("the ledger must open")
            .query_row(
                "SELECT outcome, failure_class, sanitized_error_classification \
                 FROM meter_attempt_result ORDER BY attempt_id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .expect("the refused attempt result must read");
    assert_eq!(
        outcome, "unreachable",
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the refused attempt must be unreachable, got {outcome}"
    );
    assert_eq!(
        failure_class.as_deref(),
        Some("missing_required_field"),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the refused attempt must carry the missing-required-field class, got {failure_class:?}"
    );
    assert_eq!(
        classification.as_deref(),
        Some("missing_required_field"),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the refused attempt must store the missing-required-field classification, got {classification:?}"
    );

    let report = can_run_json(&state);
    assert_eq!(
        report["outcome"]["status"], "refused",
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         can-run must refuse, got {report}"
    );
    let missing = report["outcome"]["missing"]
        .as_array()
        .expect("a refusal carries missing facts");
    assert!(
        missing.iter().any(|fact| fact["subject"] == "meter"
            && fact["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("could not be parsed")),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the refusal must name the meter with the malformed-response reason, got {missing:?}"
    );

    let text_output = run_with_endpoint(
        &state,
        "http://127.0.0.1:9",
        &[
            "can-run",
            "--task-kind",
            "task",
            "--account",
            ACCOUNT,
            "--task-model",
            TASK_MODEL,
            "--cached",
        ],
    );
    let text = stdout_of(&text_output);
    assert!(
        text.contains("the provider's response could not be parsed"),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         the rendered refusal must carry the malformed-response reason, got:\n{text}"
    );
    assert!(
        !text.contains("headroom"),
        "account={ACCOUNT} windows_present={kinds_present} required_absent={kinds_absent} verdict=refused: \
         a refusal must print no quantitative headroom, got:\n{text}"
    );
}

/// An optional window present in the first reading and absent in the second
/// changes nothing: can-run over both readings reports the same verdict and
/// window list as can-run over a ledger holding only the second reading.
#[test]
fn disappearing_optional_window_changes_neither_verdict_nor_windows() {
    let kinds_first = "[five_hour, seven_day, seven_day_sonnet]";
    let kinds_second = "[five_hour, seven_day]";
    let first = StateDir::new();
    seed_history(&first);
    let first_server = SyntheticServer::start(vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(legacy_body(Some(48.0)))),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(legacy_body(None))),
    ])
    .expect("the stub endpoint must start");
    for sample_no in [1, 2] {
        let sample = run_with_endpoint(
            &first,
            &first_server.url(),
            &["sample", "--account", ACCOUNT],
        );
        assert!(
            sample.status.success() && stdout_of(&sample).contains("outcome=success"),
            "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
             sample {sample_no} must succeed.\nstdout: {}\nstderr: {}",
            stdout_of(&sample),
            stderr_of(&sample)
        );
    }

    let second = StateDir::new();
    seed_history(&second);
    let second_server = SyntheticServer::start(vec![ScriptedOutcome::Success(
        ScriptedResponseBody::json_ok(legacy_body(None)),
    )])
    .expect("the stub endpoint must start");
    let only = run_with_endpoint(
        &second,
        &second_server.url(),
        &["sample", "--account", ACCOUNT],
    );
    assert!(
        only.status.success() && stdout_of(&only).contains("outcome=success"),
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         the lone second-reading sample must succeed.\nstdout: {}\nstderr: {}",
        stdout_of(&only),
        stderr_of(&only)
    );

    let layered = can_run_json(&first);
    let lone = can_run_json(&second);
    for (name, report) in [("layered", &layered), ("lone", &lone)] {
        assert_eq!(
            report["outcome"]["status"], "ready",
            "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
             the {name} ledger must answer quantitatively, got {report}"
        );
    }
    assert_eq!(
        layered["outcome"]["assessment"], lone["outcome"]["assessment"],
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         the layered verdict must equal the lone-reading verdict"
    );
    assert_eq!(
        layered["limiting_window"], lone["limiting_window"],
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         the layered limiting window must equal the lone-reading one"
    );
    assert_eq!(
        layered["outcome"]["windows"], lone["outcome"]["windows"],
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         the layered window list must equal the lone-reading window list"
    );
}

/// The reverse direction, an optional window absent and then present: no
/// `meter_window_anomaly` row is written and can-run answers quantitatively
/// rather than refusing.
#[test]
fn reappearing_optional_window_writes_no_anomaly_and_no_refusal() {
    let kinds_first = "[five_hour, seven_day]";
    let kinds_second = "[five_hour, seven_day, seven_day_sonnet]";
    let state = StateDir::new();
    seed_history(&state);
    let server = SyntheticServer::start(vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(legacy_body(None))),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(legacy_body(Some(48.0)))),
    ])
    .expect("the stub endpoint must start");
    for sample_no in [1, 2] {
        let sample = run_with_endpoint(&state, &server.url(), &["sample", "--account", ACCOUNT]);
        assert!(
            sample.status.success() && stdout_of(&sample).contains("outcome=success"),
            "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
             sample {sample_no} must succeed.\nstdout: {}\nstderr: {}",
            stdout_of(&sample),
            stderr_of(&sample)
        );
    }

    let db = db_path(&state);
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM meter_window_anomaly"),
        0,
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         a reappearing optional window must write no anomaly"
    );

    let report = can_run_json(&state);
    assert_eq!(
        report["outcome"]["status"], "ready",
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         can-run must answer quantitatively, got {report}"
    );
    assert!(
        report["outcome"]["windows"]
            .as_array()
            .is_some_and(|windows| !windows.is_empty()),
        "account={ACCOUNT} windows_first={kinds_first} windows_second={kinds_second} verdict=ready: \
         the quantitative answer must carry a window list, got {report}"
    );
}
