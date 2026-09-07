//! Migration 0035: make a provider quota group a scope kind (`aub-n8yx`).
//!
//! Antigravity's quota summary reports one budget per *group* of models
//! ("Gemini Models", "Claude and GPT models"), each group carrying its own
//! 5-hour and weekly buckets. A group is neither the account nor a model:
//! folding it into `ModelSpecific("Gemini Models")` was rejected because it
//! would put a non-model string into `ModelId` and let `--model` selection
//! match it. The decision is `WindowScope::ModelGroup(GroupName)`, stored as
//! `scope_kind = 'model_group'` with the group's display name in the
//! `scoped_model` column, the one free-text slot the row already has.
//!
//! WHY this rebuilds the table instead of dropping and re-adding a column
//! (the same reasoning `aub-leed` recorded for 0034): the scope CHECKs were
//! written in migration 0013, and SQLite validates a new column's CHECK
//! against every existing row at `ADD COLUMN` time. The live ledger holds
//! thousands of rows of every shape, so the only way the two CHECK
//! constraints widen is the rebuild SQLite's own ALTER TABLE guide
//! prescribes: a new table with the full constraint, every row copied
//! across, the old table dropped, the new one renamed into place, then the
//! index and the evidence triggers recreated. The runner switches foreign
//! keys off around the step (the `rebuilds_referenced_table` flag below)
//! and refuses to commit unless the foreign key check comes back empty; the
//! copy keeps every `id`, so the four referencing tables resolve against
//! the rebuilt table.
//!
//! The down step exists for the same reason the up step does, proving the
//! widened schema round-trips, and refuses loudly when any `model_group`
//! row exists: the rows are irreplaceable evidence and are never rewritten
//! to make an undo fit. It is deliberately not part of the registry
//! (migrations are forward-only); tests exercise it directly.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 35;

const MAKE_GROUP_SCOPES_STORABLE: &str = "\
DROP TRIGGER IF EXISTS meter_window_rejects_update;
DROP TRIGGER IF EXISTS meter_window_rejects_delete;
DROP INDEX IF EXISTS idx_meter_window_observation;
CREATE TABLE meter_window_0035 (
    id INTEGER PRIMARY KEY,
    observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('account_wide', 'model_specific', 'model_group')),
    scoped_model TEXT,
    quota_used_ppm INTEGER NOT NULL CHECK (quota_used_ppm >= 0 AND quota_used_ppm <= 1000000),
    reported_resolution_ppm INTEGER NOT NULL
        CHECK (reported_resolution_ppm > 0 AND reported_resolution_ppm <= 1000000),
    quantization TEXT NOT NULL CHECK (
        quantization IN ('exact', 'rounded_to_nearest', 'rounded_down', 'rounded_up', 'unknown')
    ),
    nominal_duration_nanos INTEGER NOT NULL CHECK (nominal_duration_nanos >= 0),
    resets_at INTEGER,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (
        is_active IN (0, 1)
    ),
    severity TEXT NOT NULL DEFAULT 'unknown' CHECK (
        length(severity) > 0
    ),
    reset_grid TEXT,
    reset_state TEXT NOT NULL DEFAULT 'known' CHECK (
        reset_state IN ('known', 'not_started', 'scheduled')
        AND ((resets_at IS NULL) = (reset_state = 'not_started'))
        AND ((reset_grid IS NOT NULL) = (reset_state = 'scheduled'))
    ),
    CHECK (
        (scope_kind = 'account_wide' AND scoped_model IS NULL)
        OR (scope_kind = 'model_specific' AND scoped_model IS NOT NULL)
        OR (scope_kind = 'model_group' AND scoped_model IS NOT NULL)
    )
) STRICT;
INSERT INTO meter_window_0035 (
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, nominal_duration_nanos, resets_at,
    is_active, severity, reset_grid, reset_state
)
SELECT
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, nominal_duration_nanos, resets_at,
    is_active, severity, reset_grid, reset_state
FROM meter_window
ORDER BY id;
DROP TABLE meter_window;
ALTER TABLE meter_window_0035 RENAME TO meter_window;
CREATE INDEX idx_meter_window_observation ON meter_window (observation_id);
CREATE TRIGGER meter_window_rejects_update BEFORE UPDATE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never updated');
END;
CREATE TRIGGER meter_window_rejects_delete BEFORE DELETE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never deleted');
END;
";

