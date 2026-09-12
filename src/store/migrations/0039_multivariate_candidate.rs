//! Schema step: the multivariate calibration candidate (`aub-73xa`).
//!
//! A controlled experiment whose premise names more than one token kind is
//! fitted jointly, one coefficient per kind, and that shape does not fit the
//! univariate candidate row: `window_calibration_candidate` carries a single
//! credits-per-percentage-point scalar priced through the rate book, while a
//! joint fit answers what one token of each kind moves the meter by, before
//! any rate book is assumed. So the joint candidate gets its own pair of
//! tables rather than nullable columns bolted onto the scalar one: the
//! univariate row stays byte-identical for every reader it already has, and
//! the per-kind rows are one row per kind rather than four column groups
//! that would silently rot when a fifth kind arrived.
//!
//! Both tables are immutable like every other calibration record: a candidate
//! is evidence, and evidence is never rewritten. Nothing here is ever
//! activated by the fitter; activation stays with `calibrate activate` and
//! its thresholds (`docs/INVARIANTS.md`, row 29).
//!
//! Recovery: the framework is forward-only, so there is no down step to run.
//! The manual reversal below drops both tables; it is exercised by this
//! module's own round-trip test, never by production code.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 39;

const CREATE_MULTIVARIATE_CANDIDATE: &str = "\
CREATE TABLE window_calibration_multivariate_candidate (
    id INTEGER PRIMARY KEY,
    candidate_id TEXT NOT NULL UNIQUE,
    calibration_controlled_run_id INTEGER NOT NULL REFERENCES calibration_controlled_run(id),
    provider TEXT NOT NULL,
    plan_tier TEXT NOT NULL,
    window_semantic_key TEXT NOT NULL,
    condition_number_micros INTEGER NOT NULL,
    condition_number_threshold_micros INTEGER NOT NULL,
    fit_residual_ppm INTEGER NOT NULL,
    sample_count INTEGER NOT NULL,
    inputs_digest TEXT NOT NULL,
    inputs_count INTEGER NOT NULL,
    statistical_method TEXT NOT NULL,
    statistical_parameters TEXT NOT NULL,
    phase_design TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_until INTEGER NOT NULL,
    knowledge_time INTEGER NOT NULL,
    CHECK (length(candidate_id) > 0),
    CHECK (length(provider) > 0),
    CHECK (length(plan_tier) > 0),
    CHECK (length(window_semantic_key) > 0),
    CHECK (condition_number_micros >= 1000000),
    CHECK (condition_number_threshold_micros > 1000000),
    CHECK (fit_residual_ppm >= 0 AND fit_residual_ppm <= 1000000),
    CHECK (sample_count >= 0),
    CHECK (length(inputs_digest) = 16),
    CHECK (inputs_count >= 0),
    CHECK (length(statistical_method) > 0),
    CHECK (length(phase_design) > 0),
    CHECK (valid_until >= valid_from)
) STRICT";

const CREATE_MULTIVARIATE_COEFFICIENT: &str = "\
CREATE TABLE window_calibration_multivariate_coefficient (
    id INTEGER PRIMARY KEY,
    window_calibration_multivariate_candidate_id INTEGER NOT NULL
        REFERENCES window_calibration_multivariate_candidate(id),
    token_kind TEXT NOT NULL,
    estimate_micro_ppm_per_token INTEGER NOT NULL,
    std_error_micro_ppm_per_token INTEGER NOT NULL,
    interval_low_micro_ppm_per_token INTEGER NOT NULL,
    interval_high_micro_ppm_per_token INTEGER NOT NULL,
    UNIQUE (window_calibration_multivariate_candidate_id, token_kind),
    CHECK (token_kind IN ('input', 'output', 'cache_read', 'cache_write')),
    CHECK (estimate_micro_ppm_per_token > 0),
    CHECK (std_error_micro_ppm_per_token >= 0),
    CHECK (interval_high_micro_ppm_per_token >= interval_low_micro_ppm_per_token)
) STRICT";

const IMMUTABLE_TABLES: [&str; 2] = [
    "window_calibration_multivariate_candidate",
    "window_calibration_multivariate_coefficient",
];

fn immutability_trigger_sql(table: &str, verb: &str) -> String {
    format!(
        "CREATE TRIGGER {table}_no_{lower_verb} BEFORE {verb} ON {table}
BEGIN
    SELECT RAISE(ABORT, '{table} is immutable: {lower_verb} refused');
END",
        lower_verb = verb.to_lowercase(),
    )
}

/// The manual reversal, for the round-trip test below. Production code never
/// runs it: the framework is forward-only.
#[cfg(test)]
const DROP_MULTIVARIATE_TABLES: &str = "
DROP TABLE window_calibration_multivariate_coefficient;
DROP TABLE window_calibration_multivariate_candidate;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    for (label, sql) in [
        (
            "window_calibration_multivariate_candidate",
            CREATE_MULTIVARIATE_CANDIDATE,
        ),
        (
            "window_calibration_multivariate_coefficient",
            CREATE_MULTIVARIATE_COEFFICIENT,
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
                        "cannot create the {table} {verb} immutability trigger: {e}"
                    ))
                })?;
        }
    }
    Ok(())
}

