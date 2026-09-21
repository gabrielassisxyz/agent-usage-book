//! Schema step: a result shape for the joint per-kind calibration
//! (`aub-multivariate-result-shape-2hvt`).
//!
//! `window_calibration_result` records one credits-per-percentage-point
//! coefficient, NOT NULL, and a joint fit has one coefficient per token kind.
//! Reducing the kinds to one scalar through a cost model would record as truth
//! the assumption the joint fit was run to test (PLAN.md 22.1), so the per-kind
//! result gets tables of its own, mirroring the candidate pair of 0039:
//!
//! - `window_calibration_multivariate_result`, one validated joint fit: the
//!   condition number it was fitted under, the held-out residual over evidence
//!   disjoint from the fit, and every validation and build identifier the
//!   scalar result states;
//! - `window_calibration_multivariate_result_coefficient`, one row per kind the
//!   candidate named, with its standard error and interval;
//! - `calibration_multivariate_lifecycle`, the append-only activation events of
//!   a per-kind result. Its predecessor may be either shape, because a scope
//!   has one active calibration whichever shape it is. The scalar lifecycle
//!   table cannot name a per-kind predecessor without being rebuilt, which is
//!   why the events live here rather than there.
//!
//! One calibration id names one result across both shapes: `calibrate activate`
//! takes an id and must resolve it to exactly one row, so an insert into either
//! result table refuses an id the other already holds.
//!
//! Recovery: the framework is forward-only. The manual reversal below drops the
//! new tables and the two cross-shape triggers; it is exercised by this
//! module's own round-trip test, never by production code.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 44;

const CREATE_MULTIVARIATE_RESULT: &str = "\
CREATE TABLE window_calibration_multivariate_result (
    id INTEGER PRIMARY KEY,
    calibration_id TEXT NOT NULL UNIQUE,
    window_calibration_multivariate_candidate_id INTEGER NOT NULL UNIQUE
        REFERENCES window_calibration_multivariate_candidate(id),
    provider TEXT NOT NULL,
    plan_tier TEXT NOT NULL,
    window_semantic_key TEXT NOT NULL,
    condition_number_micros INTEGER NOT NULL,
    condition_number_threshold_micros INTEGER NOT NULL,
    fit_residual_ppm INTEGER NOT NULL,
    held_out_residual_ppm INTEGER NOT NULL,
    validation_observation_count INTEGER NOT NULL,
    sample_count INTEGER NOT NULL,
    inputs_digest TEXT NOT NULL,
    inputs_count INTEGER NOT NULL,
    fitting_evidence_digest TEXT NOT NULL,
    validation_evidence_digest TEXT NOT NULL,
    validation_method TEXT NOT NULL,
    validation_version TEXT NOT NULL,
    statistical_method TEXT NOT NULL,
    statistical_parameters TEXT NOT NULL,
    phase_design TEXT NOT NULL,
    activation_policy_version TEXT NOT NULL,
    aub_version TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_until INTEGER NOT NULL,
    knowledge_time INTEGER NOT NULL,
    CHECK (length(calibration_id) > 0),
    CHECK (length(provider) > 0),
    CHECK (length(plan_tier) > 0),
    CHECK (length(window_semantic_key) > 0),
    CHECK (condition_number_micros >= 1000000),
    CHECK (condition_number_threshold_micros > 1000000),
    CHECK (fit_residual_ppm >= 0 AND fit_residual_ppm <= 1000000),
    CHECK (held_out_residual_ppm >= 0 AND held_out_residual_ppm <= 1000000),
    CHECK (validation_observation_count >= 2),
    CHECK (sample_count >= 0),
    CHECK (length(inputs_digest) = 16),
    CHECK (inputs_count >= 0),
    CHECK (length(fitting_evidence_digest) = 16),
    CHECK (length(validation_evidence_digest) = 16),
    CHECK (length(validation_method) > 0),
    CHECK (length(validation_version) > 0),
    CHECK (length(statistical_method) > 0),
    CHECK (length(statistical_parameters) > 0),
    CHECK (length(phase_design) > 0),
    CHECK (length(activation_policy_version) > 0),
    CHECK (length(aub_version) > 0),
    CHECK (length(source_revision) > 0),
    CHECK (valid_until >= valid_from)
) STRICT";