/// The inverse shape: 0034's own table definition, which the down step
/// rebuilds into so the widened CHECKs are proven reversible when no
/// `model_group` row exists.
#[cfg(test)]
const RESTORE_WIDENED_CHECKS: &str = "\
DROP TRIGGER IF EXISTS meter_window_rejects_update;
DROP TRIGGER IF EXISTS meter_window_rejects_delete;
DROP INDEX IF EXISTS idx_meter_window_observation;
CREATE TABLE meter_window_0035_down (
    id INTEGER PRIMARY KEY,
    observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('account_wide', 'model_specific')),
    scoped_model TEXT,
    quota_used_ppm INTEGER NOT NULL CHECK (quota_used_ppm >= 0 AND quota_used_ppm <= 1000000),
    reported_resolution_ppm INTEGER NOT NULL
        CHECK (reported_resolution_ppm > 0 AND reported_resolution_ppm <= 1000000),
    quantization TEXT NOT NULL CHECK (
        quantization IN ('exact', 'rounded_to_nearest', 'rounded_down', 'rounded_up', 'unknown')
    ),
    nominal_duration_nanos INTEGER NOT NULL CHECK (nominal_duration_nanos >= 0),
    resets_at INTEGER,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (
        is_active IN (0, 1)
    ),
    severity TEXT NOT NULL DEFAULT 'unknown' CHECK (
        length(severity) > 0
    ),
    reset_grid TEXT,
    reset_state TEXT NOT NULL DEFAULT 'known' CHECK (
        reset_state IN ('known', 'not_started', 'scheduled')
        AND ((resets_at IS NULL) = (reset_state = 'not_started'))
        AND ((reset_grid IS NOT NULL) = (reset_state = 'scheduled'))
    ),
    CHECK (
        (scope_kind = 'account_wide' AND scoped_model IS NULL)
        OR (scope_kind = 'model_specific' AND scoped_model IS NOT NULL)
    )
) STRICT;
INSERT INTO meter_window_0035_down (
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, nominal_duration_nanos, resets_at,
    is_active, severity, reset_grid, reset_state
)
SELECT
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, nominal_duration_nanos, resets_at,
    is_active, severity, reset_grid, reset_state
FROM meter_window
ORDER BY id;
DROP TABLE meter_window;
ALTER TABLE meter_window_0035_down RENAME TO meter_window;
CREATE INDEX idx_meter_window_observation ON meter_window (observation_id);
CREATE TRIGGER meter_window_rejects_update BEFORE UPDATE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never updated');
END;
CREATE TRIGGER meter_window_rejects_delete BEFORE DELETE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never deleted');
END;
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(MAKE_GROUP_SCOPES_STORABLE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot make quota-group meter window scopes storable: {error}"
            ))
        })
}

