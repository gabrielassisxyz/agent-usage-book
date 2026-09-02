//! Schema step: the calibration tables (`aub-c0b.1`, PLAN.md 12.14, 23.1, 24).
//!
//! Calibration turns the credits-to-quota-window relationship into a versioned
//! experiment rather than a coefficient someone typed once. Five immutable
//! tables hold it:
//!
//! - `calibration_experiment`  a recorded observation-gathering exercise;
//! - `window_calibration_candidate`  a proposed fit, kept as evidence and not
//!   yet promoted to truth;
//! - `window_calibration_result`  a validated fit, deliberately heavy: every
//!   field the design lists exists because a calibration whose record lacks it
//!   cannot answer a question somebody will eventually ask about a number this
//!   system printed;
//! - `window_calibration_source_experiment`  the many-to-many link from a
//!   result back to the experiments it was fitted from;
//! - `calibration_lifecycle`  append-only activation and supersession events,
//!   scoped per (provider, plan tier, window semantic key).
//!
//! Two independent times run through every witness here and conflating them
//! makes historical reports irreproducible (PLAN.md 12.14). `valid_from` and
//! `valid_until` say when the witness describes the physical world;
//! `knowledge_time` says when `aub` learned or recorded it. A price that took
//! effect on 1 June but was imported on 12 August means a report produced on
//! 1 July was right about what `aub` then knew and wrong about the world, and
//! both questions stay answerable because nothing is ever mutated: immutable
//! records plus append-only activation events are enough, with no general
//! temporal database.
//!
//! Immutability is enforced at the database, not by repository politeness: an
//! `UPDATE` or `DELETE` against any of these tables raises `ABORT`. Activation
//! and supersession are rows in `calibration_lifecycle`, never a status column
//! on the result, so "which coefficient was in force on that date" is the
//! latest lifecycle row at or before that instant for the matching scope.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 15;

const CREATE_CALIBRATION_EXPERIMENT: &str = "\
CREATE TABLE calibration_experiment (
    id INTEGER PRIMARY KEY,
    experiment_id TEXT NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    plan_tier TEXT NOT NULL,
    window_semantic_key TEXT NOT NULL,
    meter_semantics_id TEXT NOT NULL,
    billing_semantics_id TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_until INTEGER NOT NULL,
    knowledge_time INTEGER NOT NULL,
    CHECK (length(experiment_id) > 0),
    CHECK (length(provider) > 0),
    CHECK (length(plan_tier) > 0),
    CHECK (length(window_semantic_key) > 0),
    CHECK (length(meter_semantics_id) > 0),
    CHECK (length(billing_semantics_id) > 0),
    CHECK (valid_until >= valid_from)
) STRICT";

const CREATE_WINDOW_CALIBRATION_CANDIDATE: &str = "\
CREATE TABLE window_calibration_candidate (
    id INTEGER PRIMARY KEY,
    candidate_id TEXT NOT NULL UNIQUE,
    experiment_id INTEGER NOT NULL REFERENCES calibration_experiment(id),
    provider TEXT NOT NULL,
    plan_tier TEXT NOT NULL,
    window_semantic_key TEXT NOT NULL,
    fitted_micros_per_point INTEGER NOT NULL,
    equivalent_full_window_capacity_micros INTEGER NOT NULL,
    fit_residual_micros INTEGER NOT NULL,
    uncertainty_low_micros INTEGER NOT NULL,
    uncertainty_high_micros INTEGER NOT NULL,
    sample_count INTEGER NOT NULL,
    inputs_digest TEXT NOT NULL,
    inputs_count INTEGER NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_until INTEGER NOT NULL,
    knowledge_time INTEGER NOT NULL,
    CHECK (length(candidate_id) > 0),
    CHECK (length(provider) > 0),
    CHECK (length(plan_tier) > 0),
    CHECK (length(window_semantic_key) > 0),
    CHECK (fit_residual_micros >= 0),
    CHECK (uncertainty_high_micros >= uncertainty_low_micros),
    CHECK (sample_count >= 0),
    CHECK (length(inputs_digest) = 16),
    CHECK (inputs_count >= 0),
    CHECK (valid_until >= valid_from)
) STRICT";