const CREATE_MULTIVARIATE_RESULT_COEFFICIENT: &str = "\
CREATE TABLE window_calibration_multivariate_result_coefficient (
    id INTEGER PRIMARY KEY,
    window_calibration_multivariate_result_id INTEGER NOT NULL
        REFERENCES window_calibration_multivariate_result(id),
    token_kind TEXT NOT NULL,
    estimate_micro_ppm_per_token INTEGER NOT NULL,
    std_error_micro_ppm_per_token INTEGER NOT NULL,
    interval_low_micro_ppm_per_token INTEGER NOT NULL,
    interval_high_micro_ppm_per_token INTEGER NOT NULL,
    UNIQUE (window_calibration_multivariate_result_id, token_kind),
    CHECK (token_kind IN ('input', 'output', 'cache_read', 'cache_write')),
    CHECK (estimate_micro_ppm_per_token + 2 * std_error_micro_ppm_per_token >= 0),
    CHECK (std_error_micro_ppm_per_token >= 0),
    CHECK (interval_high_micro_ppm_per_token >= interval_low_micro_ppm_per_token)
) STRICT";

const CREATE_MULTIVARIATE_LIFECYCLE: &str = "\
CREATE TABLE calibration_multivariate_lifecycle (
    id INTEGER PRIMARY KEY,
    window_calibration_multivariate_result_id INTEGER NOT NULL
        REFERENCES window_calibration_multivariate_result(id),
    event_kind TEXT NOT NULL,
    event_at INTEGER NOT NULL,
    supersedes_result_id INTEGER REFERENCES window_calibration_result(id),
    supersedes_multivariate_result_id INTEGER
        REFERENCES window_calibration_multivariate_result(id),
    actor TEXT NOT NULL,
    activation_policy_version TEXT NOT NULL,
    fitting_evidence_digest TEXT NOT NULL,
    validation_evidence_digest TEXT NOT NULL,
    CHECK (event_kind IN ('activation', 'supersession')),
    CHECK (
        (event_kind = 'activation'
            AND supersedes_result_id IS NULL
            AND supersedes_multivariate_result_id IS NULL)
        OR (event_kind = 'supersession'
            AND (supersedes_result_id IS NULL) <> (supersedes_multivariate_result_id IS NULL))
    ),
    CHECK (length(actor) > 0),
    CHECK (length(activation_policy_version) > 0),
    CHECK (length(fitting_evidence_digest) = 16),
    CHECK (length(validation_evidence_digest) = 16),
    UNIQUE (event_at, window_calibration_multivariate_result_id)
) STRICT";

/// The two triggers that keep one calibration id naming one result across the
/// scalar and the per-kind table.
const CROSS_SHAPE_ID_TRIGGERS: &str = "
CREATE TRIGGER window_calibration_multivariate_result_id_unique_across_shapes
BEFORE INSERT ON window_calibration_multivariate_result
WHEN EXISTS (SELECT 1 FROM window_calibration_result WHERE calibration_id = NEW.calibration_id)
BEGIN
    SELECT RAISE(ABORT, 'calibration id already names a scalar window calibration result');
END;
CREATE TRIGGER window_calibration_result_id_unique_across_shapes
BEFORE INSERT ON window_calibration_result
WHEN EXISTS (
    SELECT 1 FROM window_calibration_multivariate_result WHERE calibration_id = NEW.calibration_id
)
BEGIN
    SELECT RAISE(ABORT, 'calibration id already names a per-kind window calibration result');
END;";

const IMMUTABLE_TABLES: [&str; 2] = [
    "window_calibration_multivariate_result",
    "window_calibration_multivariate_result_coefficient",
];

const APPEND_ONLY_TABLE: &str = "calibration_multivariate_lifecycle";

fn guard_trigger_sql(table: &str, verb: &str, note: &str) -> String {
    format!(
        "CREATE TRIGGER {table}_no_{lower_verb} BEFORE {verb} ON {table}
BEGIN
    SELECT RAISE(ABORT, '{table} is {note}: {lower_verb} refused');
END",
        lower_verb = verb.to_lowercase(),
    )
}

