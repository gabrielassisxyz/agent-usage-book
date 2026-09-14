//! Schema step: a multivariate coefficient may sit at zero (`aub-bupy`).
//!
//! The joint fitter reports a kind that costs nothing at its fitted value, a
//! hair either side of zero, and refuses only an estimate more than two
//! standard errors below zero (`aub-msub`). Migration 0039 declared
//! `estimate_micro_ppm_per_token > 0`, so the one finding the controlled burst
//! was designed to produce, cache reads moving the meter by nothing, could not
//! be stored. The CHECK now states the fitter's own rule, so the store still
//! refuses exactly what the fitter refuses and nothing it accepts.
//!
//! SQLite changes a CHECK only by rebuilding the table, the shape migration
//! 0034 established: drop the immutability triggers, create the replacement
//! with the new constraint, copy every row with its `id`, drop the old table,
//! rename, recreate the triggers. No table references the coefficient table,
//! so foreign keys stay on; the rows reference their candidate, which is not
//! touched.
//!
//! Recovery: the framework is forward-only. Rebuilding again with the old
//! `> 0` CHECK undoes it, and fails only if a coefficient at or below zero was
//! recorded after this step; the test below exercises that reversal.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 42;

const TABLE: &str = "window_calibration_multivariate_coefficient";

fn rebuild_sql(estimate_check: &str) -> String {
    format!(
        "\
DROP TRIGGER IF EXISTS {TABLE}_no_update;
DROP TRIGGER IF EXISTS {TABLE}_no_delete;
CREATE TABLE {TABLE}_0042 (
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
    CHECK ({estimate_check}),
    CHECK (std_error_micro_ppm_per_token >= 0),
    CHECK (interval_high_micro_ppm_per_token >= interval_low_micro_ppm_per_token)
) STRICT;
INSERT INTO {TABLE}_0042 (
    id, window_calibration_multivariate_candidate_id, token_kind,
    estimate_micro_ppm_per_token, std_error_micro_ppm_per_token,
    interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token
)
SELECT
    id, window_calibration_multivariate_candidate_id, token_kind,
    estimate_micro_ppm_per_token, std_error_micro_ppm_per_token,
    interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token
FROM {TABLE}
ORDER BY id;
DROP TABLE {TABLE};
ALTER TABLE {TABLE}_0042 RENAME TO {TABLE};
CREATE TRIGGER {TABLE}_no_update BEFORE UPDATE ON {TABLE}
BEGIN
    SELECT RAISE(ABORT, '{TABLE} is immutable: update refused');
END;
CREATE TRIGGER {TABLE}_no_delete BEFORE DELETE ON {TABLE}
BEGIN
    SELECT RAISE(ABORT, '{TABLE} is immutable: delete refused');
END;
"
    )
}

const WITHIN_TWO_STANDARD_ERRORS_OF_ZERO: &str =
    "estimate_micro_ppm_per_token + 2 * std_error_micro_ppm_per_token >= 0";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(&rebuild_sql(WITHIN_TWO_STANDARD_ERRORS_OF_ZERO))
        .map_err(|e| Error::Store(format!("cannot rebuild {TABLE} with the sign rule: {e}")))
}

