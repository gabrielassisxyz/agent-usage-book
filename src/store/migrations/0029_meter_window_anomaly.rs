//! Schema step: the window anomaly, calibration exclusion, and window-set
//! change tables (`aub-eun.14`).
//!
//! Consecutive `meter_window` readings of the same account, plan tier and
//! window identity are compared under typed reset semantics; a percentage
//! decrease without a legitimate reset, or a reset instant that changes
//! unexpectedly, is retained here as a typed `meter_window_anomaly` row
//! linking both `meter_window` rows it was found between. The original
//! `meter_observation` and `meter_window` rows are never touched: this is
//! read-only over them by construction, since no `UPDATE` or `DELETE`
//! statement against either table appears anywhere in this migration or in
//! the repository module built on it.
//!
//! `meter_calibration_exclusion` is the typed calibration-exclusion
//! annotation the affected interval carries, one per anomaly, naming the
//! interval boundaries so a downstream calibration fit can exclude it
//! without re-running detection (`aub-c0b.7`).
//!
//! `meter_window_set_change` is a different, unrelated event: a window
//! identity appearing or disappearing between two observations, not two
//! readings of the same window disagreeing. It is not an anomaly and
//! produces no exclusion; it is a structural fact for downstream constraint
//! selection to consume.
//!
//! All three tables are irreplaceable evidence: their triggers reject every
//! `UPDATE` and `DELETE`, matching every other table this module's sibling
//! migrations already added over `meter_observation` and `meter_window`.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 29;

const CREATE_WINDOW_ANOMALY_TABLES: &str = "
CREATE TABLE meter_window_anomaly (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (
        kind IN ('percentage_decrease_without_reset', 'unexpected_reset_change')
    ),
    account_id INTEGER NOT NULL REFERENCES account(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('account_wide', 'model_specific')),
    scoped_model TEXT,
    prior_observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    prior_window_id INTEGER NOT NULL REFERENCES meter_window(id),
    current_observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    current_window_id INTEGER NOT NULL REFERENCES meter_window(id),
    detected_at INTEGER NOT NULL,
    detail TEXT NOT NULL CHECK (length(detail) > 0),
    CHECK (
        (scope_kind = 'account_wide' AND scoped_model IS NULL)
        OR (scope_kind = 'model_specific' AND scoped_model IS NOT NULL)
    ),
    UNIQUE (prior_window_id, current_window_id)
) STRICT;

CREATE TABLE meter_calibration_exclusion (
    id INTEGER PRIMARY KEY,
    anomaly_id INTEGER NOT NULL UNIQUE REFERENCES meter_window_anomaly(id),
    account_id INTEGER NOT NULL REFERENCES account(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('account_wide', 'model_specific')),
    scoped_model TEXT,
    interval_start_at INTEGER NOT NULL,
    interval_end_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    CHECK (
        (scope_kind = 'account_wide' AND scoped_model IS NULL)
        OR (scope_kind = 'model_specific' AND scoped_model IS NOT NULL)
    ),
    CHECK (interval_end_at >= interval_start_at)
) STRICT;

CREATE TABLE meter_window_set_change (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (
        kind IN ('new_account_wide_window', 'missing_model_specific_window')
    ),
    account_id INTEGER NOT NULL REFERENCES account(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('account_wide', 'model_specific')),
    scoped_model TEXT,
    previous_observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    previous_window_id INTEGER REFERENCES meter_window(id),
    current_observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    current_window_id INTEGER REFERENCES meter_window(id),
    detected_at INTEGER NOT NULL,
    CHECK (
        (scope_kind = 'account_wide' AND scoped_model IS NULL)
        OR (scope_kind = 'model_specific' AND scoped_model IS NOT NULL)
    ),
    CHECK (
        (kind = 'new_account_wide_window' AND previous_window_id IS NULL
            AND current_window_id IS NOT NULL)
        OR (kind = 'missing_model_specific_window' AND current_window_id IS NULL
            AND previous_window_id IS NOT NULL)
    )
) STRICT;

-- A plain table-level UNIQUE over scoped_model would not dedupe two
-- account-wide rows (scoped_model NULL): SQL treats every NULL as distinct
-- from every other NULL in a uniqueness check, so a rerun over the same pair
-- of observations would insert a fresh account-wide row every time while the
-- model-scoped row correctly deduplicated. coalesce(scoped_model, '') gives
-- NULL a real, comparable value for this index only.
CREATE UNIQUE INDEX idx_meter_window_set_change_identity ON meter_window_set_change (
    kind, account_id, semantic_key, scope_kind, coalesce(scoped_model, ''),
    previous_observation_id, current_observation_id
);

CREATE TRIGGER meter_window_anomaly_rejects_update BEFORE UPDATE ON meter_window_anomaly
BEGIN
    SELECT RAISE(ABORT, 'meter_window_anomaly is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_window_anomaly_rejects_delete BEFORE DELETE ON meter_window_anomaly
BEGIN
    SELECT RAISE(ABORT, 'meter_window_anomaly is irreplaceable evidence; rows are never deleted');
END;

CREATE TRIGGER meter_calibration_exclusion_rejects_update BEFORE UPDATE ON meter_calibration_exclusion
BEGIN
    SELECT RAISE(ABORT, 'meter_calibration_exclusion is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_calibration_exclusion_rejects_delete BEFORE DELETE ON meter_calibration_exclusion
BEGIN
    SELECT RAISE(ABORT, 'meter_calibration_exclusion is irreplaceable evidence; rows are never deleted');
END;

CREATE TRIGGER meter_window_set_change_rejects_update BEFORE UPDATE ON meter_window_set_change
BEGIN
    SELECT RAISE(ABORT, 'meter_window_set_change is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_window_set_change_rejects_delete BEFORE DELETE ON meter_window_set_change
BEGIN
    SELECT RAISE(ABORT, 'meter_window_set_change is irreplaceable evidence; rows are never deleted');
END;

CREATE INDEX idx_meter_window_anomaly_account ON meter_window_anomaly (account_id);

CREATE INDEX idx_meter_calibration_exclusion_account ON meter_calibration_exclusion (account_id);

CREATE INDEX idx_meter_window_set_change_account ON meter_window_set_change (account_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_WINDOW_ANOMALY_TABLES)
        .map_err(|e| Error::Store(format!("cannot create the window anomaly tables: {e}")))
}

/// This step, for the registry.
///
/// Additive only: it creates tables that did not exist, so no irreplaceable
/// data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
