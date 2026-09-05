//! Schema step: durable identity and provenance for the legacy calibration import.
//!
//! The legacy regression fit imports as immutable calibration history, never as
//! an activatable record. Its import provenance lives here so a repeated import
//! of the same source is recognizable as the same history rather than a second
//! fit: the source content digest is the idempotence boundary.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 33;

const CREATE_LEGACY_CALIBRATION_IMPORT: &str = "
CREATE TABLE legacy_calibration_import (
    source_digest TEXT PRIMARY KEY CHECK (length(source_digest) = 64),
    verified_backup_id TEXT NOT NULL CHECK (length(verified_backup_id) > 0),
    imported_at INTEGER NOT NULL,
    calibration_id TEXT NOT NULL,
    records_read INTEGER NOT NULL CHECK (records_read >= 0),
    records_quarantined INTEGER NOT NULL CHECK (records_quarantined >= 0)
) STRICT;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_LEGACY_CALIBRATION_IMPORT)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create legacy calibration import table: {error}"
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
