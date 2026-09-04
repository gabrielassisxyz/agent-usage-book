//! Integration and unit tests for seed archive import (aub-fon.2, PLAN.md sections 15, 32, 33).

use agent_usage_book::config::CoverageFloor;
use agent_usage_book::domain::attempt::AttemptOutcome;
use agent_usage_book::domain::failure::FailureClass;
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::report::coverage::{
    CoverageFloors, CoverageSelector, assemble as assemble_coverage,
};
use agent_usage_book::seed_archive::{SeedArchiveRecord, read_source};
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::seed_archive_import::import;
use agent_usage_book::store::{account, meter_attempt, meter_evidence, sample_run};
use rusqlite::Connection;
use test_support::StateDir;

fn open_migrated_ledger(state: &StateDir) -> Connection {
    let path = state.path().join(LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&path, AccessMode::ReadWrite, &policy).expect("scratch ledger must open");
    run_migrations(
        &mut conn,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .expect("scratch ledger must migrate");
    conn
}

fn sample_seed_record_json(
    received_at: &str,
    generated_at: &str,
    account: &str,
    five_pct: f64,
    seven_pct: f64,
) -> String {
    format!(
        r#"{{"received_at":"{received_at}","account":"{account}","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{{"generatedAt":"{generated_at}","providers":[{{"provider":"anthropic","windows":[{{"id":"five_hour","percentUsed":{five_pct},"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}},{{"id":"seven_day","percentUsed":{seven_pct},"resetsAt":"2026-09-02T00:00:00Z","windowSeconds":604800}}]}}]}}}}"#
    )
}

fn sample_seed_failure_json(received_at: &str, account: &str, failure_class: &str) -> String {
    format!(
        r#"{{"received_at":"{received_at}","account":"{account}","tool":"aub-meter","tool_version":"0.1.0","failure":"{failure_class}","exit_code":1}}"#
    )
}

fn sample_seed_jsonl_20_rows() -> String {
    let mut lines = Vec::new();
    // 18 successes, 2 failures
    for i in 0..9 {
        let rec = format!("2026-08-26T03:{:02}:00Z", i * 6);
        let gen_time = format!("2026-08-26T03:{:02}:58Z", i * 6);
        lines.push(sample_seed_record_json(
            &rec,
            &gen_time,
            "primary",
            10.0 + i as f64,
            20.0 + i as f64,
        ));
    }
    lines.push(sample_seed_failure_json(
        "2026-08-26T03:54:00Z",
        "primary",
        "spawn_failed",
    ));
    for i in 10..19 {
        let rec = format!("2026-08-26T04:{:02}:00Z", (i - 10) * 6);
        let gen_time = format!("2026-08-26T04:{:02}:58Z", (i - 10) * 6);
        lines.push(sample_seed_record_json(
            &rec,
            &gen_time,
            "primary",
            15.0 + i as f64,
            25.0 + i as f64,
        ));
    }
    lines.push(sample_seed_failure_json(
        "2026-08-26T04:54:00Z",
        "primary",
        "empty_output",
    ));
    lines.join("\n") + "\n"
}

#[test]
fn integration_importer_run_twice_is_idempotent_asserting_exact_counts() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("source.jsonl");
    std::fs::write(&source_path, sample_seed_jsonl_20_rows()).unwrap();

    let parsed = read_source(&source_path).expect("source must parse");
    assert_eq!(parsed.records_read, 20);
    assert_eq!(parsed.records_quarantined, 0);
    assert_eq!(parsed.records.len(), 20);

    let import_time = UtcTimestamp::from_unix_nanos(1_000_000_000);
    let first = import(&mut conn, &parsed, "backup-archive-1", import_time)
        .expect("first import must succeed");
    assert_eq!(first.imported, 20);
    assert_eq!(first.unchanged, 0);
    assert_eq!(first.quarantined, 0);

    let obs_count_1 = meter_evidence::count_observations(&conn).unwrap();
    let runs_count_1 = sample_run::count_sample_runs(&conn).unwrap();
    let (attempts_1, term_attempts_1) = meter_attempt::count_attempts(&conn).unwrap();
    let markers_count_1: i64 = conn
        .query_row("SELECT count(*) FROM session_account_marker", [], |r| {
            r.get(0)
        })
        .unwrap();

    // 18 successes produce 18 observations and 18 markers; all 20 produce attempts
    assert_eq!(obs_count_1, 18);
    assert_eq!(runs_count_1, 1);
    assert_eq!((attempts_1, term_attempts_1), (20, 20));
    assert_eq!(markers_count_1, 18);

    // Re-run over exactly the same source
    let repeated = import(&mut conn, &parsed, "backup-archive-1", import_time)
        .expect("repeated import must succeed");
    assert_eq!(repeated.imported, 0);
    assert_eq!(repeated.unchanged, 20);
    assert_eq!(repeated.quarantined, 0);

    let obs_count_2 = meter_evidence::count_observations(&conn).unwrap();
    let runs_count_2 = sample_run::count_sample_runs(&conn).unwrap();
    let (attempts_2, term_attempts_2) = meter_attempt::count_attempts(&conn).unwrap();
    let markers_count_2: i64 = conn
        .query_row("SELECT count(*) FROM session_account_marker", [], |r| {
            r.get(0)
        })
        .unwrap();

    assert_eq!(obs_count_2, obs_count_1);
    assert_eq!(runs_count_2, runs_count_1);
    assert_eq!((attempts_2, term_attempts_2), (attempts_1, term_attempts_1));
    assert_eq!(markers_count_2, markers_count_1);
}

