//! Schema step: contamination thresholds on the controlled experiment run (`aub-c0b.6`).
//!
//! Each contamination signal's threshold is configuration recorded on the experiment
//! rather than a source constant, so detection reads the row. `begin` fills these
//! columns once from the conservative defaults (or explicit configuration); the
//! rebuilt no-rewrite trigger keeps every `begin` column immutable afterwards, with
//! the single `NULL -> set` transition of `ended_at` still the only permitted
//! mutation. `baseline_plateau_started_at` is the idle plateau period `begin`
//! asserted: the pre-burn check examines only that window.
//!
//! Existing rows predate contamination detection and therefore asserted no plateau
//! and recorded no thresholds. They backfill to the conservative defaults, and the
//! plateau start backfills to the baseline observation instant (a degenerate
//! plateau, which keeps the pre-burn check vacuous for those rows rather than
//! inventing a period nobody asserted). SQLite cannot reference another column in
//! a `DEFAULT`, so the backfill for the plateau column is a plain constant and
//! `begin` always writes the real value for new rows.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 31;

const ADD_CONTAMINATION_COLUMNS: &str = "\
ALTER TABLE calibration_controlled_run ADD COLUMN baseline_plateau_started_at INTEGER NOT NULL DEFAULT 0;\
ALTER TABLE calibration_controlled_run ADD COLUMN contamination_policy_version TEXT NOT NULL DEFAULT 'contamination-v1' CHECK (length(contamination_policy_version) > 0);\
ALTER TABLE calibration_controlled_run ADD COLUMN contamination_pre_burn_max_movement_ppm INTEGER NOT NULL DEFAULT 10000 CHECK (contamination_pre_burn_max_movement_ppm >= 0 AND contamination_pre_burn_max_movement_ppm <= 1000000);\
ALTER TABLE calibration_controlled_run ADD COLUMN contamination_post_settlement_max_movement_ppm INTEGER NOT NULL DEFAULT 10000 CHECK (contamination_post_settlement_max_movement_ppm >= 0 AND contamination_post_settlement_max_movement_ppm <= 1000000);\
ALTER TABLE calibration_controlled_run ADD COLUMN contamination_post_settlement_grace_nanos INTEGER NOT NULL DEFAULT 3600000000000 CHECK (contamination_post_settlement_grace_nanos > 0);\
ALTER TABLE calibration_controlled_run ADD COLUMN contamination_flat_meter_min_movement_ppm INTEGER NOT NULL DEFAULT 20000 CHECK (contamination_flat_meter_min_movement_ppm >= 0 AND contamination_flat_meter_min_movement_ppm <= 1000000);\
ALTER TABLE calibration_controlled_run ADD COLUMN contamination_flat_local_max_micros INTEGER NOT NULL DEFAULT 0 CHECK (contamination_flat_local_max_micros >= 0);\
";

const DROP_NO_REWRITE: &str = "DROP TRIGGER calibration_controlled_run_no_rewrite";

const NO_REWRITE: &str = "\
CREATE TRIGGER calibration_controlled_run_no_rewrite BEFORE UPDATE ON calibration_controlled_run
WHEN NOT (
    OLD.ended_at IS NULL
    AND NEW.ended_at IS NOT NULL
    AND NEW.ended_at >= OLD.started_at
    AND OLD.experiment_id = NEW.experiment_id
    AND OLD.account = NEW.account
    AND OLD.provider = NEW.provider
    AND OLD.plan_tier = NEW.plan_tier
    AND OLD.window_semantic_key = NEW.window_semantic_key
    AND OLD.cost_model_id = NEW.cost_model_id
    AND OLD.expected_token_kinds = NEW.expected_token_kinds
    AND OLD.baseline_observation_id = NEW.baseline_observation_id
    AND OLD.baseline_quota_used_ppm = NEW.baseline_quota_used_ppm
    AND OLD.baseline_reported_resolution_ppm = NEW.baseline_reported_resolution_ppm
    AND OLD.baseline_observed_at = NEW.baseline_observed_at
    AND OLD.started_at = NEW.started_at
    AND OLD.exclusivity_assertion = NEW.exclusivity_assertion
    AND OLD.baseline_plateau_started_at = NEW.baseline_plateau_started_at
    AND OLD.contamination_policy_version = NEW.contamination_policy_version
    AND OLD.contamination_pre_burn_max_movement_ppm = NEW.contamination_pre_burn_max_movement_ppm
    AND OLD.contamination_post_settlement_max_movement_ppm = NEW.contamination_post_settlement_max_movement_ppm
    AND OLD.contamination_post_settlement_grace_nanos = NEW.contamination_post_settlement_grace_nanos
    AND OLD.contamination_flat_meter_min_movement_ppm = NEW.contamination_flat_meter_min_movement_ppm
    AND OLD.contamination_flat_local_max_micros = NEW.contamination_flat_local_max_micros
)
BEGIN
    SELECT RAISE(ABORT, 'calibration_controlled_run is append-only: only record_end may set ended_at once');
END";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_CONTAMINATION_COLUMNS)
        .map_err(|e| Error::Store(format!("cannot add contamination columns: {e}")))?;
    conn.execute_batch(DROP_NO_REWRITE)
        .map_err(|e| Error::Store(format!("cannot drop the controlled-run rewrite guard: {e}")))?;
    conn.execute_batch(NO_REWRITE).map_err(|e| {
        Error::Store(format!(
            "cannot rebuild the controlled-run rewrite guard: {e}"
        ))
    })?;
    Ok(())
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
