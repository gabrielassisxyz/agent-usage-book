//! Integration tests for legacy meter series import (aub-fon.1, PLAN.md sections 12.6, 32, 33).

use agent_usage_book::backup::{BackupSummary, create_archive, verify_archive};
use agent_usage_book::config::CoverageFloor;
use agent_usage_book::domain::time::{
    FakeClock, MeasurementBasis, MonotonicDuration, UtcTimestamp,
};
use agent_usage_book::legacy_meter::read_source;
use agent_usage_book::presentation::{Style, render_coverage_report};
use agent_usage_book::report::coverage::{
    AccountIdentity, CoverageFloors, CoverageSelector, assemble as assemble_coverage,
};
use agent_usage_book::store::connection::{LEDGER_DATABASE_FILE, PragmaPolicy};
use agent_usage_book::store::legacy_meter_import::import;
use agent_usage_book::store::{account, meter_attempt, meter_evidence, sample_run};
use rusqlite::Connection;
use test_support::StateDir;

fn open_migrated_ledger(state: &StateDir) -> Connection {
    let path = state.path().join(LEDGER_DATABASE_FILE);
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    // The schema comes from the cross-process template cache (aub-yr9c) instead
    // of a per-fixture migration replay.
    test_support::open_migrated(&path, &policy)
}

fn verified_legacy_meter_archive(state: &StateDir) -> BackupSummary {
    let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(1_000_000_000));
    let timeout = MonotonicDuration::from_millis(1000);
    let archive = create_archive(
        state.path(),
        &state.path().join("verified-archive"),
        timeout,
        &clock,
    )
    .expect("legacy import fixture backup must be created");
    assert!(archive.verified, "legacy import fixture backup must verify");
    let verified = verify_archive(&archive.destination, timeout, &clock)
        .expect("legacy import fixture backup must independently verify");
    assert!(verified.verified);
    archive
}

struct LegacyMeterBackupFixture {
    state: StateDir,
    conn: Connection,
    archive: std::path::PathBuf,
    source: std::path::PathBuf,
}

impl LegacyMeterBackupFixture {
    fn new() -> Self {
        let state = StateDir::new();
        let mut conn = open_migrated_ledger(&state);
        let source = state.path().join("source.jsonl");
        let existing_row = sample_legacy_jsonl_20_rows()
            .lines()
            .next()
            .unwrap()
            .replace("sess-01", "existing-session");
        std::fs::write(&source, existing_row).unwrap();
        import(
            &mut conn,
            &read_source(&source).unwrap(),
            &sample_configured_accounts(),
            "previous-backup",
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();
        let archive = verified_legacy_meter_archive(&state).destination;
        std::fs::write(&source, sample_legacy_jsonl_20_rows()).unwrap();
        std::fs::write(
            state.path().join("config.toml"),
            sample_configured_accounts()
                .iter()
                .map(|name| format!("[[accounts]]\nname = \"{name}\"\nprovider = \"anthropic\"\n"))
                .collect::<String>(),
        )
        .unwrap();
        Self {
            state,
            conn,
            archive,
            source,
        }
    }

    fn truncate_archive(&self) {
        let database = std::fs::OpenOptions::new()
            .write(true)
            .open(
                self.archive
                    .join(agent_usage_book::backup::ARCHIVE_DATABASE_FILE),
            )
            .unwrap();
        let original_length = database.metadata().unwrap().len();
        assert!(original_length > 0);
        database.set_len(original_length / 2).unwrap();
    }

    fn import(&self) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_aub"))
            .env("HOME", self.state.path().join("home"))
            .env("AUB_STATE_DIR", self.state.path())
            .env("AUB_CONFIG_FILE", self.state.path().join("config.toml"))
            .args(["import", "legacy-meter", "--source"])
            .arg(&self.source)
            .arg("--backup")
            .arg(&self.archive)
            .output()
            .expect("legacy import command must run")
    }

    fn row_counts(&self) -> (i64, i64, i64) {
        self.conn
            .query_row(
                "SELECT (SELECT count(*) FROM legacy_meter_import),
                        (SELECT count(*) FROM meter_attempt),
                        (SELECT count(*) FROM session_account_marker)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }
}

#[test]
fn cli_import_accepts_verified_backup() {
    let fixture = LegacyMeterBackupFixture::new();
    assert_eq!(fixture.row_counts(), (1, 1, 1));
    let output = fixture.import();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("imported=20"));
    assert_eq!(fixture.row_counts(), (2, 21, 21));
}

