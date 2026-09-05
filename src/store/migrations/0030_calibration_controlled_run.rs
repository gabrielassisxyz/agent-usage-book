//! Schema step: the controlled calibration experiment run table (`aub-c0b.2`).
//!
//! A controlled experiment is a record, not a session: `calibrate begin` writes
//! the premise, the process exits, the scheduler keeps sampling, and
//! `calibrate end` later records the end of controlled local work. Both
//! commands must see the same row, so the row lives in the ledger, and a
//! simulated reboot (reopening the database with no in-memory state) loses
//! nothing.
//!
//! One row per experiment, keyed by its semantic `experiment_id`. The `begin`
//! columns are immutable; the only permitted mutation is the single
//! `NULL -> set` transition of `ended_at` performed by the repository's
//! narrowly scoped `record_end`, enforced here at the database by a trigger
//! that refuses every other `UPDATE` shape. `DELETE` is refused outright.
//! This mirrors the lifecycle-events pattern of the calibration tables without
//! a second table for a transition that happens at most once.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 30;

const CREATE_CALIBRATION_CONTROLLED_RUN: &str = "\
CREATE TABLE calibration_controlled_run (
    id INTEGER PRIMARY KEY,
    experiment_id TEXT NOT NULL UNIQUE,
    account TEXT NOT NULL,
    provider TEXT NOT NULL,
    plan_tier TEXT NOT NULL,
    window_semantic_key TEXT NOT NULL,
    cost_model_id TEXT NOT NULL,
    expected_token_kinds TEXT NOT NULL,
    baseline_observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    baseline_quota_used_ppm INTEGER NOT NULL
        CHECK (baseline_quota_used_ppm >= 0 AND baseline_quota_used_ppm <= 1000000),
    baseline_reported_resolution_ppm INTEGER NOT NULL
        CHECK (baseline_reported_resolution_ppm >= 1 AND baseline_reported_resolution_ppm <= 1000000),
    baseline_observed_at INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    exclusivity_assertion TEXT NOT NULL,
    CHECK (length(experiment_id) > 0),
    CHECK (length(account) > 0),
    CHECK (length(provider) > 0),
    CHECK (length(plan_tier) > 0),
    CHECK (length(window_semantic_key) > 0),
    CHECK (length(cost_model_id) > 0),
    CHECK (length(expected_token_kinds) > 0),
    CHECK (length(exclusivity_assertion) > 0),
    CHECK (ended_at IS NULL OR ended_at >= started_at)
) STRICT";

const CREATE_CONTROLLED_RUN_ACCOUNT_INDEX: &str = "\
CREATE INDEX idx_calibration_controlled_run_account ON calibration_controlled_run (
    account, ended_at
)";

const NO_DELETE: &str = "\
CREATE TRIGGER calibration_controlled_run_no_delete BEFORE DELETE ON calibration_controlled_run
BEGIN
    SELECT RAISE(ABORT, 'calibration_controlled_run is append-only: DELETE refused');
END";

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
)
BEGIN
    SELECT RAISE(ABORT, 'calibration_controlled_run is append-only: only record_end may set ended_at once');
END";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    for (label, sql) in [
        (
            "calibration_controlled_run",
            CREATE_CALIBRATION_CONTROLLED_RUN,
        ),
        (
            "idx_calibration_controlled_run_account",
            CREATE_CONTROLLED_RUN_ACCOUNT_INDEX,
        ),
        ("calibration_controlled_run_no_delete", NO_DELETE),
        ("calibration_controlled_run_no_rewrite", NO_REWRITE),
    ] {
        conn.execute_batch(sql)
            .map_err(|e| Error::Store(format!("cannot create {label}: {e}")))?;
    }
    Ok(())
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