#[test]
fn integration_sanitized_seed_fixtures_reconcile_success_and_failure_counts() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("fixture.jsonl");
    let jsonl = sample_seed_jsonl_20_rows();
    std::fs::write(&source_path, jsonl).unwrap();

    let parsed = read_source(&source_path).unwrap();
    assert_eq!(parsed.records.len(), 20);

    let summary = import(
        &mut conn,
        &parsed,
        "backup-sanitized-fixture",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    assert_eq!(summary.imported, 20);
    assert_eq!(summary.unchanged, 0);
    assert_eq!(summary.quarantined, 0);

    let success_count = parsed
        .records
        .iter()
        .filter(|r| matches!(r, SeedArchiveRecord::Success(_)))
        .count();
    let failure_count = parsed
        .records
        .iter()
        .filter(|r| matches!(r, SeedArchiveRecord::Failure(_)))
        .count();
    assert_eq!(success_count, 18);
    assert_eq!(failure_count, 2);

    let stored_obs = meter_evidence::count_observations(&conn).unwrap();
    assert_eq!(stored_obs as usize, success_count);

    let (stored_attempts, stored_term) = meter_attempt::count_attempts(&conn).unwrap();
    assert_eq!(stored_attempts as usize, success_count + failure_count);
    assert_eq!(stored_term as usize, success_count + failure_count);

    // Verify session_account_marker has marker_source = "seed_capture"
    let marker_sources: Vec<String> = conn
        .prepare("SELECT DISTINCT marker_source FROM session_account_marker")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(marker_sources, vec!["seed_capture".to_string()]);

    // Verify meter_observation has provider = "anthropic" and observed_plan = "pro"
    let (obs_provider, obs_plan): (String, Option<String>) = conn
        .query_row(
            "SELECT provider, observed_plan FROM meter_observation LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(obs_provider, "anthropic");
    assert_eq!(obs_plan.as_deref(), Some("pro"));

    // Verify failure attempts have sanitized classification starting with "seed_"
    let failure_classifications: Vec<Option<String>> = conn
        .prepare("SELECT sanitized_error_classification FROM meter_attempt_result WHERE outcome != 'success' ORDER BY attempt_id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        failure_classifications,
        vec![
            Some("seed_spawn_failed".to_string()),
            Some("seed_empty_output".to_string()),
        ]
    );
}

#[test]
fn unit_seed_failure_records_import_as_attempted_and_failed_distinguishable_from_no_capture() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("failure_distinction.jsonl");

    // Line 1: success at 03:00
    // Line 2: failure at 03:06
    // Gap: no capture at 03:12
    let jsonl = format!(
        "{}\n{}\n",
        sample_seed_record_json(
            "2026-08-26T03:00:00Z",
            "2026-08-26T02:59:58Z",
            "primary",
            10.0,
            20.0
        ),
        sample_seed_failure_json("2026-08-26T03:06:00Z", "primary", "spawn_failed"),
    );
    std::fs::write(&source_path, jsonl).unwrap();

    let parsed = read_source(&source_path).unwrap();
    import(
        &mut conn,
        &parsed,
        "backup-fail-check",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    let account_id = account::account_id_by_identity(&conn, "anthropic", "primary")
        .unwrap()
        .expect("primary account must exist");

    let since = UtcTimestamp::parse_rfc3339("2026-08-26T03:00:00Z").unwrap();
    let until = UtcTimestamp::parse_rfc3339("2026-08-26T03:18:00Z").unwrap();

    let attempts =
        meter_attempt::attempts_with_outcomes_for_account_between(&conn, account_id, since, until)
            .unwrap();
    assert_eq!(attempts.len(), 2, "must have exactly 2 attempts recorded");
    assert_eq!(
        attempts[0].terminal.as_ref().map(|t| t.outcome),
        Some(AttemptOutcome::Success)
    );
    assert!(
        matches!(
            attempts[1].terminal.as_ref().map(|t| t.outcome),
            Some(AttemptOutcome::Unreachable(
                FailureClass::ReadTimeout | FailureClass::ConnectTimeout
            ))
        ),
        "failure record must import as unreachable attempt"
    );

    let observations =
        meter_evidence::observation_times_for_account_between(&conn, account_id, since, until)
            .unwrap();
    assert_eq!(
        observations.len(),
        1,
        "only successful capture yields an observation"
    );

    // Assemble coverage report over this 18-minute window (3 expected opportunities at 6m cadence: 03:00, 03:06, 03:12)
    let floors = CoverageFloors {
        attempt: CoverageFloor::new(0.50).unwrap(),
        measurement: CoverageFloor::new(0.30).unwrap(),
    };
    let report = assemble_coverage(
        &conn,
        since,
        until,
        &CoverageSelector {
            account: Some("primary".to_string()),
            severe_only: false,
        },
        floors,
        until,
    )
    .unwrap();

    let primary_acct = &report.accounts[0];
    let engine_rep = &primary_acct.engine;
    assert_eq!(engine_rep.expected_opportunities, Some(3));
    assert_eq!(engine_rep.attempted_opportunities, 2);
    assert_eq!(engine_rep.successful_observations, 1);
    // Failure tally accounts for the failed attempt
    assert_eq!(primary_acct.failures.provider_unreachable, 1);
}

#[test]
fn unit_declared_measurement_basis_matches_seed_format_document() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("basis_check.jsonl");

    let rec_ts = "2026-08-26T03:00:05Z";
    let gen_ts = "2026-08-26T02:59:58Z";
    std::fs::write(
        &source_path,
        sample_seed_record_json(rec_ts, gen_ts, "primary", 12.0, 34.0) + "\n",
    )
    .unwrap();

    let parsed = read_source(&source_path).unwrap();
    assert_eq!(
        parsed.declared_measurement_basis,
        MeasurementBasis::ProviderObserved
    );

    import(
        &mut conn,
        &parsed,
        "backup-basis",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    let (basis, provider_obs_at, received_at): (String, Option<i64>, i64) = conn
        .query_row(
            "SELECT measurement_basis, provider_observed_at, received_at FROM meter_observation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();

    let expected_gen = UtcTimestamp::parse_rfc3339(gen_ts).unwrap().unix_nanos();
    let expected_rec = UtcTimestamp::parse_rfc3339(rec_ts).unwrap().unix_nanos();

    assert_eq!(
        basis, "provider_observed",
        "declared basis must be provider_observed"
    );
    assert_eq!(
        provider_obs_at,
        Some(expected_gen),
        "generatedAt must be stored as provider_observed_at"
    );
    assert_eq!(received_at, expected_rec, "received_at must be preserved");
    assert_ne!(
        provider_obs_at.unwrap(),
        received_at,
        "provider_observed_at and received_at must be distinct"
    );
}

#[test]
fn unit_malformed_or_partial_trailing_record_quarantined_with_reason() {
    let state = StateDir::new();
    let source_path = state.path().join("quarantine.jsonl");

    let line1 = sample_seed_record_json(
        "2026-08-26T03:00:00Z",
        "2026-08-26T02:59:58Z",
        "primary",
        10.0,
        20.0,
    );
    let line2 = "not valid json in the middle";
    let line3 = sample_seed_failure_json("2026-08-26T03:06:00Z", "primary", "spawn_failed");
    let line4 = "{\"received_at\":\"2026-08-26T03:12:00Z\",\"account\":\"primary\""; // partial trailing line

    let content = format!("{line1}\n{line2}\n{line3}\n{line4}");
    std::fs::write(&source_path, content).unwrap();

    let parsed = read_source(&source_path).unwrap();
    assert_eq!(parsed.records_read, 4);
    assert_eq!(parsed.records.len(), 2);
    assert_eq!(parsed.records_quarantined, 2);
    assert_eq!(parsed.quarantined.len(), 2);

    assert_eq!(parsed.quarantined[0].source_line, 2);
    assert_eq!(parsed.quarantined[0].reason, "invalid_json");

    assert_eq!(parsed.quarantined[1].source_line, 4);
    assert_eq!(parsed.quarantined[1].reason, "partial_trailing_line");
}

#[test]
fn integration_coverage_over_seed_interval_uses_seed_cadence_as_denominator() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("seed_cadence.jsonl");

    // Create 72 hours of records at 6-minute nominal cadence: exactly 720 records
    let mut lines = Vec::new();
    for h in 0..72 {
        let day = 26 + h / 24;
        let hour = h % 24;
        for m in (0..60).step_by(6) {
            let ts_str = format!("2026-08-{:02}T{:02}:{:02}:00Z", day, hour, m);
            lines.push(sample_seed_record_json(
                &ts_str, &ts_str, "primary", 10.0, 20.0,
            ));
        }
    }
    std::fs::write(&source_path, lines.join("\n") + "\n").unwrap();

    let parsed = read_source(&source_path).unwrap();
    assert_eq!(parsed.records.len(), 720);

    import(
        &mut conn,
        &parsed,
        "backup-cadence",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    let since = UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap();
    let until = UtcTimestamp::parse_rfc3339("2026-08-29T00:00:00Z").unwrap(); // Exactly 72 hours

    let floors = CoverageFloors {
        attempt: CoverageFloor::new(0.95).unwrap(),
        measurement: CoverageFloor::new(0.90).unwrap(),
    };
    let report = assemble_coverage(
        &conn,
        since,
        until,
        &CoverageSelector {
            account: Some("primary".to_string()),
            severe_only: false,
        },
        floors,
        until,
    )
    .unwrap();

    let primary_acct = &report.accounts[0];
    let engine_rep = &primary_acct.engine;

    // 72 hours * 60 min / 6 min nominal = 720 expected opportunities
    // Under 5-min ordinary cadence this would have been 72 * 60 / 5 = 864
    assert_eq!(
        engine_rep.expected_opportunities,
        Some(720),
        "seed interval coverage must use 6-minute nominal cadence as denominator (720), not default 5-min (864)"
    );
    assert_eq!(engine_rep.attempted_opportunities, 720);
    assert_eq!(engine_rep.successful_observations, 720);
    assert_eq!(engine_rep.attempt_coverage.map(|f| f.as_f64()), Some(1.0));
    assert_eq!(
        engine_rep.measurement_coverage.map(|f| f.as_f64()),
        Some(1.0)
    );
}

#[test]
fn operational_reconcile_real_seed_archive_counts() {
    let archive_dir = std::path::Path::new("/home/gabriel/.local/state/aub-meter");
    if !archive_dir.is_dir() {
        return;
    }

    let parsed = read_source(archive_dir).expect("real seed archive must parse");
    assert!(
        parsed.records.len() > 0,
        "seed archive must contain records"
    );

    // Reconcile over the 72h window from aub-d41.3:
    // 2026-08-26T02:55:36Z to 2026-08-29T02:55:36Z
    let window_start = UtcTimestamp::parse_rfc3339("2026-08-26T02:55:36Z").unwrap();
    let window_end = UtcTimestamp::parse_rfc3339("2026-08-29T02:55:36Z").unwrap();

    let records_in_window: Vec<_> = parsed
        .records
        .iter()
        .filter(|r| {
            let at = r.received_at();
            at >= window_start && at <= window_end
        })
        .collect();

    let failures_in_window = records_in_window
        .iter()
        .filter(|r| matches!(r, SeedArchiveRecord::Failure(_)))
        .count();

    println!(
        "operational reconciliation: {} records in 72h window, {} failures (total archive: {} records, {} quarantined)",
        records_in_window.len(),
        failures_in_window,
        parsed.records.len(),
        parsed.records_quarantined,
    );

    // Matches verification bead aub-d41.3: 775 records in 72h window (or 776 with inclusive endpoint), 0 failures
    assert!(
        records_in_window.len() == 775 || records_in_window.len() == 776,
        "records count in 72h window must match seed verification bead aub-d41.3 (775 or 776, got {})",
        records_in_window.len()
    );
    assert_eq!(
        failures_in_window, 0,
        "failure count in 72h window must match seed verification bead aub-d41.3 (0)"
    );
}