#[test]
fn cli_import_rejects_truncated_backup_with_store_error() {
    let fixture = LegacyMeterBackupFixture::new();
    fixture.truncate_archive();
    let output = fixture.import();
    assert_eq!(output.status.code(), Some(5), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("legacy import requires a verified backup archive"),
        "missing verified-backup refusal: {stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn cli_refusal_preserves_legacy_meter_row_counts() {
    let fixture = LegacyMeterBackupFixture::new();
    fixture.truncate_archive();
    let before = fixture.row_counts();
    assert_eq!(before, (1, 1, 1));
    let output = fixture.import();
    assert_eq!(
        fixture.row_counts(),
        before,
        "refused import changed legacy_meter_import, meter_attempt or session_account_marker"
    );
    assert_eq!(output.status.code(), Some(5), "{output:?}");
}

/// The accounts the 20-row fixture names, as a configuration would declare
/// them. The importer quarantines anything else, so a fixture that means to
/// import must say which names are configured.
fn sample_configured_accounts() -> Vec<String> {
    vec![
        "primary".to_owned(),
        "secondary".to_owned(),
        "tertiary".to_owned(),
    ]
}

/// Every account a parsed source names, for the cases whose subject is not the
/// configured-account rule.
fn configured_accounts_of(
    source: &agent_usage_book::legacy_meter::ParsedLegacyMeterSource,
) -> Vec<String> {
    let mut names: Vec<String> = source
        .records
        .iter()
        .map(|record| record.account.clone())
        .collect();
    names.sort();
    names.dedup();
    names
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
    let backup = verified_legacy_meter_archive(&state);
    let backup_id = format!(
        "archive-v{}-g{}",
        backup.schema_version, backup.ledger_generation
    );
    let first = import(
        &mut conn,
        &parsed,
        &sample_configured_accounts(),
        &backup_id,
        import_time,
    )
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
    let repeated = import(
        &mut conn,
        &parsed,
        &sample_configured_accounts(),
        &backup_id,
        import_time,
    )
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
        &configured_accounts_of(&parsed),
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
        &configured_accounts_of(&parsed),
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

    let rendered = render_coverage_report(&report, "24h", Style::plain());
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
        &configured_accounts_of(&parsed),
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
        &configured_accounts_of(&parsed),
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
        &configured_accounts_of(&parsed),
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

/// Plants one native meter attempt for `account` at `started_at`, under a
/// contract that is neither legacy one. It is the cutoff the importer must
/// then respect for that account and for no other.
fn plant_native_attempt(
    conn: &rusqlite::Connection,
    account_name: &str,
    started_at: UtcTimestamp,
) -> agent_usage_book::store::account::AccountId {
    use agent_usage_book::store::meter_attempt::{DueReason, NewMeterAttempt};
    use agent_usage_book::store::sample_run::{Trigger, start_sample_run};
    use agent_usage_book::store::sampling_policy_snapshot::{
        ResolvedSamplingPolicy, resolve_policy_snapshot,
    };

    let account_id = account::observe_account(conn, "anthropic", account_name, started_at).unwrap();
    let run = start_sample_run(conn, Trigger::Timer, started_at, "native-fixture-v1").unwrap();
    // The policy snapshot is effective an hour before the attempt: a snapshot
    // is unique per (account, instant), and the legacy importer resolves its
    // own policy at each reading's timestamp. An instant any fixture reading
    // also uses would make the fixture collide with the subject instead of
    // measuring it.
    let policy = resolve_policy_snapshot(
        conn,
        account_id,
        UtcTimestamp::from_unix_nanos(started_at.unix_nanos() - 3_600_000_000_000),
        &ResolvedSamplingPolicy {
            ordinary_cadence: MonotonicDuration::from_seconds(600),
            freshness_horizon: MonotonicDuration::from_seconds(600),
            reset_edge_policy: "native-fixture".to_owned(),
            retry_backoff_policy: "native-fixture".to_owned(),
            command_budget: MonotonicDuration::from_seconds(5),
            policy_algorithm_version: "native-fixture-v1".to_owned(),
        },
    )
    .unwrap();
    meter_attempt::start_meter_attempt(
        conn,
        &NewMeterAttempt {
            run_id: run,
            account_id,
            provider: "anthropic".to_owned(),
            request_started_at: started_at,
            credential_context_id: None,
            policy_snapshot_id: policy,
            due_at: started_at,
            due_reason: DueReason::ForcedOrManual,
            due_basis: None,
            provider_contract_id: "anthropic-usage-endpoint-v1".to_owned(),
            meter_semantics_id: "native-account-windows-v1".to_owned(),
        },
    )
    .unwrap();
    account_id
}

/// The cutoff instant the straddling fixture is built around, and the three
/// readings that straddle it: one second before, exactly on it, and after.
const STRADDLE_CUTOFF: &str = "2026-09-04T12:00:00Z";

fn straddling_source_jsonl() -> &'static str {
    r#"{"ts":"2026-09-04T11:59:59Z","session_id":"straddle-before","account":"primary","tier":"pro","five_hour":10,"seven_day":20,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
{"ts":"2026-09-04T12:00:00Z","session_id":"straddle-at","account":"primary","tier":"pro","five_hour":11,"seven_day":21,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
{"ts":"2026-09-04T12:00:01Z","session_id":"straddle-after","account":"primary","tier":"pro","five_hour":12,"seven_day":22,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
"#
}

fn straddling_fixture() -> (
    StateDir,
    Connection,
    agent_usage_book::legacy_meter::ParsedLegacyMeterSource,
) {
    let state = StateDir::new();
    let conn = open_migrated_ledger(&state);
    plant_native_attempt(
        &conn,
        "primary",
        UtcTimestamp::parse_rfc3339(STRADDLE_CUTOFF).unwrap(),
    );
    let source_path = state.path().join("straddle.jsonl");
    std::fs::write(&source_path, straddling_source_jsonl()).unwrap();
    let parsed = read_source(&source_path).unwrap();
    (state, conn, parsed)
}

fn legacy_observation_count(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM legacy_meter_import_record",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn marker_sessions(conn: &Connection) -> Vec<String> {
    let mut sessions: Vec<String> = conn
        .prepare("SELECT session_native FROM session_account_marker WHERE marker_source = 'legacy_meter_series'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    sessions.sort();
    sessions
}

/// The cutoff is exclusive of itself: a reading one second before the earliest
/// native attempt imports, and a reading at exactly that instant does not.
#[test]
fn integration_straddling_source_imports_only_its_uncovered_prefix() {
    let (_state, mut conn, parsed) = straddling_fixture();
    let summary = import(
        &mut conn,
        &parsed,
        &["primary".to_owned()],
        "backup-straddle",
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .unwrap();

    assert_eq!(summary.imported, 1, "only the reading before T may import");
    assert_eq!(
        summary.superseded_by_native, 2,
        "the reading at T and the one after it are superseded"
    );
    assert_eq!(summary.unchanged, 0);
    assert_eq!(summary.quarantined, 0);
    assert_eq!(legacy_observation_count(&conn), 1);

    let imported_sessions: Vec<String> = conn
        .prepare(
            "SELECT sam.session_native FROM session_account_marker sam
             JOIN legacy_meter_import_record lir ON lir.marker_id = sam.id",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        imported_sessions,
        vec!["straddle-before".to_owned()],
        "the observation that imported must be the one before T"
    );
}

/// Markers are exempt from the cutoff: every configured line contributes one,
/// on both sides of T, because native sampling produces no such evidence.
#[test]
fn integration_markers_import_across_the_native_cutoff() {
    let (_state, mut conn, parsed) = straddling_fixture();
    import(
        &mut conn,
        &parsed,
        &["primary".to_owned()],
        "backup-straddle",
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .unwrap();

    assert_eq!(
        marker_sessions(&conn),
        vec![
            "straddle-after".to_owned(),
            "straddle-at".to_owned(),
            "straddle-before".to_owned(),
        ],
        "every source line with a session id and a configured account keeps its marker"
    );
}

/// A cutoff belongs to the account it was measured on. A second configured
/// account the sampler has never reached imports its whole series.
#[test]
fn integration_the_cutoff_is_per_account_not_global() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    plant_native_attempt(
        &conn,
        "primary",
        UtcTimestamp::parse_rfc3339(STRADDLE_CUTOFF).unwrap(),
    );
    let source_path = state.path().join("two-accounts.jsonl");
    let jsonl =
        straddling_source_jsonl().replace("\"account\":\"primary\"", "\"account\":\"gmail\"");
    std::fs::write(
        &source_path,
        format!("{}{}", straddling_source_jsonl(), jsonl),
    )
    .unwrap();
    let parsed = read_source(&source_path).unwrap();

    let summary = import(
        &mut conn,
        &parsed,
        &["primary".to_owned(), "gmail".to_owned()],
        "backup-two-accounts",
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .unwrap();
    assert_eq!(
        summary.imported, 4,
        "primary contributes its one uncovered reading, gmail all three"
    );
    assert_eq!(summary.superseded_by_native, 2);
}

/// An account no `[[accounts]]` entry names is quarantined with its line
/// number, and creates no account row. Its twin differs only in the account
/// name and imports in full.
#[test]
fn integration_an_unconfigured_account_is_quarantined_and_creates_no_account_row() {
    use agent_usage_book::store::ingest_quarantine::load_all_quarantine;

    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let source_path = state.path().join("unconfigured.jsonl");
    let jsonl = r#"{"ts":"2026-08-15T18:23:29Z","session_id":"twin","account":"primary","tier":"pro","five_hour":7,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
{"ts":"2026-08-15T18:23:29Z","session_id":"twin","account":"file-auth:pro","tier":"pro","five_hour":7,"seven_day":63,"five_resets_at":"1786834200","seven_resets_at":"1787148000"}
"#;
    std::fs::write(&source_path, jsonl).unwrap();
    let parsed = read_source(&source_path).unwrap();

    let summary = import(
        &mut conn,
        &parsed,
        &["primary".to_owned()],
        "backup-unconfigured",
        UtcTimestamp::from_unix_nanos(5_000),
    )
    .unwrap();
    assert_eq!(summary.imported, 1, "the configured twin must import");
    assert_eq!(summary.quarantined, 1);

    let rows = load_all_quarantine(&conn).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].parser(), "legacy-meter");
    assert_eq!(rows[0].failure_class(), "unconfigured_account");
    assert_eq!(rows[0].line_number(), Some(2));
    assert!(
        !rows[0].source_file().contains('/'),
        "the quarantine must name the source by digest, not by path: {}",
        rows[0].source_file()
    );

    assert!(
        account::account_id_by_identity(&conn, "anthropic", "file-auth:pro")
            .unwrap()
            .is_none(),
        "an unconfigured account must gain no account row"
    );
    assert_eq!(marker_sessions(&conn), vec!["twin".to_owned()]);
}

/// Two consecutive runs over one straddling source that also carries an
/// unconfigured line leave every durable table at the same cardinality.
#[test]
fn integration_two_runs_over_a_straddling_source_leave_identical_cardinalities() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    plant_native_attempt(
        &conn,
        "primary",
        UtcTimestamp::parse_rfc3339(STRADDLE_CUTOFF).unwrap(),
    );
    let source_path = state.path().join("idempotent.jsonl");
    std::fs::write(
        &source_path,
        format!(
            "{}{}",
            straddling_source_jsonl(),
            r#"{"ts":"2026-09-04T11:59:58Z","session_id":"stray","account":"file-auth:max","tier":"pro","five_hour":1,"seven_day":2,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
"#
        ),
    )
    .unwrap();
    let parsed = read_source(&source_path).unwrap();
    let configured = vec!["primary".to_owned()];

    let cardinalities = |conn: &Connection| -> (i64, i64, i64, i64) {
        conn.query_row(
            "SELECT (SELECT count(*) FROM meter_attempt),
                    (SELECT count(*) FROM meter_observation),
                    (SELECT count(*) FROM session_account_marker),
                    (SELECT count(*) FROM ingest_quarantine)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    };

    let first = import(
        &mut conn,
        &parsed,
        &configured,
        "backup-idempotent",
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .unwrap();
    let after_first = cardinalities(&conn);
    // One native attempt was planted, plus the single imported legacy reading.
    assert_eq!(after_first, (2, 1, 3, 1));

    let second = import(
        &mut conn,
        &parsed,
        &configured,
        "backup-idempotent",
        UtcTimestamp::from_unix_nanos(2_000),
    )
    .unwrap();
    assert_eq!(cardinalities(&conn), after_first);
    assert_eq!(second.imported, 0);
    assert_eq!(second.unchanged, 1);
    assert_eq!(
        second.superseded_by_native, first.superseded_by_native,
        "the superseded count classifies the source, so it cannot move between runs"
    );
    assert_eq!(second.quarantined, first.quarantined);
}

/// A legacy row already in the ledger must never become its own cutoff: the
/// second import of the same source would otherwise import nothing.
#[test]
fn integration_a_previous_legacy_import_is_not_a_native_cutoff() {
    let state = StateDir::new();
    let mut conn = open_migrated_ledger(&state);
    let first_path = state.path().join("first.jsonl");
    std::fs::write(
        &first_path,
        straddling_source_jsonl().lines().next().unwrap(),
    )
    .unwrap();
    import(
        &mut conn,
        &read_source(&first_path).unwrap(),
        &["primary".to_owned()],
        "backup-first",
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .unwrap();

    let second_path = state.path().join("second.jsonl");
    std::fs::write(&second_path, straddling_source_jsonl()).unwrap();
    let summary = import(
        &mut conn,
        &read_source(&second_path).unwrap(),
        &["primary".to_owned()],
        "backup-second",
        UtcTimestamp::from_unix_nanos(2_000),
    )
    .unwrap();
    assert_eq!(
        summary.superseded_by_native, 0,
        "a legacy attempt is history, not sampling coverage"
    );
    assert_eq!(summary.imported, 3);
}
