//! End-to-end proof owned by `aub-71j.10` for three trustworthy-first-release
//! criteria (PLAN.md section 44) that are properties of an accumulated series
//! or of the attempt lifecycle, not of any single call:
//!
//! - criterion 14, an attempt is durable before its request leaves the
//!   process: checked in a controlled run where the synthetic server holds the
//!   request open, so the ledger is read while the request is provably in
//!   flight rather than after the process is gone;
//! - criterion 15, the sanitized response evidence behind every observation is
//!   retained: checked against a series the release binary accumulated over
//!   several real invocations, by replaying every stored capsule through the
//!   adapter and comparing with what was stored from the live response;
//! - criterion 16, coverage denominators are reconstructed from the policy in
//!   force: the same series crosses a cadence change, and every attempt must
//!   point at the snapshot that was in force when it started, with the coverage
//!   engine fed the ledger's own snapshots owing the new cadence only after the
//!   change.
//!
//! Every invocation is the compiled `aub` binary against a real socket; the
//! ledger is read only after, or read-only during, each run.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use agent_usage_book::coverage::{CoverageInputs, PolicySnapshot, compute};
use agent_usage_book::domain::time::{MonotonicDuration, UtcTimestamp};
use agent_usage_book::meter::adapter::MeterRequest;
use agent_usage_book::meter::anthropic::replay_anthropic_capsule;
use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

const ACCOUNT: &str = "release-criteria";
const OLD_CADENCE_SECONDS: i64 = 300;
const NEW_CADENCE_SECONDS: i64 = 600;

struct SeriesEnvironment {
    root: PathBuf,
}

