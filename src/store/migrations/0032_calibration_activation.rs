//! Schema step: the activation event carries its own audit trail (`aub-c0b.8`).
//!
//! Activation is an explicit append-only event carrying the actor, the
//! timestamp, the activation policy version and the evidence it relied on, so
//! the lifecycle table gains four columns: who activated, under which policy
//! version, and the fitting and validation evidence digests the activation
//! judged. The timestamp was already there as `event_at`.
//!
//! Existing rows predate the audit trail. The actor backfills to `unknown`
//! because no record says who activated those rows, while the policy version
//! and both digests backfill from the referenced result row, which is the
//! best available statement of what those activations relied on. The backfill
//! runs while the lifecycle rewrite guard is down and the guard is rebuilt
//! identically afterwards, following the `aub-c0b.6` precedent, so the
//! append-only property never changes meaning.
//!
//! `rewrites_irreplaceable` is false: no pre-existing column is written, only
//! the added columns are filled, and every filled value re-derives from a row
//! the database already holds.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 32;

const ADD_ACTIVATION_COLUMNS: &str = "\
ALTER TABLE calibration_lifecycle ADD COLUMN actor TEXT NOT NULL DEFAULT 'unknown' CHECK (length(actor) > 0);\
ALTER TABLE calibration_lifecycle ADD COLUMN activation_policy_version TEXT NOT NULL DEFAULT 'unknown' CHECK (length(activation_policy_version) > 0);\
ALTER TABLE calibration_lifecycle ADD COLUMN fitting_evidence_digest TEXT NOT NULL DEFAULT '0000000000000000' CHECK (length(fitting_evidence_digest) = 16);\
ALTER TABLE calibration_lifecycle ADD COLUMN validation_evidence_digest TEXT NOT NULL DEFAULT '0000000000000000' CHECK (length(validation_evidence_digest) = 16);\
";

const DROP_NO_UPDATE: &str = "DROP TRIGGER calibration_lifecycle_no_update";

const BACKFILL_FROM_RESULT: &str = "\
UPDATE calibration_lifecycle SET
    activation_policy_version = (SELECT activation_policy_version FROM window_calibration_result WHERE id = calibration_lifecycle.calibration_result_id),
    fitting_evidence_digest = (SELECT fitting_evidence_digest FROM window_calibration_result WHERE id = calibration_lifecycle.calibration_result_id),
    validation_evidence_digest = (SELECT validation_evidence_digest FROM window_calibration_result WHERE id = calibration_lifecycle.calibration_result_id)";

const NO_UPDATE: &str = "\
CREATE TRIGGER calibration_lifecycle_no_update BEFORE UPDATE ON calibration_lifecycle
BEGIN
    SELECT RAISE(ABORT, 'calibration_lifecycle is append-only: update refused');
END";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_ACTIVATION_COLUMNS)
        .map_err(|e| Error::Store(format!("cannot add activation columns: {e}")))?;
    conn.execute_batch(DROP_NO_UPDATE)
        .map_err(|e| Error::Store(format!("cannot drop the lifecycle rewrite guard: {e}")))?;
    conn.execute_batch(BACKFILL_FROM_RESULT)
        .map_err(|e| Error::Store(format!("cannot backfill the activation audit trail: {e}")))?;
    conn.execute_batch(NO_UPDATE)
        .map_err(|e| Error::Store(format!("cannot rebuild the lifecycle rewrite guard: {e}")))?;
    Ok(())
}

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
                "aub-migration-0032-test-{}-{suffix}",
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

    fn migrate_to(conn: &mut rusqlite::Connection, max_version: u32) {
        let selected: Vec<_> = crate::store::migrations::registry()
            .into_iter()
            .filter(|m| m.version <= max_version)
            .collect();
        run_migrations(
            conn,
            &selected,
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(1_000)),
        )
        .expect("selected migrations must run");
    }

    /// A pre-trail activation row backfills its policy version and both
    /// digests from the referenced result, keeps `unknown` as its actor, and
    /// the rebuilt guard still refuses rewrites afterwards.
    #[test]
    fn pre_trail_events_backfill_from_the_referenced_result() {
        let scratch = ScratchDir::new();
        let mut conn = open(
            &scratch.path().join("activation.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        )
        .expect("scratch database must open");
        migrate_to(&mut conn, 31);

        conn.execute(
            "INSERT INTO calibration_experiment (
                experiment_id, provider, plan_tier, window_semantic_key,
                meter_semantics_id, billing_semantics_id,
                valid_from, valid_until, knowledge_time
            ) VALUES ('exp-1', 'anthropic', 'max', 'account',
                'meter-v1', 'billing-v1', 100, 200, 300)",
            [],
        )
        .expect("experiment insert must work");
        conn.execute(
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
                'aaaaaaaaaaaaaaaa', 3, 'dddddddddddddddd',
                'eeeeeeeeeeeeeeee', 'holdout', 'v2',
                NULL, 'ols', '{}',
                NULL, 'ninety-percent', 'plateau',
                '[]', 'ap-v1', '0.1.0', 'abc123',
                100, 200, 300
            )",
            [],
        )
        .expect("result insert must work");
        conn.execute(
            "INSERT INTO calibration_lifecycle (calibration_result_id, event_kind, event_at, supersedes_result_id)
             VALUES (1, 'activation', 400, NULL)",
            [],
        )
        .expect("lifecycle insert must work");

        migrate_to(&mut conn, 32);

        let (actor, policy, fitting, validation): (String, String, String, String) = conn
            .query_row(
                "SELECT actor, activation_policy_version, fitting_evidence_digest,
                    validation_evidence_digest FROM calibration_lifecycle",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("backfilled row must read");
        assert_eq!(actor, "unknown");
        assert_eq!(policy, "ap-v1");
        assert_eq!(fitting, "dddddddddddddddd");
        assert_eq!(validation, "eeeeeeeeeeeeeeee");

        let err = conn
            .execute("UPDATE calibration_lifecycle SET event_at = 1", [])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("append-only"),
            "rebuilt guard must refuse rewrites: {err}"
        );
    }
}
