//! End-to-end proof for `aub-eun.14`: the release `aub` binary, invoked
//! against a real synthetic HTTP server over a real socket, four times in a
//! row against one persistent state directory, produces the two anomaly
//! classes, accepts a legitimate reset, and leaves both stdout and the
//! diagnostic log naming the anomaly. This is the same shape
//! `tests/sample_exit.rs` (`aub-eun.6`) already uses to prove exit-status
//! semantics against the compiled binary rather than a library call; this
//! file adds the sequence a single invocation cannot exercise, since a
//! consecutive-window comparison needs two observations across two runs.
//!
//! Sequence, one HTTP response per `aub sample` invocation, all for the
//! `five_hour` window (the `seven_day` window is held constant throughout, so
//! its own rows prove adjacent clean intervals never receive an
//! anomaly-derived annotation):
//!
//! 1. `five_hour` 60% used, resets at `T1` (baseline, no prior observation to
//!    compare against).
//! 2. `five_hour` 40% used (a decrease), resets still `T1`, not yet due:
//!    `percentage_decrease_without_reset`.
//! 3. `five_hour` 40% used (no decrease), resets moved to `T3`, `T1` still not
//!    due: `unexpected_reset_change`.
//! 4. Sent after real time passes `T3`: `five_hour` 10% used, resets moved to
//!    `T4` (forward of `T3`): a legitimate reset, no anomaly.
//!
//! `T1`/`T3` are real wall-clock instants a few seconds ahead of the test's
//! own start, because the production measurement instant for this adapter is
//! `MeasurementBasis::LocallyReceived` (`src/meter/anthropic.rs`): the real
//! binary's `RealClock` is what decides whether a boundary is due, so proving
//! this end to end means letting real time cross it, not injecting a fake one
//! the binary has no seam for.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use test_support::synthetic_server::SyntheticServer;
use test_support::synthetic_server::script::{ScriptedOutcome, ScriptedResponseBody};

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

/// Days since the Unix epoch to a proleptic-Gregorian `(year, month, day)`,
/// Howard Hinnant's civil-calendar algorithm - the same one
/// `agent_usage_book::domain::time::UtcDate` uses, reproduced here because a
/// black-box binary test builds its fixture bodies from outside the crate,
/// with no access to the crate's own (test-only) formatting.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Formats a Unix-epoch instant as the RFC 3339 shape the Anthropic adapter's
/// `resets_at` parser accepts (`UtcTimestamp::parse_rfc3339`).
fn rfc3339(unix_nanos: i64) -> String {
    let total_seconds = unix_nanos.div_euclid(1_000_000_000);
    let days = total_seconds.div_euclid(86_400);
    let seconds_of_day = total_seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the epoch")
        .as_nanos() as i64
}

/// One synthetic Anthropic usage body: `five_hour` at `five_hour_pct` percent
/// used with the given reset instant, and a `seven_day` window held constant
/// throughout the whole sequence.
const SEVEN_DAY_RESETS_AT: &str = "2027-01-01T00:00:00Z";

fn anthropic_body(five_hour_pct: f64, five_hour_resets_at: &str) -> Vec<u8> {
    format!(
        r#"{{"five_hour":{{"utilization":{five_hour_pct},"resets_at":"{five_hour_resets_at}"}},"seven_day":{{"utilization":20.0,"resets_at":"{SEVEN_DAY_RESETS_AT}"}}}}"#
    )
    .into_bytes()
}

/// A body identical in its two required account-wide windows across the
/// whole test (so neither ever triggers an anomaly of its own), plus an
/// optional `seven_day_<model>` key: the one window shape this adapter can
/// make appear or disappear between two responses, since `five_hour` and
/// `seven_day` are both required fields and can never be absent from a
/// parseable response.
fn anthropic_body_with_optional_model(model_window: bool) -> Vec<u8> {
    const FIVE_HOUR_RESETS_AT: &str = "2027-01-01T00:00:00Z";
    let model_field = if model_window {
        r#","seven_day_model_a":{"utilization":30.0,"resets_at":"2027-02-01T00:00:00Z"}"#
    } else {
        ""
    };
    format!(
        r#"{{"five_hour":{{"utilization":50.0,"resets_at":"{FIVE_HOUR_RESETS_AT}"}},"seven_day":{{"utilization":20.0,"resets_at":"{SEVEN_DAY_RESETS_AT}"}}{model_field}}}"#
    )
    .into_bytes()
}

