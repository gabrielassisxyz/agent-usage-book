//! Schema step: make the not-started meter window reset state storable (`aub-wu69`).
//!
//! An idle window reports `resets_at` as null (`WindowResetState::NotStarted`).
//! This additive migration makes `resets_at` nullable with a `CHECK` that ties null
//! to a new `reset_state` column (`'known' | 'not_started'`), with existing rows
//! reading back as `Known`.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 25;

const MAKE_METER_WINDOW_RESET_STATE_STORABLE: &str = "\
DROP TRIGGER IF EXISTS meter_window_rejects_update;
ALTER TABLE meter_window ADD COLUMN old_resets_at INTEGER;
UPDATE meter_window SET old_resets_at = resets_at;
ALTER TABLE meter_window DROP COLUMN resets_at;
ALTER TABLE meter_window ADD COLUMN resets_at INTEGER;
UPDATE meter_window SET resets_at = old_resets_at;
ALTER TABLE meter_window DROP COLUMN old_resets_at;
ALTER TABLE meter_window ADD COLUMN reset_state TEXT NOT NULL DEFAULT 'known' CHECK (
    reset_state IN ('known', 'not_started')
    AND ((reset_state = 'known') = (resets_at IS NOT NULL))
);
CREATE TRIGGER meter_window_rejects_update BEFORE UPDATE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never updated');
END;
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(MAKE_METER_WINDOW_RESET_STATE_STORABLE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot make meter window reset state storable: {error}"
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
