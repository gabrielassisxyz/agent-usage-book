//! Rebuildable routing axes from immutable tracker label candidates.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 46;

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(
        "ALTER TABLE task_identity ADD COLUMN verify TEXT CHECK (
            verify IS NULL OR verify IN ('local', 'gate', 'external')
        );
        ALTER TABLE task_identity ADD COLUMN verify_state TEXT NOT NULL DEFAULT 'unknown' CHECK (
            verify_state IN ('resolved', 'unknown', 'conflict')
            AND ((verify_state = 'resolved') = (verify IS NOT NULL))
        );
        ALTER TABLE task_identity ADD COLUMN verify_evidence TEXT NOT NULL DEFAULT '';
        ALTER TABLE task_identity ADD COLUMN spec TEXT CHECK (
            spec IS NULL OR spec IN ('closed', 'open')
        );
        ALTER TABLE task_identity ADD COLUMN spec_state TEXT NOT NULL DEFAULT 'unknown' CHECK (
            spec_state IN ('resolved', 'unknown', 'conflict')
            AND ((spec_state = 'resolved') = (spec IS NOT NULL))
        );
        ALTER TABLE task_identity ADD COLUMN spec_evidence TEXT NOT NULL DEFAULT '';",
    )
    .map_err(|error| Error::Store(format!("cannot add task routing identity columns: {error}")))
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: false,
        apply,
    }
}