/// This step, for the registry.
///
/// Additive only: two new empty tables, so no irreplaceable data is at risk
/// and the verified-backup guard does not apply.
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

    fn scratch_path() -> std::path::PathBuf {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "aub-migration-0039-test-{}-{suffix}.sqlite3",
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

    fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .expect("sqlite_master must answer")
            == 1
    }

    /// The tables arrive with the step, the manual reversal removes them, and
    /// re-applying the step brings them back.
    #[test]
    fn multivariate_tables_round_trip_through_up_and_down() {
        let path = scratch_path();
        let conn = open_at_full_schema(&path);
        for table in IMMUTABLE_TABLES {
            assert!(table_exists(&conn, table), "{table} must exist after up");
        }
        conn.execute_batch(DROP_MULTIVARIATE_TABLES)
            .expect("manual reversal must run");
        for table in IMMUTABLE_TABLES {
            assert!(
                !table_exists(&conn, table),
                "{table} must be gone after down"
            );
        }
        apply(&conn).expect("re-applying the step must work");
        for table in IMMUTABLE_TABLES {
            assert!(
                table_exists(&conn, table),
                "{table} must be back after re-up"
            );
        }
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// A coefficient row cannot name a kind the domain does not have, cannot
    /// carry a non-positive estimate, and cannot be rewritten once written.
    #[test]
    fn coefficient_rows_are_checked_and_immutable() {
        let path = scratch_path();
        let conn = open_at_full_schema(&path);
        let at = UtcTimestamp::from_unix_nanos(1_000);
        crate::store::calibrate_cli_test_ledger::insert_calibrate_cli_meter_chain(
            &conn,
            "acct",
            "five_hour",
            at,
            10_000,
        );
        let baseline_id: i64 = conn
            .query_row("SELECT id FROM meter_observation LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("the fixture observation must exist");
        let run = crate::store::calibration_controlled::ControlledExperimentRun {
            id: crate::store::calibration_controlled::ControlledExperimentId::new("exp-1"),
            account: "acct".to_string(),
            provider: crate::store::cost_model::ProviderKey::new("anthropic"),
            plan_tier: crate::store::calibration::PlanTier::new("pro"),
            window_semantic_key: crate::domain::window::WindowSemanticKey::new("five_hour"),
            cost_model_id: crate::domain::provenance::CostModelId::new("cm"),
            expected_token_kinds: vec![
                crate::domain::tokens::TokenKind::Input,
                crate::domain::tokens::TokenKind::Output,
            ],
            baseline_observation_id: crate::store::meter_evidence::ObservationRowId::new(
                baseline_id,
            ),
            baseline_quota_used: crate::domain::quota::QuotaUsed::new(
                crate::domain::quota::QuotaFractionPpm::new(10_000).expect("valid"),
            ),
            baseline_resolution: crate::domain::window::ReportedResolution::new(
                crate::domain::quota::QuotaFractionPpm::new(10_000).expect("valid"),
            )
            .expect("non-zero"),
            baseline_observed_at: at,
            baseline_plateau_started_at: at,
            contamination_thresholds:
                crate::calibration::contamination::ContaminationThresholds::conservative_default(),
            started_at: at,
            ended_at: None,
            exclusivity_assertion: "reserved".to_string(),
        };
        let run_row = crate::store::calibration_controlled::insert_begin(&conn, &run)
            .expect("the controlled run must insert");
        conn.execute(
            "INSERT INTO window_calibration_multivariate_candidate (id, candidate_id, calibration_controlled_run_id, provider, plan_tier, window_semantic_key, condition_number_micros, condition_number_threshold_micros, fit_residual_ppm, sample_count, inputs_digest, inputs_count, statistical_method, statistical_parameters, phase_design, valid_from, valid_until, knowledge_time) VALUES (1, 'mvcand-1', ?1, 'anthropic', 'pro', 'five_hour', 1000000, 30000000, 0, 8, '0123456789abcdef', 8, 'ols', '{}', 'design', 0, 10, 10)",
            [run_row],
        )
        .expect("the candidate row must insert");

        let insert = |kind: &str, estimate: i64| {
            conn.execute(
                "INSERT INTO window_calibration_multivariate_coefficient (window_calibration_multivariate_candidate_id, token_kind, estimate_micro_ppm_per_token, std_error_micro_ppm_per_token, interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token) VALUES (1, ?1, ?2, 0, ?2, ?2)",
                rusqlite::params![kind, estimate],
            )
        };
        insert("input", 500_000).expect("a valid coefficient row must insert");
        assert!(
            insert("embedding", 1).is_err(),
            "an unknown kind must be refused"
        );
        assert!(
            insert("output", 0).is_err(),
            "a zero estimate must be refused"
        );
        assert!(
            insert("input", 7).is_err(),
            "a second row for one kind must be refused"
        );

        let rewrite = conn.execute(
            "UPDATE window_calibration_multivariate_coefficient SET estimate_micro_ppm_per_token = 1",
            [],
        );
        assert!(rewrite.is_err(), "a coefficient row is immutable");
        let erase = conn.execute("DELETE FROM window_calibration_multivariate_candidate", []);
        assert!(erase.is_err(), "a candidate row is immutable");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
