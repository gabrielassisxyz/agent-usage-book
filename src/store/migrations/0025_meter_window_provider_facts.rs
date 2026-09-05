//! Schema step: retain provider activity and severity facts on meter windows.
//!
//! These fields are provider observations, not derived selection state. Existing
//! rows receive explicit compatibility defaults because the older contracts did
//! not report either fact.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 25;

const ADD_PROVIDER_WINDOW_FACTS: &str = "\
ALTER TABLE meter_window ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1 CHECK (
    is_active IN (0, 1)
);
ALTER TABLE meter_window ADD COLUMN severity TEXT NOT NULL DEFAULT 'unknown' CHECK (
    length(severity) > 0
);
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_PROVIDER_WINDOW_FACTS)
        .map_err(|error| Error::Store(format!("cannot add meter window provider facts: {error}")))
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
