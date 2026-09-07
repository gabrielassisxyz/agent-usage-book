//! Migration 0034: make the scheduled reset state storable (`aub-ud17`).
//!
//! `WindowResetState::Scheduled { at, grid }` is a reset instant `aub` computed
//! from a provider's fixed grid rather than one the provider reported, and the
//! store keeps that distinction: `reset_state` gains the value `scheduled` and
//! a `reset_grid` column names the grid, present exactly when the state is
//! `scheduled`. The instant itself still lives in `resets_at`, so a scheduled
//! window is `NULL`-free there like a known one, and only `not_started` leaves
//! it empty.
//!
//! WHY this rebuilds the table instead of dropping and re-adding the column
//! (`aub-leed`): SQLite validates a new column's CHECK against every existing
//! row at `ADD COLUMN` time, using the column's default for those rows. A
//! default of `known` under a CHECK that ties `resets_at IS NULL` to
//! `not_started` fails on the first `not_started` row the table holds, and
//! the live ledger held 317 of them when the first version of this migration
//! rolled back against it. A constraint on an existing column changes only
//! through the rebuild SQLite's own ALTER TABLE guide prescribes: a new table
//! with the full constraint, every row copied across, the old table dropped,
//! the new one renamed into place, then the index and the evidence triggers
//! recreated. The runner switches foreign keys off around the step (the
//! `rebuilds_referenced_table` flag below) and refuses to commit unless the
//! foreign key check comes back empty; the copy keeps every `id`, so the four
//! referencing tables resolve against the rebuilt table.
//!
//! The column order of the rebuilt table matches what the drop-and-add would
//! have produced (`reset_grid` then `reset_state` at the end), so a ledger
//! migrated by either shape reads the same.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 34;

const MAKE_SCHEDULED_RESET_STATE_STORABLE: &str = "\
DROP TRIGGER IF EXISTS meter_window_rejects_update;
DROP TRIGGER IF EXISTS meter_window_rejects_delete;
DROP INDEX IF EXISTS idx_meter_window_observation;
CREATE TABLE meter_window_0034 (
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
INSERT INTO meter_window_0034 (
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, nominal_duration_nanos, resets_at,
    is_active, severity, reset_grid, reset_state
)
SELECT
    id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
    reported_resolution_ppm, quantization, nominal_duration_nanos, resets_at,
    is_active, severity, NULL, reset_state
FROM meter_window
ORDER BY id;
DROP TABLE meter_window;
ALTER TABLE meter_window_0034 RENAME TO meter_window;
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
    conn.execute_batch(MAKE_SCHEDULED_RESET_STATE_STORABLE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot make the scheduled meter window reset state storable: {error}"
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