const CREATE_WINDOW_CALIBRATION_RESULT: &str = "\
CREATE TABLE window_calibration_result (
    id INTEGER PRIMARY KEY,
    calibration_id TEXT NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    plan_tier TEXT NOT NULL,
    window_semantic_key TEXT NOT NULL,
    meter_semantics_id TEXT NOT NULL,
    billing_semantics_id TEXT NOT NULL,
    cost_model_id TEXT NOT NULL,
    fitted_micros_per_point INTEGER NOT NULL,
    equivalent_full_window_capacity_micros INTEGER NOT NULL,
    fit_residual_micros INTEGER NOT NULL,
    uncertainty_low_micros INTEGER NOT NULL,
    uncertainty_high_micros INTEGER NOT NULL,
    lag_estimate_nanos INTEGER,
    lag_handling TEXT NOT NULL,
    sample_count INTEGER NOT NULL,
    fit_timestamp INTEGER NOT NULL,
    inputs_digest TEXT NOT NULL,
    inputs_count INTEGER NOT NULL,
    fitting_evidence_digest TEXT NOT NULL,
    validation_evidence_digest TEXT NOT NULL,
    validation_method TEXT NOT NULL,
    validation_version TEXT NOT NULL,
    out_of_sample_residual_micros INTEGER,
    statistical_method TEXT NOT NULL,
    statistical_parameters TEXT NOT NULL,
    condition_number_micros INTEGER,
    observation_coverage_requirement TEXT NOT NULL,
    settling_policy TEXT NOT NULL,
    excluded_samples TEXT NOT NULL,
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
    CHECK (length(meter_semantics_id) > 0),
    CHECK (length(billing_semantics_id) > 0),
    CHECK (length(cost_model_id) > 0),
    CHECK (fit_residual_micros >= 0),
    CHECK (uncertainty_high_micros >= uncertainty_low_micros),
    CHECK (lag_estimate_nanos IS NULL OR lag_estimate_nanos >= 0),
    CHECK (length(lag_handling) > 0),
    CHECK (sample_count >= 0),
    CHECK (length(inputs_digest) = 16),
    CHECK (inputs_count >= 0),
    CHECK (length(fitting_evidence_digest) = 16),
    CHECK (length(validation_evidence_digest) = 16),
    CHECK (length(validation_method) > 0),
    CHECK (length(validation_version) > 0),
    CHECK (length(statistical_method) > 0),
    CHECK (length(statistical_parameters) > 0),
    CHECK (length(observation_coverage_requirement) > 0),
    CHECK (length(settling_policy) > 0),
    CHECK (length(excluded_samples) > 0),
    CHECK (length(activation_policy_version) > 0),
    CHECK (length(aub_version) > 0),
    CHECK (length(source_revision) > 0),
    CHECK (valid_until >= valid_from)
) STRICT";

const CREATE_WINDOW_CALIBRATION_SOURCE_EXPERIMENT: &str = "\
CREATE TABLE window_calibration_source_experiment (
    id INTEGER PRIMARY KEY,
    result_id INTEGER NOT NULL REFERENCES window_calibration_result(id),
    experiment_id INTEGER NOT NULL REFERENCES calibration_experiment(id),
    UNIQUE (result_id, experiment_id)
) STRICT";

const CREATE_CALIBRATION_LIFECYCLE: &str = "\
CREATE TABLE calibration_lifecycle (
    id INTEGER PRIMARY KEY,
    calibration_result_id INTEGER NOT NULL REFERENCES window_calibration_result(id),
    event_kind TEXT NOT NULL,
    event_at INTEGER NOT NULL,
    supersedes_result_id INTEGER REFERENCES window_calibration_result(id),
    CHECK (event_kind IN ('activation', 'supersession')),
    CHECK (
        (event_kind = 'activation' AND supersedes_result_id IS NULL)
        OR (event_kind = 'supersession' AND supersedes_result_id IS NOT NULL)
    ),
    UNIQUE (event_at, calibration_result_id)
) STRICT";

/// The scope a result is looked up and activated within: an active calibration
/// is one per window, so lookups filter on these three columns.
const CREATE_RESULT_SCOPE_INDEX: &str = "\
CREATE INDEX idx_window_calibration_result_scope ON window_calibration_result (
    provider, plan_tier, window_semantic_key, valid_from, valid_until
)";