/// The manual reversal, for the round-trip test below. Production code never
/// runs it: the framework is forward-only.
#[cfg(test)]
const DROP_MULTIVARIATE_RESULT_TABLES: &str = "
DROP TRIGGER window_calibration_result_id_unique_across_shapes;
DROP TABLE calibration_multivariate_lifecycle;
DROP TABLE window_calibration_multivariate_result_coefficient;
DROP TABLE window_calibration_multivariate_result;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    for (label, sql) in [
        (
            "window_calibration_multivariate_result",
            CREATE_MULTIVARIATE_RESULT,
        ),
        (
            "window_calibration_multivariate_result_coefficient",
            CREATE_MULTIVARIATE_RESULT_COEFFICIENT,
        ),
        (
            "calibration_multivariate_lifecycle",
            CREATE_MULTIVARIATE_LIFECYCLE,
        ),
        ("the cross-shape id triggers", CROSS_SHAPE_ID_TRIGGERS),
    ] {
        conn.execute_batch(sql)
            .map_err(|e| Error::Store(format!("cannot create {label}: {e}")))?;
    }
    let guarded = IMMUTABLE_TABLES
        .iter()
        .map(|table| (*table, "immutable"))
        .chain([(APPEND_ONLY_TABLE, "append-only")]);
    for (table, note) in guarded {
        for verb in ["UPDATE", "DELETE"] {
            conn.execute_batch(&guard_trigger_sql(table, verb, note))
                .map_err(|e| {
                    Error::Store(format!("cannot create the {table} {verb} guard: {e}"))
                })?;
        }
    }
    Ok(())
}

/// This step, for the registry.
///
/// Additive only: three new empty tables and two insert triggers, so no
/// irreplaceable data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: false,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use crate::store::migrations::registry;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    const NEW_TABLES: [&str; 3] = [
        "window_calibration_multivariate_result",
        "window_calibration_multivariate_result_coefficient",
        "calibration_multivariate_lifecycle",
    ];

    fn scratch_path() -> std::path::PathBuf {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "aub-migration-0044-test-{}-{suffix}.sqlite3",
            std::process::id()
        ))
    }

    fn open_at_full_schema(path: &std::path::Path) -> rusqlite::Connection {
        let mut conn = open(
            path,
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(500),
            },
        )
        .expect("scratch database must open");
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .expect("the registry must apply");
        conn
    }

    fn object_exists(conn: &rusqlite::Connection, kind: &str, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
            [kind, name],
            |row| row.get::<_, i64>(0),
        )
        .expect("sqlite_master must answer")
            == 1
    }

    /// The tables arrive with the step, the manual reversal removes them and
    /// the trigger it added to the scalar table, and re-applying the step
    /// brings everything back.
    #[test]
    fn multivariate_result_tables_round_trip_through_up_and_down() {
        let path = scratch_path();
        let conn = open_at_full_schema(&path);
        for table in NEW_TABLES {
            assert!(object_exists(&conn, "table", table), "{table} after up");
        }
        let scalar_trigger = "window_calibration_result_id_unique_across_shapes";
        assert!(object_exists(&conn, "trigger", scalar_trigger));
        conn.execute_batch(DROP_MULTIVARIATE_RESULT_TABLES)
            .expect("manual reversal must run");
        for table in NEW_TABLES {
            assert!(!object_exists(&conn, "table", table), "{table} after down");
        }
        assert!(!object_exists(&conn, "trigger", scalar_trigger));
        apply(&conn).expect("re-applying the step must work");
        for table in NEW_TABLES {
            assert!(object_exists(&conn, "table", table), "{table} after re-up");
        }
        assert!(object_exists(&conn, "trigger", scalar_trigger));
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// A lifecycle row may name at most one predecessor, of either shape, and
    /// only a supersession names one.
    #[test]
    fn a_lifecycle_row_names_at_most_one_predecessor() {
        let path = scratch_path();
        let conn = open_at_full_schema(&path);
        let insert = |kind: &str, scalar: Option<i64>, joint: Option<i64>| {
            conn.execute(
                "INSERT INTO calibration_multivariate_lifecycle (window_calibration_multivariate_result_id, event_kind, event_at, supersedes_result_id, supersedes_multivariate_result_id, actor, activation_policy_version, fitting_evidence_digest, validation_evidence_digest) VALUES (1, ?1, 10, ?2, ?3, 'operator', 'v1', '0000000000000000', '0000000000000000')",
                rusqlite::params![kind, scalar, joint],
            )
        };
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .expect("the shape test needs no parent rows");
        assert!(insert("supersession", Some(1), Some(1)).is_err());
        assert!(insert("supersession", None, None).is_err());
        assert!(insert("activation", Some(1), None).is_err());
        insert("supersession", None, Some(2)).expect("one per-kind predecessor is a supersession");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