/// Reverts the schema to 0034's shape, refusing when any `model_group` row
/// exists: those rows are irreplaceable evidence, and an undo that would
/// rewrite them must not run. This is a test-facing inverse, not a registry
/// step; migrations here are forward-only.
#[cfg(test)]
pub(crate) fn down(conn: &rusqlite::Connection) -> Result<(), Error> {
    let model_group_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM meter_window WHERE scope_kind = 'model_group'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            Error::Store(format!(
                "cannot count the model_group rows before undoing migration 0035: {error}"
            ))
        })?;
    if model_group_rows > 0 {
        return Err(Error::Store(format!(
            "cannot undo migration 0035: {model_group_rows} model_group meter_window row(s) exist; \
             the rows are irreplaceable evidence and are not rewritten, so restoring schema 34 \
             requires restoring a backup instead"
        )));
    }
    conn.execute_batch(RESTORE_WIDENED_CHECKS).map_err(|error| {
        Error::Store(format!(
            "cannot restore the pre-group meter window scope CHECKs: {error}"
        ))
    })
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        // Every row is copied unchanged into the rebuilt table; the evidence
        // is carried across, never rewritten, which is why no verified backup
        // is demanded before this step.
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: true,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDb(PathBuf);

    impl ScratchDb {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-migration-0035-test-{}-{suffix}.sqlite3",
                std::process::id()
            ));
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn open_conn(db: &ScratchDb) -> rusqlite::Connection {
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(5000),
        };
        open(db.path(), AccessMode::ReadWrite, &policy).expect("fixture connection must open")
    }

    /// The minimal foreign-key chain one `meter_window` row needs: one
    /// account, one sample run, one policy snapshot, one attempt, one
    /// evidence row, one observation, so `PRAGMA foreign_key_check` has
    /// something honest to answer about.
    fn seed_reference_chain(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
                 VALUES (1, 'agy-test', 'antigravity', 100, 100);
             INSERT INTO sample_run (id, trigger, started_at, ended_at, aub_version, configuration_fingerprint)
                 VALUES (1, 'manual', 10, NULL, '0.0.0-test', 'test-fingerprint');
             INSERT INTO sampling_policy_snapshot (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos, reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version)
                 VALUES (1, 1, 100, 3600000000000, 86400000000000, 'wait_for_reset', 'exponential', 300000000000, 'test-policy-v1');
             INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, credential_context_id, policy_snapshot_id, due_at, due_reason, due_basis_attempt_id, due_basis_result_id, provider_contract_id, meter_semantics_id)
                 VALUES (1, 1, 1, 'antigravity', 400, NULL, 1, 500, 'ordinary_cadence', NULL, NULL, 'google-antigravity-quota-summary-v1', 'google-antigravity-subscription-v1');
             INSERT INTO meter_response_evidence (id, attempt_id, response_classification, received_at, provider_observed_at_original, evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated)
                 VALUES (1, 1, 'success', 410, NULL, 'test-capsule-1', 'json-quota-capsule-v1', 'sensitive-json-v1', 'test-hash-1', 0);
             INSERT INTO meter_observation (id, attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at, measurement_basis, observed_plan, observed_tier, adapter_version, provider_contract_id, meter_semantics_id, normalized_fingerprint)
                 VALUES (1, 1, 1, 1, 'antigravity', NULL, 420, 'locally_received', NULL, NULL, 'test-adapter-1', 'google-antigravity-quota-summary-v1', 'google-antigravity-subscription-v1', 'test-fingerprint-1');",
        )
        .expect("reference chain must seed");
    }

    /// One `meter_window` row of the named scope and reset shape, written
    /// through the plain column names so the seed is readable against the
    /// 0034 schema it runs under. Eight arguments, every one of them a
    /// column of the row being seeded; the test seeds the row shape the
    /// migration copies, not a builder-friendly subset.
    #[allow(clippy::too_many_arguments)]
    fn seed_window(
        conn: &rusqlite::Connection,
        id: i64,
        semantic_key: &str,
        scope_kind: &str,
        scoped_model: Option<&str>,
        resets_at: Option<i64>,
        reset_state: &str,
        reset_grid: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO meter_window (
                id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
                reported_resolution_ppm, quantization, resets_at, reset_state, reset_grid,
                nominal_duration_nanos
            ) VALUES (
                ?1, 1, ?2, ?3, ?4, 163000, 10000, 'exact', ?5, ?6, ?7, 18000000000000
            )",
            rusqlite::params![
                id,
                semantic_key,
                scope_kind,
                scoped_model,
                resets_at,
                reset_state,
                reset_grid,
            ],
        )
        .expect("window row must seed");
    }

    fn window_rows(conn: &rusqlite::Connection) -> Vec<(i64, String, Option<String>, String)> {
        let mut statement = conn
            .prepare(
                "SELECT id, scope_kind, scoped_model, reset_state
                 FROM meter_window ORDER BY id",
            )
            .expect("window query must prepare");
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, String>(1)?,
                    row.get(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("window query must run");
        rows.collect::<Result<Vec<_>, _>>().expect("rows must read")
    }

    /// Up applies to a ledger holding rows of every shape the live table
    /// has, both scopes and all three reset states, and every row survives
    /// unchanged, with the four referencing tables still naming
    /// `meter_window` and no foreign key dangling.
    #[test]
    fn up_copies_every_shape_and_leaves_the_referencing_tables_named() {
        let db = ScratchDb::new();
        let mut conn = open_conn(&db);
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(0));
        let all_migrations = registry();
        run_migrations(&mut conn, &all_migrations[..34], None, &clock)
            .expect("migrations up to 34 must succeed");
        seed_reference_chain(&conn);

        // Every shape the live ledger has, before migrating: both pre-0035
        // scope kinds across all three reset states.
        seed_window(
            &conn,
            1,
            "session",
            "account_wide",
            None,
            Some(500),
            "known",
            None,
        );
        seed_window(
            &conn,
            2,
            "weekly_all",
            "account_wide",
            None,
            None,
            "not_started",
            None,
        );
        seed_window(
            &conn,
            3,
            "weekly_scoped_claude",
            "model_specific",
            Some("claude-sonnet"),
            Some(600),
            "known",
            None,
        );
        seed_window(
            &conn,
            4,
            "session",
            "model_specific",
            Some("claude-opus"),
            Some(700),
            "scheduled",
            Some("ollama-cloud-v1"),
        );

        run_migrations(&mut conn, &all_migrations, None, &clock)
            .expect("migration 35 must apply cleanly to an existing database");

        let rows = window_rows(&conn);
        assert_eq!(
            rows,
            vec![
                (1, "account_wide".to_owned(), None, "known".to_owned()),
                (2, "account_wide".to_owned(), None, "not_started".to_owned()),
                (
                    3,
                    "model_specific".to_owned(),
                    Some("claude-sonnet".to_owned()),
                    "known".to_owned()
                ),
                (
                    4,
                    "model_specific".to_owned(),
                    Some("claude-opus".to_owned()),
                    "scheduled".to_owned()
                ),
            ],
            "every pre-migration row survives the rebuild unchanged"
        );

        // The widened CHECK accepts a group row under the new kind, with the
        // group's display name in the scoped_model column.
        seed_window(
            &conn,
            5,
            "5h",
            "model_group",
            Some("Gemini Models"),
            Some(800),
            "known",
            None,
        );
        let group_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM meter_window WHERE scope_kind = 'model_group'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(group_rows, 1, "the widened CHECK admits a group row");

        // The rebuild left the four referencing tables pointing at the name
        // `meter_window`, nothing references the transient name, and no
        // foreign key dangles.
        let referrers: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'
                 AND sql LIKE '%REFERENCES meter_window%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            referrers, 4,
            "every child table still references meter_window by name"
        );
        let renamed_referrers: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE sql LIKE '%meter_window_0035%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            renamed_referrers, 0,
            "nothing references the rebuild's transient name"
        );
        let dangling: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(dangling, 0, "no foreign key dangles after the rebuild");

        // The narrowed CHECKs still fire after the widening: a group row
        // without a group name is refused, as is any row of an unknown kind.
        let refused = conn.execute(
            "INSERT INTO meter_window (
                observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
                reported_resolution_ppm, quantization, resets_at, reset_state,
                nominal_duration_nanos
            ) VALUES (1, '5h', 'model_group', NULL, 0, 10000, 'exact', 100, 'known', 1000)",
            [],
        );
        assert!(
            refused.is_err(),
            "a model_group row without a group name must be refused"
        );
        let unknown_kind = conn.execute(
            "INSERT INTO meter_window (
                observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
                reported_resolution_ppm, quantization, resets_at, reset_state,
                nominal_duration_nanos
            ) VALUES (1, '5h', 'workspace', NULL, 0, 10000, 'exact', 100, 'known', 1000)",
            [],
        );
        assert!(
            unknown_kind.is_err(),
            "an unknown scope kind must still be refused"
        );
    }

    /// The down step refuses loudly when any `model_group` row exists: the
    /// rows are irreplaceable evidence and are never rewritten to make an
    /// undo fit.
    #[test]
    fn down_refuses_when_a_model_group_row_exists() {
        let db = ScratchDb::new();
        let mut conn = open_conn(&db);
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(0));
        let all_migrations = registry();
        run_migrations(&mut conn, &all_migrations, None, &clock).expect("migrations must succeed");
        seed_reference_chain(&conn);
        seed_window(
            &conn,
            1,
            "5h",
            "model_group",
            Some("Gemini Models"),
            Some(800),
            "known",
            None,
        );

        let error = down(&conn).expect_err("the down step must refuse");
        assert!(
            error.to_string().contains("1 model_group meter_window row"),
            "the refusal must name the evidence it would have destroyed: {error}"
        );

        // The refusal left the schema untouched: the widened CHECK still
        // admits the group row and every row is still there.
        assert_eq!(window_rows(&conn).len(), 1);
    }

    /// The down step succeeds on a ledger with no `model_group` rows and
    /// carries every row across to 0034's shape.
    #[test]
    fn down_reverts_to_the_narrowed_check_without_group_rows() {
        let db = ScratchDb::new();
        let mut conn = open_conn(&db);
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(0));
        let all_migrations = registry();
        run_migrations(&mut conn, &all_migrations, None, &clock).expect("migrations must succeed");
        seed_reference_chain(&conn);
        seed_window(
            &conn,
            1,
            "session",
            "account_wide",
            None,
            Some(500),
            "known",
            None,
        );
        seed_window(
            &conn,
            2,
            "weekly_all",
            "account_wide",
            None,
            None,
            "not_started",
            None,
        );
        seed_window(
            &conn,
            3,
            "weekly_scoped_claude",
            "model_specific",
            Some("claude-sonnet"),
            Some(600),
            "known",
            None,
        );

        down(&conn).expect("the down step must succeed without group rows");

        let rows = window_rows(&conn);
        assert_eq!(
            rows.len(),
            3,
            "every row survives the inverse rebuild unchanged"
        );

        // The narrowed CHECK is live again: a group row is refused.
        let refused = conn.execute(
            "INSERT INTO meter_window (
                observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
                reported_resolution_ppm, quantization, resets_at, reset_state,
                nominal_duration_nanos
            ) VALUES (1, '5h', 'model_group', 'Gemini Models', 0, 10000, 'exact', 100, 'known', 1000)",
            [],
        );
        assert!(
            refused.is_err(),
            "the reverted CHECK must refuse the model_group kind again"
        );

        // And the table still names its referenced and referencing shape.
        let referrers: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'
                 AND sql LIKE '%REFERENCES meter_window%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(referrers, 4, "the child tables still reference the table");
        let dangling: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(dangling, 0, "no foreign key dangles after the down step");
    }
}
