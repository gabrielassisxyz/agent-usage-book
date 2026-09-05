//! Schema step: record the settled-boundary policy on each calibration experiment.
//!
//! The policy is an immutable experiment snapshot. Defaults preserve the conservative
//! settled-boundary decision for experiments created before this migration; new rows
//! write their complete role-specific values through the calibration repository.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 25;

const ADD_SETTLEMENT_POLICY_COLUMNS: &str = "\
ALTER TABLE calibration_experiment ADD COLUMN settlement_policy_version TEXT NOT NULL DEFAULT 'settled-boundary-v1' CHECK (length(settlement_policy_version) > 0);\
ALTER TABLE calibration_experiment ADD COLUMN baseline_sampling_interval_nanos INTEGER NOT NULL DEFAULT 300000000000 CHECK (baseline_sampling_interval_nanos > 0);\
ALTER TABLE calibration_experiment ADD COLUMN baseline_observation_count INTEGER NOT NULL DEFAULT 3 CHECK (baseline_observation_count >= 2);\
ALTER TABLE calibration_experiment ADD COLUMN baseline_minimum_span_nanos INTEGER NOT NULL DEFAULT 600000000000 CHECK (baseline_minimum_span_nanos > 0);\
ALTER TABLE calibration_experiment ADD COLUMN baseline_max_change_resolution_units INTEGER NOT NULL DEFAULT 0 CHECK (baseline_max_change_resolution_units >= 0);\
ALTER TABLE calibration_experiment ADD COLUMN baseline_maximum_settlement_window_nanos INTEGER NOT NULL DEFAULT 3600000000000 CHECK (baseline_maximum_settlement_window_nanos > 0);\
ALTER TABLE calibration_experiment ADD COLUMN baseline_reported_resolution_ppm INTEGER NOT NULL DEFAULT 10000 CHECK (baseline_reported_resolution_ppm >= 1 AND baseline_reported_resolution_ppm <= 1000000);\
ALTER TABLE calibration_experiment ADD COLUMN terminal_sampling_interval_nanos INTEGER NOT NULL DEFAULT 300000000000 CHECK (terminal_sampling_interval_nanos > 0);\
ALTER TABLE calibration_experiment ADD COLUMN terminal_observation_count INTEGER NOT NULL DEFAULT 3 CHECK (terminal_observation_count >= 2);\
ALTER TABLE calibration_experiment ADD COLUMN terminal_minimum_span_nanos INTEGER NOT NULL DEFAULT 600000000000 CHECK (terminal_minimum_span_nanos > 0);\
ALTER TABLE calibration_experiment ADD COLUMN terminal_max_change_resolution_units INTEGER NOT NULL DEFAULT 0 CHECK (terminal_max_change_resolution_units >= 0);\
ALTER TABLE calibration_experiment ADD COLUMN terminal_maximum_settlement_window_nanos INTEGER NOT NULL DEFAULT 3600000000000 CHECK (terminal_maximum_settlement_window_nanos > 0);\
ALTER TABLE calibration_experiment ADD COLUMN terminal_reported_resolution_ppm INTEGER NOT NULL DEFAULT 10000 CHECK (terminal_reported_resolution_ppm >= 1 AND terminal_reported_resolution_ppm <= 1000000);\
ALTER TABLE calibration_experiment ADD COLUMN settlement_shared_criteria_reason TEXT NOT NULL DEFAULT 'baseline and terminal share the conservative provider-lag criterion';\
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_SETTLEMENT_POLICY_COLUMNS)
        .map_err(|error| {
            Error::Store(format!(
                "cannot add calibration settlement policy columns: {error}"
            ))
        })
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
