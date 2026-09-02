//! The schema-compliance audit and its deliberate-violation suite (`aub-sth.8`).
//!
//! Two halves. `the_live_schema_meets_the_contract` runs the auto-discovering audit over
//! the real migrated schema, so a table added later without STRICT or without a CHECK on
//! a quantity column fails here rather than passing unnoticed. The deliberate-violation
//! suite drives one case per constraint class against the database and asserts each is
//! refused: STRICT typing, the non-negative token-count floor, the quota-fraction range
//! floor, foreign-key enforcement, and each named uniqueness constraint.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::schema_audit::{SchemaFinding, audit};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-test-schema-audit-{}-{count}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open_bare(&self) -> rusqlite::Connection {
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(5000),
        };
        open(&self.path, AccessMode::ReadWrite, &policy).unwrap()
    }

    fn open_migrated(&self) -> rusqlite::Connection {
        let mut conn = self.open_bare();
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        conn
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

// --- the auto-discovering audit -------------------------------------------

/// The real migrated schema meets the contract. A regression, a new table missing STRICT
/// or a new quantity column with neither a CHECK nor an exemption, fails here with the
/// offending table and column named.
#[test]
fn the_live_schema_meets_the_contract() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let result = audit(&conn).expect("the audit must run");
    assert!(
        result.is_clean(),
        "schema audit found regressions:\n{}",
        result.report().unwrap_or_default()
    );
}

/// Every user table is declared STRICT, enumerated from SQLite rather than from a list.
#[test]
fn every_user_table_is_strict() {
    let db = TestDb::new();
    let conn = db.open_migrated();

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        tables.len() > 20,
        "expected the full core schema, got {tables:?}"
    );

    for table in tables {
        let strict: i64 = conn
            .query_row(
                "SELECT strict FROM pragma_table_list WHERE name = ?1",
                [&table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(strict, 1, "table {table} is not STRICT");
    }
}

/// Foreign keys are enforced on every connection the store hands out.
#[test]
fn foreign_key_enforcement_is_on() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let enabled: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(enabled, 1, "foreign_keys pragma must be ON");
}

/// Every `INTEGER` child column (`*_id`, not the rowid) carries a declared foreign key.
/// This is the structural half of "foreign keys are declared on every child
/// relationship"; the behavioural half is `an_orphan_child_row_is_refused` below.
#[test]
fn every_child_column_has_a_declared_foreign_key() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let result = audit(&conn).expect("the audit must run");
    let orphans: Vec<&SchemaFinding> = result
        .findings()
        .iter()
        .filter(|f| matches!(f, SchemaFinding::ChildColumnWithoutForeignKey { .. }))
        .collect();
    assert!(
        orphans.is_empty(),
        "child columns with no foreign key: {orphans:?}"
    );
}

/// The two named uniqueness constraints exist in the real schema: one terminal result
/// per attempt, one preferred interpretation per evidence row and semantics version.
#[test]
fn the_named_uniqueness_constraints_exist() {
    let db = TestDb::new();
    let conn = db.open_migrated();

    let result_pk: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('meter_attempt_result') WHERE pk > 0")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        result_pk,
        vec!["attempt_id"],
        "meter_attempt_result must be keyed by attempt_id alone"
    );

    let mut pref_pk: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('meter_observation_preference') WHERE pk > 0")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    pref_pk.sort();
    assert_eq!(
        pref_pk,
        vec!["evidence_id".to_string(), "meter_semantics_id".to_string()],
        "meter_observation_preference must be keyed by (evidence_id, meter_semantics_id)"
    );
}

// --- deliberate violations, one per constraint class ----------------------

fn seed_account(conn: &rusqlite::Connection) {
    conn.execute(
        "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
         VALUES ('acct', 'anthropic', 1, 2)",
        [],
    )
    .expect("a well-formed account row inserts");
}

fn seed_usage_event(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "INSERT INTO usage_event (
            canonical_event_id, evidence_kind, source_provenance, parser_version, created_at
        ) VALUES ('ce-1', 'transcript', 'claude-code', 'v1', 100) RETURNING id",
        [],
        |row| row.get::<_, i64>(0),
    )
    .expect("a well-formed usage_event row inserts")
}

/// STRICT typing: a text value in an `INTEGER` column is refused rather than stored.
#[test]
fn strict_typing_refuses_a_wrong_type() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let err = conn
        .execute(
            "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
             VALUES ('acct', 'anthropic', 'not-an-integer', 2)",
            [],
        )
        .unwrap_err()
        .to_string();
    let lower = err.to_lowercase();
    assert!(
        lower.contains("cannot store") && lower.contains("integer column"),
        "expected a STRICT type rejection, got: {err}"
    );
}

/// The non-negative token-count floor: a negative `count` is refused by `usage_component`.
#[test]
fn a_negative_token_count_is_refused() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let event_id = seed_usage_event(&conn);
    let err = conn
        .execute(
            "INSERT INTO usage_component (event_id, token_class, count) VALUES (?1, 'input', -1)",
            [event_id],
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("CHECK"),
        "expected a CHECK rejection, got: {err}"
    );

    // The near-identical permitted positive proceeds.
    conn.execute(
        "INSERT INTO usage_component (event_id, token_class, count) VALUES (?1, 'input', 0)",
        [event_id],
    )
    .expect("a zero token count is legal");
}