/// This step, for the registry.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        // Every row is copied unchanged, so no verified backup is demanded.
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
            "aub-migration-0042-test-{}-{suffix}.sqlite3",
            std::process::id()
        ))
    }

    fn open_at(path: &std::path::Path, through_version: u32) -> rusqlite::Connection {
        let mut conn = open(
            path,
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(500),
            },
        )
        .expect("scratch database must open");
        let steps = registry()
            .into_iter()
            .filter(|step| step.version <= through_version)
            .collect::<Vec<_>>();
        run_migrations(
            &mut conn,
            &steps,
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .expect("the registry must apply");
        conn
    }

    fn seed_candidate(conn: &rusqlite::Connection) {
        let at = UtcTimestamp::from_unix_nanos(1_000);
        crate::store::calibrate_cli_test_ledger::insert_calibrate_cli_meter_chain(
            conn,
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
        let resolution = crate::domain::quota::QuotaFractionPpm::new(10_000).expect("valid");
        let run = crate::store::calibration_controlled::ControlledExperimentRun {
            id: crate::store::calibration_controlled::ControlledExperimentId::new("exp-1"),
            account: "acct".to_string(),
            provider: crate::store::cost_model::ProviderKey::new("anthropic"),
            plan_tier: crate::store::calibration::PlanTier::new("pro"),
            window_semantic_key: crate::domain::window::WindowSemanticKey::new("five_hour"),
            cost_model_id: crate::domain::provenance::CostModelId::new("cm"),
            expected_token_kinds: vec![
                crate::domain::tokens::TokenKind::Input,
                crate::domain::tokens::TokenKind::CacheRead,
            ],
            baseline_observation_id: crate::store::meter_evidence::ObservationRowId::new(
                baseline_id,
            ),
            baseline_quota_used: crate::domain::quota::QuotaUsed::new(resolution),
            baseline_resolution: crate::domain::window::ReportedResolution::new(resolution)
                .expect("non-zero"),
            baseline_observed_at: at,
            baseline_plateau_started_at: at,
            contamination_thresholds:
                crate::calibration::contamination::ContaminationThresholds::conservative_default(),
            started_at: at,
            ended_at: None,
            exclusivity_assertion: "reserved".to_string(),
        };
        let run_id = crate::store::calibration_controlled::insert_begin(conn, &run)
            .expect("the controlled run must insert");
        conn.execute(
            "INSERT INTO window_calibration_multivariate_candidate (id, candidate_id, calibration_controlled_run_id, provider, plan_tier, window_semantic_key, condition_number_micros, condition_number_threshold_micros, fit_residual_ppm, sample_count, inputs_digest, inputs_count, statistical_method, statistical_parameters, phase_design, valid_from, valid_until, knowledge_time) VALUES (1, 'mvcand-1', ?1, 'anthropic', 'pro', 'five_hour', 1000000, 30000000, 0, 8, '0123456789abcdef', 8, 'ols', '{}', 'design', 0, 10, 10)",
            [run_id],
        )
        .expect("the candidate row must insert");
    }

    fn insert_coefficient(
        conn: &rusqlite::Connection,
        kind: &str,
        estimate: i64,
        std_error: i64,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO window_calibration_multivariate_coefficient (window_calibration_multivariate_candidate_id, token_kind, estimate_micro_ppm_per_token, std_error_micro_ppm_per_token, interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token) VALUES (1, ?1, ?2, ?3, ?2 - 2 * ?3, ?2 + 2 * ?3)",
            rusqlite::params![kind, estimate, std_error],
        )
    }

    fn rows(conn: &rusqlite::Connection) -> Vec<(i64, String, i64, i64, i64, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT id, token_kind, estimate_micro_ppm_per_token, std_error_micro_ppm_per_token, interval_low_micro_ppm_per_token, interval_high_micro_ppm_per_token FROM window_calibration_multivariate_coefficient ORDER BY id",
            )
            .expect("select must prepare");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("select must run")
        .collect::<Result<_, _>>()
        .expect("rows must read")
    }

    /// A row written under 0039 survives the rebuild unchanged, the new rule
    /// admits a free kind and refuses a clearly negative one, and the table is
    /// still immutable.
    #[test]
    fn the_rebuild_keeps_rows_and_admits_a_coefficient_at_zero() {
        let path = scratch_path();
        let conn = open_at(&path, VERSION - 1);
        seed_candidate(&conn);
        insert_coefficient(&conn, "input", 1_322_702, 217_148)
            .expect("a positive coefficient inserts under 0039");
        assert!(
            insert_coefficient(&conn, "cache_read", -2_953, 168_705).is_err(),
            "0039 refuses the free kind this step exists for"
        );
        let before = rows(&conn);
        drop(conn);

        let conn = open_at(&path, VERSION);
        assert_eq!(rows(&conn), before, "rows copied byte-for-byte");
        insert_coefficient(&conn, "cache_read", -2_953, 168_705)
            .expect("a coefficient within two errors of zero inserts");
        assert!(
            insert_coefficient(&conn, "output", -500_000, 100_000).is_err(),
            "five errors below zero is refused"
        );
        assert!(
            conn.execute(
                "UPDATE window_calibration_multivariate_coefficient SET estimate_micro_ppm_per_token = 1",
                [],
            )
            .is_err(),
            "update still refused"
        );
        assert!(
            conn.execute(
                "DELETE FROM window_calibration_multivariate_coefficient",
                []
            )
            .is_err(),
            "delete still refused"
        );
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }

    /// Rebuilding with the old CHECK is the manual reversal; it holds on a
    /// table with only positive rows.
    #[test]
    fn rebuilding_with_the_old_check_reverses_the_step() {
        let path = scratch_path();
        let conn = open_at(&path, VERSION);
        seed_candidate(&conn);
        insert_coefficient(&conn, "input", 500_000, 1_000).expect("positive row inserts");
        conn.execute_batch(&rebuild_sql("estimate_micro_ppm_per_token > 0"))
            .expect("the reversal applies");
        assert!(insert_coefficient(&conn, "cache_read", 0, 1_000).is_err());
        apply(&conn).expect("re-applying the step works");
        insert_coefficient(&conn, "cache_read", 0, 1_000).expect("zero admitted again");
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
