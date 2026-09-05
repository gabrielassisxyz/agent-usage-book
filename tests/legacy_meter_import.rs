//! Integration tests for legacy meter series import (aub-fon.1, PLAN.md sections 12.6, 32, 33).

use agent_usage_book::config::CoverageFloor;
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::legacy_meter::read_source;
use agent_usage_book::presentation::render_coverage_report;
use agent_usage_book::report::coverage::{
    AccountIdentity, CoverageFloors, CoverageSelector, assemble as assemble_coverage,
};
use agent_usage_book::store::connection::{AccessMode, LEDGER_DATABASE_FILE, PragmaPolicy, open};
use agent_usage_book::store::legacy_meter_import::import;
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
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

fn sample_legacy_jsonl_20_rows() -> &'static str {
    r#"{"ts":"2026-08-15T18:23:29Z","session_id":"sess-01","account":"primary","tier":"default_claude_max_5x","five_hour":7,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:23:30Z","session_id":"sess-02","account":"primary","tier":"default_claude_max_5x","five_hour":7,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:26:47Z","session_id":"sess-03","account":"primary","tier":"default_claude_max_5x","five_hour":7,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:33:40Z","session_id":"sess-04","account":"primary","tier":"default_claude_max_5x","five_hour":8,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:35:17Z","session_id":"sess-05","account":"secondary","tier":"default_claude_ai","five_hour":15,"seven_day":65,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:35:22Z","session_id":"sess-06","account":"primary","tier":"default_claude_max_5x","five_hour":8,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:36:55Z","session_id":"sess-07","account":"primary","tier":"default_claude_max_5x","five_hour":9,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:39:44Z","session_id":"sess-08","account":"secondary","tier":"default_claude_ai","five_hour":16,"seven_day":65,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:39:45Z","session_id":"sess-09","account":"primary","tier":"default_claude_max_5x","five_hour":9,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:40:26Z","session_id":"sess-10","account":"secondary","tier":"default_claude_ai","five_hour":17,"seven_day":65,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:40:54Z","session_id":"sess-11","account":"secondary","tier":"default_claude_ai","five_hour":18,"seven_day":65,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:41:37Z","session_id":"sess-12","account":"primary","tier":"default_claude_max_5x","five_hour":10,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:42:47Z","session_id":"sess-13","account":"primary","tier":"default_claude_max_5x","five_hour":9,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:43:09Z","session_id":"sess-14","account":"secondary","tier":"default_claude_ai","five_hour":19,"seven_day":65,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:43:15Z","session_id":"sess-15","account":"secondary","tier":"default_claude_ai","five_hour":20,"seven_day":66,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:45:31Z","session_id":"sess-16","account":"primary","tier":"default_claude_max_5x","five_hour":11,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:45:43Z","session_id":"sess-17","account":"primary","tier":"default_claude_max_5x","five_hour":11,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:45:43Z","session_id":"sess-18","account":"secondary","tier":"default_claude_ai","five_hour":21,"seven_day":66,"five_resets_at":"1786834200","seven_resets_at":"1787068800"}
{"ts":"2026-08-15T18:46:42Z","session_id":"sess-19","account":"primary","tier":"default_claude_max_5x","five_hour":11,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:46:59Z","session_id":"sess-20","account":"tertiary","tier":"default_claude_ai","five_hour":11,"seven_day":1,"five_resets_at":"1786837200","seven_resets_at":"1787414400"}
"#
}