/// The quota-fraction range floor: `quota_used_ppm` is bounded to `0..=1_000_000`, proved
/// against a constraint of the exact shape the real `meter_window` table carries. Seeding
/// a real `meter_window` row needs the full attempt/evidence/observation chain, so the
/// structural half (the CHECK is present in the shipped schema) is asserted separately in
/// `the_meter_window_ppm_columns_carry_their_range_check`.
#[test]
fn a_quota_fraction_outside_its_range_is_refused() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    conn.execute_batch(
        "CREATE TABLE probe_ppm (
            v INTEGER NOT NULL CHECK (v >= 0 AND v <= 1000000)
        ) STRICT",
    )
    .unwrap();

    for bad in ["-1", "1000001"] {
        let err = conn
            .execute(&format!("INSERT INTO probe_ppm (v) VALUES ({bad})"), [])
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHECK"), "ppm={bad} not refused: {err}");
    }
    conn.execute("INSERT INTO probe_ppm (v) VALUES (1000000)", [])
        .expect("the range endpoint is legal");
}

/// Structural half of the ppm floor: the shipped `meter_window` table carries the
/// `0..=1_000_000` range CHECK on both of its ppm columns.
#[test]
fn the_meter_window_ppm_columns_carry_their_range_check() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'meter_window'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let normalized = sql.replace(['\n', ' '], "");
    for column in ["quota_used_ppm", "reported_resolution_ppm"] {
        assert!(
            normalized.contains(&format!("{column}<=1000000"))
                || normalized.contains(&format!("{column}<=1000000)")),
            "{column} has no <= 1000000 ceiling in:\n{sql}"
        );
    }
}

/// Foreign-key enforcement: an orphan child row is refused at the database.
#[test]
fn an_orphan_child_row_is_refused() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    let err = conn
        .execute(
            "INSERT INTO usage_component (event_id, token_class, count) VALUES (999999, 'input', 1)",
            [],
        )
        .unwrap_err()
        .to_string()
        .to_lowercase();
    assert!(
        err.contains("foreign key"),
        "expected a foreign key rejection, got: {err}"
    );
}

/// A named uniqueness constraint: `account (provider_key, logical_name)` rejects a
/// duplicate.
#[test]
fn a_duplicate_unique_key_is_refused() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    seed_account(&conn);
    let err = conn
        .execute(
            "INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
             VALUES ('acct', 'anthropic', 9, 9)",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.to_uppercase().contains("UNIQUE"),
        "expected a UNIQUE rejection, got: {err}"
    );
}

// --- planted negatives: the audit is not a no-op --------------------------

/// The STRICT check catches a real non-STRICT table, not just a string.
#[test]
fn the_audit_flags_a_non_strict_table() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    conn.execute_batch("CREATE TABLE sloppy (n INTEGER)")
        .unwrap();
    let result = audit(&conn).expect("the audit must run");
    assert!(
        result
            .findings()
            .iter()
            .any(|f| matches!(f, SchemaFinding::TableNotStrict { table } if table == "sloppy")),
        "audit missed a non-STRICT table: {:?}",
        result.findings()
    );
}

/// The quantity-CHECK rule catches a bare quantity column, and clears once it is checked.
/// The two tables differ only in the forbidden dimension.
#[test]
fn the_audit_flags_a_bare_quantity_column_and_clears_a_checked_one() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    conn.execute_batch("CREATE TABLE bare (widget_count INTEGER NOT NULL) STRICT")
        .unwrap();
    conn.execute_batch(
        "CREATE TABLE checked (widget_count INTEGER NOT NULL CHECK (widget_count >= 0)) STRICT",
    )
    .unwrap();

    let findings = audit(&conn).expect("the audit must run");
    let flagged = |table: &str| {
        findings.findings().iter().any(|f| {
            matches!(
                f,
                SchemaFinding::QuantityColumnWithoutCheck { table: t, column }
                    if t == table && column == "widget_count"
            )
        })
    };
    assert!(flagged("bare"), "audit missed a bare quantity column");
    assert!(
        !flagged("checked"),
        "audit flagged a checked quantity column"
    );
}

/// The ppm floor is not exemptible: a bare `*_ppm` column is a finding even though a
/// same-named non-ppm column could be exempted.
#[test]
fn the_audit_flags_a_ppm_column_missing_its_ceiling() {
    let db = TestDb::new();
    let conn = db.open_migrated();
    conn.execute_batch(
        "CREATE TABLE half_bounded (frac_ppm INTEGER NOT NULL CHECK (frac_ppm >= 0)) STRICT",
    )
    .unwrap();
    let result = audit(&conn).expect("the audit must run");
    assert!(
        result.findings().iter().any(|f| matches!(
            f,
            SchemaFinding::QuotaFractionColumnWithoutRangeCheck { table, column }
                if table == "half_bounded" && column == "frac_ppm"
        )),
        "audit missed a ppm column with no ceiling: {:?}",
        result.findings()
    );
}