struct Environment {
    root: PathBuf,
}

impl Environment {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "aub-window-anomaly-e2e-{tag}-{}",
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
}

/// Reads one integer aggregate from the ledger, opened read-only, so the test
/// never contends with a running invocation for the write lock.
fn scalar(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn decreasing_percentage_changed_reset_and_legitimate_reset_sequence() {
    let env = Environment::new("sequence");

    let t0 = now_nanos();
    // Comfortably ahead of the three fast requests that come first, and short
    // enough that the fourth request's real sleep stays a matter of seconds.
    let t1 = rfc3339(t0 + 20_000_000_000);
    let t3 = rfc3339(t0 + 25_000_000_000);
    let t4 = rfc3339(t0 + 40_000_000_000);

    let server = SyntheticServer::start(vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(anthropic_body(60.0, &t1))),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(anthropic_body(40.0, &t1))),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(anthropic_body(40.0, &t3))),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(anthropic_body(10.0, &t4))),
    ])
    .expect("synthetic server must start");

    let args = ["sample", "--account", "work-primary", "--require-success"];

    // 1. Baseline observation: nothing to compare against yet, no anomaly.
    let (status, stdout, stderr) = env.run(&server.url(), &args);
    assert_eq!(status, 0, "baseline sample must succeed; stderr: {stderr}");
    assert!(
        !stdout.contains("window anomaly"),
        "the first observation has no prior reading to compare against: {stdout}"
    );
    assert!(!stderr.contains("meter_window_anomaly_detected"));

    // 2. A decrease with the reset instant unchanged and not yet due.
    let (status, stdout, stderr) = env.run(&server.url(), &args);
    assert_eq!(
        status, 0,
        "sample must still exit 0 on an anomaly; stderr: {stderr}"
    );
    assert!(
        stdout.contains("window anomaly: kind=percentage_decrease_without_reset"),
        "stdout must name the decrease anomaly: {stdout}"
    );
    assert!(
        stderr.contains("\"event\":\"meter_window_anomaly_detected\"")
            && stderr.contains("\"kind\":\"percentage_decrease_without_reset\""),
        "the diagnostic log must carry the typed event: {stderr}"
    );

    // 3. No decrease, but the reset instant changes without being due.
    let (status, stdout, stderr) = env.run(&server.url(), &args);
    assert_eq!(status, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("window anomaly: kind=unexpected_reset_change"),
        "stdout must name the reset-change anomaly: {stdout}"
    );
    assert!(
        stderr.contains("\"event\":\"meter_window_anomaly_detected\"")
            && stderr.contains("\"kind\":\"unexpected_reset_change\""),
        "the diagnostic log must carry the typed event: {stderr}"
    );

    // Let real time cross T3 before the fourth request, so the boundary is
    // genuinely due when the binary's own RealClock reads it.
    let elapsed = now_nanos() - t0;
    let remaining_nanos = 26_000_000_000 - elapsed;
    if remaining_nanos > 0 {
        std::thread::sleep(Duration::from_nanos(remaining_nanos as u64));
    }

    // 4. A decrease explained by a legitimate, forward-moving reset.
    let (status, stdout, stderr) = env.run(&server.url(), &args);
    assert_eq!(status, 0, "stderr: {stderr}");
    assert!(
        !stdout.contains("window anomaly"),
        "a legitimate boundary reset must not be flagged: {stdout}"
    );
    assert!(!stderr.contains("meter_window_anomaly_detected"));

    // The ledger: four observations, eight windows (two per observation), and
    // exactly the two anomalies plus their exclusions - the seven_day window
    // stayed constant throughout and carries neither.
    let conn = rusqlite::Connection::open(env.db_path()).expect("ledger must open");
    assert_eq!(scalar(&conn, "SELECT count(*) FROM meter_observation"), 4);
    assert_eq!(scalar(&conn, "SELECT count(*) FROM meter_window"), 8);
    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM meter_window_anomaly"),
        2
    );
    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM meter_calibration_exclusion"),
        2
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM meter_window_anomaly WHERE kind = 'percentage_decrease_without_reset'"
        ),
        1
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM meter_window_anomaly WHERE kind = 'unexpected_reset_change'"
        ),
        1
    );
    assert_eq!(
        scalar(
            &conn,
            "SELECT count(*) FROM meter_window_anomaly WHERE semantic_key = 'seven_day'"
        ),
        0,
        "the constant seven_day window must never be flagged"
    );

    // The original observations are unmutated and queryable: the very first
    // five_hour reading still reads 600000 ppm (60%) after three later
    // observations and two anomaly detections against it.
    let first_used_ppm: i64 = conn
        .query_row(
            "SELECT mw.quota_used_ppm FROM meter_window mw
             JOIN meter_observation mo ON mo.id = mw.observation_id
             WHERE mw.semantic_key = 'five_hour' ORDER BY mo.id ASC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(first_used_ppm, 600_000);

    // doctor consumes the persisted evidence without reimplementing detection.
    // `aub doctor` is a report, not a gate: it always exits 0, and a failed
    // check is a line in the report rather than a nonzero status.
    let (doctor_status, doctor_stdout, doctor_stderr) = env.run(&server.url(), &["doctor"]);
    assert_eq!(doctor_status, 0, "stderr: {doctor_stderr}");
    assert!(
        doctor_stdout.contains("[FAIL] meter-anomalies"),
        "doctor must fail the meter-anomalies check: {doctor_stdout}"
    );
    assert!(doctor_stdout.contains("percentage_decrease_without_reset"));
    assert!(doctor_stdout.contains("unexpected_reset_change"));
}