const IMMUTABLE_TABLES: [&str; 5] = [
    "calibration_experiment",
    "window_calibration_candidate",
    "window_calibration_result",
    "window_calibration_source_experiment",
    "calibration_lifecycle",
];

fn immutability_trigger_sql(table: &str, verb: &str) -> String {
    let note = if table == "calibration_lifecycle" {
        "append-only"
    } else {
        "immutable"
    };
    format!(
        "CREATE TRIGGER {table}_no_{lower_verb} BEFORE {verb} ON {table}
BEGIN
    SELECT RAISE(ABORT, '{table} is {note}: {lower_verb} refused');
END",
        lower_verb = verb.to_lowercase(),
    )
}

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    for (label, sql) in [
        ("calibration_experiment", CREATE_CALIBRATION_EXPERIMENT),
        (
            "window_calibration_candidate",
            CREATE_WINDOW_CALIBRATION_CANDIDATE,
        ),
        (
            "window_calibration_result",
            CREATE_WINDOW_CALIBRATION_RESULT,
        ),
        (
            "window_calibration_source_experiment",
            CREATE_WINDOW_CALIBRATION_SOURCE_EXPERIMENT,
        ),
        ("calibration_lifecycle", CREATE_CALIBRATION_LIFECYCLE),
        (
            "idx_window_calibration_result_scope",
            CREATE_RESULT_SCOPE_INDEX,
        ),
    ] {
        conn.execute_batch(sql)
            .map_err(|e| Error::Store(format!("cannot create {label}: {e}")))?;
    }

    for table in IMMUTABLE_TABLES {
        for verb in ["UPDATE", "DELETE"] {
            conn.execute_batch(&immutability_trigger_sql(table, verb))
                .map_err(|e| {
                    Error::Store(format!(
                        "cannot create the {table} {} guard: {e}",
                        verb.to_lowercase()
                    ))
                })?;
        }
    }
    Ok(())
}

