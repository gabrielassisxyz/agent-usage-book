//! Schema step: the `ledger_generation` counter (`aub-sth.9`).
//!
//! One row, seeded at zero. The table's own module owns every read and write
//! of it from here on; this step exists only to create and seed it once, and
//! is never touched again (a version, once applied, is never edited).

use crate::error::Error;
use crate::store::ledger_generation::create_table;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 3;

/// This step, for the registry.
///
/// Not a rewrite: it creates a table that did not exist, so no irreplaceable
/// data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    create_table(conn)
}