/// The one window-set-evolution direction the current Anthropic adapter can
/// actually produce end to end: `five_hour` and `seven_day` are both required
/// fields the adapter refuses a whole response without, so an account-wide
/// window can never newly appear through this adapter today. A
/// `seven_day_<model>` key is optional and dynamic, so its disappearance
/// between two responses is the reachable case, and this proves it through
/// the real binary rather than only through `store::window_anomaly`'s own
/// fixtures.
#[test]
fn a_disappearing_model_specific_window_persists_its_typed_classification() {
    let env = Environment::new("window-set-evolution");
    let server = SyntheticServer::start(vec![
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(
            anthropic_body_with_optional_model(true),
        )),
        ScriptedOutcome::Success(ScriptedResponseBody::json_ok(
            anthropic_body_with_optional_model(false),
        )),
    ])
    .expect("synthetic server must start");
    let args = ["sample", "--account", "work-primary", "--require-success"];

    let (status, _stdout, stderr) = env.run(&server.url(), &args);
    assert_eq!(status, 0, "stderr: {stderr}");
    let (status, _stdout, stderr) = env.run(&server.url(), &args);
    assert_eq!(status, 0, "stderr: {stderr}");

    let conn = rusqlite::Connection::open(env.db_path()).expect("ledger must open");
    // No anomaly: the two required windows never changed, and a window-set
    // change is not itself an anomaly.
    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM meter_window_anomaly"),
        0
    );
    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM meter_window_set_change"),
        1
    );
    let kind: String = conn
        .query_row("SELECT kind FROM meter_window_set_change", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(kind, "missing_model_specific_window");
    let semantic_key: String = conn
        .query_row(
            "SELECT semantic_key FROM meter_window_set_change",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(semantic_key, "seven_day_model_a");

    // Neither observation's windows were touched: the first observation's
    // model-specific reading is still there, immutable and queryable.
    let model_window_count: i64 = scalar(
        &conn,
        "SELECT count(*) FROM meter_window WHERE semantic_key = 'seven_day_model_a'",
    );
    assert_eq!(model_window_count, 1);
}