#[test]
fn integration_importer_run_twice_is_idempotent_asserting_exact_counts() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("source.jsonl");
    std::fs::write(&source_path, sample_legacy_jsonl_20_rows()).unwrap();

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

    assert_eq!(obs_count_1, 20);
    assert_eq!(runs_count_1, 1);
    assert_eq!((attempts_1, term_attempts_1), (20, 20));
    assert_eq!(markers_count_1, 20);

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
fn unit_measurement_basis_assigned_per_timestamp_kind_hook_time_is_not_provider_observed() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("basis.jsonl");
    let jsonl = r#"{"ts":"2026-08-15T18:23:29Z","session_id":"s1","account":"acc","tier":"pro","five_hour":10,"seven_day":20,"five_resets_at":"1786834200","seven_resets_at":"1787148000","timestamp_kind":"hook_time"}
{"ts":"2026-08-15T18:23:30Z","session_id":"s2","account":"acc","tier":"pro","five_hour":10,"seven_day":20,"five_resets_at":"1786834200","seven_resets_at":"1787148000","timestamp_kind":"provider_observed"}
{"ts":"2026-08-15T18:23:31Z","session_id":"s3","account":"acc","tier":"pro","five_hour":10,"seven_day":20,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
"#;
    std::fs::write(&source_path, jsonl).unwrap();
    let parsed = read_source(&source_path).unwrap();
    assert_eq!(
        parsed.records[0].measurement_basis,
        MeasurementBasis::LocallyReceived
    );
    assert_eq!(
        parsed.records[1].measurement_basis,
        MeasurementBasis::ProviderObserved
    );
    assert_eq!(
        parsed.records[2].measurement_basis,
        MeasurementBasis::LocallyReceived
    );

    import(
        &mut conn,
        &parsed,
        "backup-1",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    let bases: Vec<(String, Option<i64>)> = conn
        .prepare("SELECT measurement_basis, provider_observed_at FROM meter_observation ORDER BY received_at")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(bases[0].0, "locally_received");
    assert_eq!(bases[0].1, None);

    assert_eq!(bases[1].0, "provider_observed");
    assert!(bases[1].1.is_some());

    assert_eq!(bases[2].0, "locally_received");
    assert_eq!(bases[2].1, None);
}

#[test]
fn integration_coverage_distinguishes_legacy_evidence_from_live_sampling() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("cov.jsonl");
    std::fs::write(&source_path, sample_legacy_jsonl_20_rows()).unwrap();
    let parsed = read_source(&source_path).unwrap();
    import(
        &mut conn,
        &parsed,
        "backup-1",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    let account_id = account::account_id_by_identity(&conn, "anthropic", "primary")
        .unwrap()
        .expect("primary account must exist");

    let since = UtcTimestamp::parse_rfc3339("2026-08-15T00:00:00Z").unwrap();
    let until = UtcTimestamp::parse_rfc3339("2026-08-16T00:00:00Z").unwrap();

    let live_attempts =
        meter_attempt::attempts_with_outcomes_for_account_between(&conn, account_id, since, until)
            .unwrap();
    assert!(
        live_attempts.is_empty(),
        "legacy imported rows must not be reported as live sampler attempts"
    );

    let floors = CoverageFloors {
        attempt: CoverageFloor::new(0.95).unwrap(),
        measurement: CoverageFloor::new(0.90).unwrap(),
    };
    let report = assemble_coverage(
        &conn,
        since,
        until,
        &CoverageSelector {
            account: None,
            severe_only: false,
        },
        floors,
        until,
        &[AccountIdentity::new("anthropic", "primary")],
    )
    .expect("coverage report must assemble");

    let primary_acct = report
        .accounts
        .iter()
        .find(|a| a.name.as_str() == "primary")
        .unwrap();
    assert!(
        primary_acct.legacy_evidence_present,
        "legacy_evidence_present must be true when legacy observations fall in the window"
    );

    let rendered = render_coverage_report(&report, "24h");
    assert!(
        rendered.contains(
            "legacy observations are shown as historical evidence, not ordinary attempt coverage"
        ),
        "rendered coverage report must distinguish legacy evidence: {rendered}"
    );
}

#[test]
fn unit_plan_tier_and_resets_land_as_observation_evidence_not_mutable_account_columns() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("evidence.jsonl");
    let jsonl = r#"{"ts":"2026-08-15T18:23:29Z","session_id":"sess-tier","account":"primary","tier":"enterprise_5x","five_hour":12,"seven_day":34,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}"#;
    std::fs::write(&source_path, jsonl).unwrap();
    let parsed = read_source(&source_path).unwrap();
    import(
        &mut conn,
        &parsed,
        "backup-1",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    let account_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(account)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        !account_columns
            .iter()
            .any(|c| c.contains("tier") || c.contains("plan") || c.contains("reset")),
        "account table must not have mutable tier/plan/reset columns: {:?}",
        account_columns
    );

    let (observed_plan, observed_tier): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT observed_plan, observed_tier FROM meter_observation WHERE observed_plan = 'enterprise_5x'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(observed_plan.as_deref(), Some("enterprise_5x"));
    assert_eq!(observed_tier.as_deref(), Some("enterprise_5x"));

    let window_resets: Vec<i64> = conn
        .prepare("SELECT resets_at FROM meter_window ORDER BY semantic_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        window_resets,
        vec![1_786_834_200 * 1_000_000_000, 1_787_148_000 * 1_000_000_000,]
    );

    let marker_source: String = conn
        .query_row(
            "SELECT marker_source FROM session_account_marker WHERE session_native = 'sess-tier'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(marker_source, "legacy_meter_series");
}

#[test]
fn integration_sanitized_source_fixture_spot_check_20_rows() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("spot_check.jsonl");
    let jsonl = sample_legacy_jsonl_20_rows();
    std::fs::write(&source_path, jsonl).unwrap();
    let parsed = read_source(&source_path).unwrap();
    assert_eq!(parsed.records.len(), 20);

    import(
        &mut conn,
        &parsed,
        "backup-spot-check",
        UtcTimestamp::from_unix_nanos(100),
    )
    .unwrap();

    for record in &parsed.records {
        let obs_row: (String, Option<String>, i64) = conn
            .query_row(
                "SELECT mo.provider, mo.observed_tier, mo.received_at
                 FROM meter_observation mo
                 JOIN legacy_meter_import_record lir ON lir.observation_id = mo.id
                 WHERE lir.source_line = ?1",
                rusqlite::params![record.source_line as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(obs_row.0, "anthropic");
        assert_eq!(obs_row.1, record.tier);
        assert_eq!(obs_row.2, record.timestamp.unix_nanos());

        let windows: Vec<(String, i64, i64)> = conn
            .prepare(
                "SELECT mw.semantic_key, mw.quota_used_ppm, mw.resets_at
                 FROM meter_window mw
                 JOIN legacy_meter_import_record lir ON lir.observation_id = mw.observation_id
                 WHERE lir.source_line = ?1
                 ORDER BY mw.semantic_key",
            )
            .unwrap()
            .query_map(rusqlite::params![record.source_line as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].0, "five_hour");
        assert_eq!(
            windows[0].1,
            record.windows[0].quota_used.as_ppm().get() as i64
        );
        assert_eq!(windows[0].2, record.windows[0].resets_at.unix_nanos());

        assert_eq!(windows[1].0, "seven_day");
        assert_eq!(
            windows[1].1,
            record.windows[1].quota_used.as_ppm().get() as i64
        );
        assert_eq!(windows[1].2, record.windows[1].resets_at.unix_nanos());

        let marker: (String, String) = conn
            .query_row(
                "SELECT sam.session_native, sam.logical_account
                 FROM session_account_marker sam
                 JOIN legacy_meter_import_record lir ON lir.marker_id = sam.id
                 WHERE lir.source_line = ?1",
                rusqlite::params![record.source_line as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert_eq!(marker.0, record.session_id);
        assert_eq!(marker.1, record.account);
    }

    // Explicitly verify known golden rows from the 20-row fixture
    let row_1_5h: i64 = conn
        .query_row(
            "SELECT mw.quota_used_ppm FROM meter_window mw
             JOIN legacy_meter_import_record lir ON lir.observation_id = mw.observation_id
             WHERE lir.source_line = 1 AND mw.semantic_key = 'five_hour'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row_1_5h, 70_000, "7% used must map to 70_000 ppm");

    let row_5_5h: i64 = conn
        .query_row(
            "SELECT mw.quota_used_ppm FROM meter_window mw
             JOIN legacy_meter_import_record lir ON lir.observation_id = mw.observation_id
             WHERE lir.source_line = 5 AND mw.semantic_key = 'five_hour'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row_5_5h, 150_000, "15% used must map to 150_000 ppm");

    let row_20_7d: i64 = conn
        .query_row(
            "SELECT mw.quota_used_ppm FROM meter_window mw
             JOIN legacy_meter_import_record lir ON lir.observation_id = mw.observation_id
             WHERE lir.source_line = 20 AND mw.semantic_key = 'seven_day'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(row_20_7d, 10_000, "1% used must map to 10_000 ppm");
}

#[test]
fn operational_spot_check_live_persisted_series_when_present() {
    let live_source = std::path::Path::new("/home/gabriel/.local/state/quota-ledger/samples.jsonl");
    if !live_source.exists() {
        return;
    }
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let parsed = read_source(live_source).expect("live legacy quota ledger must parse");
    assert!(
        parsed.records.len() >= 20,
        "live legacy quota ledger must have at least 20 records"
    );

    let summary = import(
        &mut conn,
        &parsed,
        "live-backup-001",
        UtcTimestamp::from_unix_nanos(1_000_000_000),
    )
    .expect("live legacy source must import");
    assert_eq!(summary.imported, parsed.records.len() as u64);
    assert_eq!(summary.quarantined, parsed.records_quarantined);

    // Spot-check first 20 records
    for record in parsed.records.iter().take(20) {
        let obs_row: (String, Option<String>, i64) = conn
            .query_row(
                "SELECT mo.provider, mo.observed_tier, mo.received_at
                 FROM meter_observation mo
                 JOIN legacy_meter_import_record lir ON lir.observation_id = mo.id
                 WHERE lir.source_line = ?1",
                rusqlite::params![record.source_line as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(obs_row.0, "anthropic");
        assert_eq!(obs_row.1, record.tier);
        assert_eq!(obs_row.2, record.timestamp.unix_nanos());

        let windows: Vec<(String, i64, i64)> = conn
            .prepare(
                "SELECT mw.semantic_key, mw.quota_used_ppm, mw.resets_at
                 FROM meter_window mw
                 JOIN legacy_meter_import_record lir ON lir.observation_id = mw.observation_id
                 WHERE lir.source_line = ?1
                 ORDER BY mw.semantic_key",
            )
            .unwrap()
            .query_map(rusqlite::params![record.source_line as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].0, "five_hour");
        assert_eq!(
            windows[0].1,
            record.windows[0].quota_used.as_ppm().get() as i64
        );
        assert_eq!(windows[0].2, record.windows[0].resets_at.unix_nanos());

        assert_eq!(windows[1].0, "seven_day");
        assert_eq!(
            windows[1].1,
            record.windows[1].quota_used.as_ppm().get() as i64
        );
        assert_eq!(windows[1].2, record.windows[1].resets_at.unix_nanos());
    }
}