impl SeriesEnvironment {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("aub-release-criteria-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "state", "creds"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("creds/token.json"),
            r#"{"accessToken":"test-token"}"#,
        )
        .unwrap();
        let env = Self { root };
        env.write_config("5m");
        env
    }

    /// Rewrites the configuration with a new ordinary cadence. The binary reads
    /// it afresh on every invocation, so the next sample records the change as
    /// a new policy snapshot.
    fn write_config(&self, default_interval: &str) {
        std::fs::write(
            self.root.join("aub.toml"),
            format!(
                "state.dir = \"{}\"\n\n[sampling]\ndefault_interval = \"{default_interval}\"\n\n[[accounts]]\nname = \"{ACCOUNT}\"\nprovider = \"anthropic\"\ncredential = {{ kind = \"file\", path = \"{}\" }}\n",
                self.root.join("state").display(),
                self.root.join("creds/token.json").display(),
            ),
        )
        .unwrap();
    }

    fn db_path(&self) -> PathBuf {
        self.root.join("state").join("ledger.db")
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

    fn sample(&self, server_url: &str) {
        let output = self
            .command(
                server_url,
                &["sample", "--account", ACCOUNT, "--require-success"],
            )
            .output()
            .expect("aub must run");
        assert_eq!(
            output.status.code(),
            Some(0),
            "sample must succeed; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn read_only(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(
            self.db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("ledger must open read-only")
    }
}

impl Drop for SeriesEnvironment {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn anthropic_body(five_hour_pct: f64, seven_day_pct: f64) -> Vec<u8> {
    format!(
        r#"{{"five_hour":{{"utilization":{five_hour_pct},"resets_at":"2027-01-01T00:00:00Z"}},"seven_day":{{"utilization":{seven_day_pct},"resets_at":"2027-01-08T00:00:00Z"}}}}"#
    )
    .into_bytes()
}

fn scalar(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Utilization pairs served one per invocation, all distinct, so a capsule
/// retained from the wrong response cannot replay to the right numbers.
const SERIES: [(f64, f64); 6] = [
    (10.0, 21.0),
    (12.0, 22.0),
    (15.0, 23.0),
    (19.0, 24.0),
    (24.0, 25.0),
    (30.0, 26.0),
];
const CHANGE_AFTER: usize = 3;

/// Accumulates the series: three samples at the old cadence, a configuration
/// change, three more at the new one.
fn accumulate_series(env: &SeriesEnvironment) {
    let server = SyntheticServer::start(
        SERIES
            .iter()
            .map(|(five, seven)| {
                ScriptedOutcome::Success(ScriptedResponseBody::json_ok(anthropic_body(
                    *five, *seven,
                )))
            })
            .collect(),
    )
    .expect("synthetic server must start");
    for index in 0..SERIES.len() {
        if index == CHANGE_AFTER {
            env.write_config("10m");
        }
        env.sample(&server.url());
    }
    assert_eq!(server.request_count(), SERIES.len());
}

#[test]
fn release_criterion_15_every_accumulated_observation_replays_from_its_retained_evidence() {
    let env = SeriesEnvironment::new("evidence");
    accumulate_series(&env);
    let conn = env.read_only();

    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM meter_observation"),
        SERIES.len() as i64
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(DISTINCT content_hash) FROM meter_response_evidence"
        ),
        SERIES.len() as i64,
        "every response must retain evidence of its own"
    );

    let mut statement = conn
        .prepare(
            "SELECT o.id, e.evidence_capsule FROM meter_observation o
             JOIN meter_response_evidence e ON e.id = o.evidence_id
             WHERE e.attempt_id = o.attempt_id ORDER BY o.id",
        )
        .unwrap();
    let rows: Vec<(i64, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows.len(),
        SERIES.len(),
        "every observation must join evidence recorded by its own attempt"
    );

    for ((observation_id, capsule), (five, seven)) in rows.iter().zip(SERIES) {
        let replayed =
            replay_anthropic_capsule(capsule, &MeterRequest::default()).unwrap_or_else(|class| {
                panic!("observation {observation_id}: replay failed: {class:?}")
            });
        let replayed: BTreeMap<String, u32> = replayed
            .windows
            .iter()
            .map(|window| {
                (
                    window.semantic_key().as_str().to_owned(),
                    window.quota_used().as_ppm().get(),
                )
            })
            .collect();

        let mut stored_statement = conn
            .prepare(
                "SELECT semantic_key, quota_used_ppm FROM meter_window WHERE observation_id = ?1",
            )
            .unwrap();
        let stored: BTreeMap<String, u32> = stored_statement
            .query_map([observation_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();

        assert_eq!(
            replayed, stored,
            "observation {observation_id}: the retained capsule must re-derive what was stored"
        );
        let served = BTreeMap::from([
            ("five_hour".to_owned(), (five * 10_000.0) as u32),
            ("seven_day".to_owned(), (seven * 10_000.0) as u32),
        ]);
        assert_eq!(
            replayed, served,
            "observation {observation_id}: the retained capsule must be the response served"
        );
    }
}

#[test]
fn release_criterion_16_accumulated_attempts_carry_the_policy_in_force_and_denominators_follow_it()
{
    let env = SeriesEnvironment::new("policy");
    accumulate_series(&env);
    let conn = env.read_only();

    let mut statement = conn
        .prepare(
            "SELECT id, effective_at, ordinary_cadence_nanos, retry_backoff_policy
             FROM sampling_policy_snapshot ORDER BY effective_at",
        )
        .unwrap();
    let snapshots: Vec<(i64, i64, i64, String)> = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let cadences: Vec<i64> = snapshots.iter().map(|s| s.2 / 1_000_000_000).collect();
    assert_eq!(
        cadences,
        vec![OLD_CADENCE_SECONDS, NEW_CADENCE_SECONDS],
        "a changed cadence must add a snapshot and leave the earlier one as it was"
    );

    // Every attempt points at the snapshot whose effective instant is the latest
    // one not after the attempt's own start.
    let mut attempts = conn
        .prepare("SELECT id, request_started_at, policy_snapshot_id FROM meter_attempt ORDER BY id")
        .unwrap();
    let attempts: Vec<(i64, i64, i64)> = attempts
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(attempts.len(), SERIES.len());
    let mut per_snapshot = BTreeMap::<i64, usize>::new();
    for (attempt_id, started_at, snapshot_id) in &attempts {
        let in_force = snapshots
            .iter()
            .rfind(|s| s.1 <= *started_at)
            .unwrap_or_else(|| panic!("attempt {attempt_id} started before any policy"));
        assert_eq!(
            *snapshot_id, in_force.0,
            "attempt {attempt_id} must carry the policy in force when it started"
        );
        *per_snapshot.entry(*snapshot_id).or_default() += 1;
    }
    assert_eq!(
        per_snapshot.values().copied().collect::<Vec<_>>(),
        vec![CHANGE_AFTER, SERIES.len() - CHANGE_AFTER]
    );

    // The coverage engine, fed the ledger's own snapshots, owes the new cadence
    // only from the change on, and leaves the span before it at the old one.
    let ledger_snapshots: Vec<PolicySnapshot> = snapshots
        .iter()
        .map(|s| PolicySnapshot {
            effective_at: UtcTimestamp::from_unix_nanos(s.1),
            ordinary_cadence: MonotonicDuration::from_nanos(s.2 as u64),
            retry_backoff_policy: s.3.clone(),
        })
        .collect();
    let owed = |start: i64, end: i64, policies: Vec<PolicySnapshot>| {
        compute(&CoverageInputs {
            interval_start: UtcTimestamp::from_unix_nanos(start),
            interval_end: UtcTimestamp::from_unix_nanos(end),
            policy_snapshots: policies,
            attempts: vec![],
            observations: vec![],
            resets: vec![],
            timer_runs: vec![],
        })
        .expected_opportunities
    };
    let change_at = snapshots[1].1;
    let hour = 3_600 * 1_000_000_000_i64;
    assert_eq!(
        owed(change_at, change_at + hour, ledger_snapshots.clone()),
        Some((3_600 / NEW_CADENCE_SECONDS) as u64),
        "the hour after the change is owed at the new cadence"
    );
    assert_eq!(
        owed(change_at - hour, change_at, ledger_snapshots.clone()),
        owed(change_at - hour, change_at, ledger_snapshots[..1].to_vec()),
        "the span before the change is owed exactly as the old policy alone owed it"
    );
}

#[test]
fn release_criterion_14_the_attempt_is_durable_while_its_request_is_in_flight() {
    let env = SeriesEnvironment::new("in-flight");
    let server = SyntheticServer::start(vec![ScriptedOutcome::HeadersThenStall {
        status: 200,
        headers: vec![],
    }])
    .expect("synthetic server must start");

    let mut child = env
        .command(&server.url(), &["sample", "--due"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("aub must spawn");

    // Event 1: the request has reached the server, and the server is holding it.
    // The bound is generous because a loaded machine can take many seconds to
    // start the binary; a collector that exits instead ends the wait at once.
    let waited = Instant::now();
    while server.request_count() == 0
        && child.try_wait().unwrap().is_none()
        && waited.elapsed() < Duration::from_secs(120)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    if server.request_count() != 1 || child.try_wait().unwrap().is_some() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "the request must be in flight with the collector waiting on it; stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Event 2, read while event 1 is still open: the ledger already holds the
    // committed attempt start, and no result for it.
    let conn = env.read_only();
    let attempts = scalar(&conn, "SELECT count(*) FROM meter_attempt");
    let results = scalar(&conn, "SELECT count(*) FROM meter_attempt_result");
    drop(conn);
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        attempts, 1,
        "the attempt start must be committed before its request left the process"
    );
    assert_eq!(results, 0, "a request still in flight has no result yet");
}