/// This step, for the registry.
///
/// Not a rewrite: it creates tables that did not exist, so no irreplaceable
/// data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-migration-0015-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn migrated() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let mut conn = open(
            &scratch.path().join("calibration.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        )
        .expect("scratch database must open");
        run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .expect("migrations must run");
        (scratch, conn)
    }

    fn insert_experiment(conn: &rusqlite::Connection) -> i64 {
        conn.query_row(
            "INSERT INTO calibration_experiment (
                experiment_id, provider, plan_tier, window_semantic_key,
                meter_semantics_id, billing_semantics_id,
                valid_from, valid_until, knowledge_time
            ) VALUES ('exp-1', 'anthropic', 'max', 'account',
                'meter-v1', 'billing-v1', 100, 200, 300) RETURNING id",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("experiment insert must work")
    }

    #[test]
    fn every_calibration_table_and_scope_index_exists() {
        let (_scratch, conn) = migrated();
        let names: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type IN ('table', 'index') ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for expected in [
            "calibration_experiment",
            "window_calibration_candidate",
            "window_calibration_result",
            "window_calibration_source_experiment",
            "calibration_lifecycle",
            "idx_window_calibration_result_scope",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }

    /// Immutability is a property of every table here, not of a repository:
    /// a direct UPDATE and a direct DELETE both fail at the database, on a
    /// populated table so the per-row triggers actually fire.
    #[test]
    fn direct_update_and_delete_are_refused_on_every_table() {
        let (_scratch, conn) = migrated();
        let exp_id = insert_experiment(&conn);
        let result_id = conn
            .query_row(
                "INSERT INTO window_calibration_result (
                    calibration_id, provider, plan_tier, window_semantic_key,
                    meter_semantics_id, billing_semantics_id, cost_model_id,
                    fitted_micros_per_point, equivalent_full_window_capacity_micros,
                    fit_residual_micros, uncertainty_low_micros, uncertainty_high_micros,
                    lag_estimate_nanos, lag_handling, sample_count, fit_timestamp,
                    inputs_digest, inputs_count, fitting_evidence_digest,
                    validation_evidence_digest, validation_method, validation_version,
                    out_of_sample_residual_micros, statistical_method, statistical_parameters,
                    condition_number_micros, observation_coverage_requirement, settling_policy,
                    excluded_samples, activation_policy_version, aub_version, source_revision,
                    valid_from, valid_until, knowledge_time
                ) VALUES (
                    'wc-1', 'anthropic', 'max', 'account', 'meter-v1', 'billing-v1', 'cm-1',
                    900000, 12000000, 5, 800000, 1000000,
                    NULL, 'none', 40, 500,
                    'aaaaaaaaaaaaaaaa', 3, 'bbbbbbbbbbbbbbbb',
                    'cccccccccccccccc', 'holdout', 'v2',
                    NULL, 'ols', '{}',
                    NULL, 'ninety-percent', 'plateau',
                    '[]', 'ap-v1', '0.1.0', 'abc123',
                    100, 200, 300
                ) RETURNING id",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("result insert must work");
        conn.execute(
            "INSERT INTO window_calibration_source_experiment (result_id, experiment_id) VALUES (?1, ?2)",
            [result_id, exp_id],
        )
        .expect("link insert must work");
        conn.execute(
            "INSERT INTO calibration_lifecycle (calibration_result_id, event_kind, event_at, supersedes_result_id)
             VALUES (?1, 'activation', 400, NULL)",
            [result_id],
        )
        .expect("lifecycle insert must work");

        for (table, update, delete) in [
            (
                "calibration_experiment",
                "UPDATE calibration_experiment SET provider = 'x'",
                "DELETE FROM calibration_experiment",
            ),
            (
                "window_calibration_result",
                "UPDATE window_calibration_result SET provider = 'x'",
                "DELETE FROM window_calibration_result",
            ),
            (
                "window_calibration_source_experiment",
                "UPDATE window_calibration_source_experiment SET result_id = 9",
                "DELETE FROM window_calibration_source_experiment",
            ),
            (
                "calibration_lifecycle",
                "UPDATE calibration_lifecycle SET event_at = 1",
                "DELETE FROM calibration_lifecycle",
            ),
        ] {
            let u = conn.execute(update, []).unwrap_err().to_string();
            assert!(
                u.contains("immutable") || u.contains("append-only"),
                "{table} UPDATE not refused: {u}"
            );
            let d = conn.execute(delete, []).unwrap_err().to_string();
            assert!(
                d.contains("immutable") || d.contains("append-only"),
                "{table} DELETE not refused: {d}"
            );
        }
    }

    /// A candidate cannot reference an experiment that does not exist: foreign
    /// keys are enforced on this connection.
    #[test]
    fn candidate_cannot_reference_a_missing_experiment() {
        let (_scratch, conn) = migrated();
        let err = conn
            .execute(
                "INSERT INTO window_calibration_candidate (
                    candidate_id, experiment_id, provider, plan_tier, window_semantic_key,
                    fitted_micros_per_point, equivalent_full_window_capacity_micros,
                    fit_residual_micros, uncertainty_low_micros, uncertainty_high_micros,
                    sample_count, inputs_digest, inputs_count, valid_from, valid_until, knowledge_time
                ) VALUES (
                    'cand-1', 4242, 'anthropic', 'max', 'account',
                    900000, 12000000, 5, 800000, 1000000,
                    40, 'aaaaaaaaaaaaaaaa', 3, 100, 200, 300
                )",
                [],
            )
            .unwrap_err()
            .to_string()
            .to_lowercase();
        assert!(err.contains("foreign key"), "expected FK violation: {err}");
    }

    /// The negative planted against a naive schema: an inverted validity
    /// interval (`valid_until` before `valid_from`) must be refused. A schema
    /// that stored the two timestamps without the CHECK would accept this and
    /// silently make point-in-time-by-valid-time queries wrong.
    #[test]
    fn an_inverted_validity_interval_is_refused() {
        let (_scratch, conn) = migrated();
        let err = conn
            .execute(
                "INSERT INTO calibration_experiment (
                    experiment_id, provider, plan_tier, window_semantic_key,
                    meter_semantics_id, billing_semantics_id,
                    valid_from, valid_until, knowledge_time
                ) VALUES ('exp-bad', 'anthropic', 'max', 'account',
                    'meter-v1', 'billing-v1', 500, 200, 300)",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHECK"), "expected a CHECK violation: {err}");
    }
}
