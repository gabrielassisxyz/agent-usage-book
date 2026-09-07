//! Schema step: make the `Scheduled` grid-computed meter window reset state
//! storable (`aub-ud17`).
//!
//! Ollama Cloud's usage endpoint reports no reset instant, so the adapter
//! computes one from a fixed grid instead of from the response. This is
//! additive: a third `reset_state` value (`'scheduled'`), and a nullable
//! `reset_grid` column naming which grid a `'scheduled'` row's instant was
//! computed from. Every pre-existing row keeps `reset_state` in
//! `('known', 'not_started')` with `reset_grid` null, unchanged.
//!
//! The recreated `CHECK` extends 0025's rule (`resets_at` non-null exactly
//! when the state carries an instant) with a matching rule for the new
//! column: `reset_grid` non-null exactly when the state is `'scheduled'`.
//! SQLite cannot alter an existing `CHECK` in place, so `reset_state` is
//! dropped and re-added under the wider constraint, the same dance 0025 used
//! to storage-migrate `resets_at` itself.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 34;

const MAKE_SCHEDULED_RESET_STATE_STORABLE: &str = "\
DROP TRIGGER IF EXISTS meter_window_rejects_update;
ALTER TABLE meter_window ADD COLUMN reset_grid TEXT;
ALTER TABLE meter_window ADD COLUMN old_reset_state TEXT;
UPDATE meter_window SET old_reset_state = reset_state;
ALTER TABLE meter_window DROP COLUMN reset_state;
ALTER TABLE meter_window ADD COLUMN reset_state TEXT NOT NULL DEFAULT 'known' CHECK (
    reset_state IN ('known', 'not_started', 'scheduled')
    AND ((resets_at IS NULL) = (reset_state = 'not_started'))
    AND ((reset_grid IS NOT NULL) = (reset_state = 'scheduled'))
);
UPDATE meter_window SET reset_state = old_reset_state;
ALTER TABLE meter_window DROP COLUMN old_reset_state;
CREATE TRIGGER meter_window_rejects_update BEFORE UPDATE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never updated');
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
        rewrites_irreplaceable: false,
        apply,
    }
}
