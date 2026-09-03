//! Schema step: durable identity and provenance for the legacy meter import.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 19;

const CREATE_LEGACY_IMPORT_TABLES: &str = "
CREATE TABLE legacy_meter_import (
    source_digest TEXT PRIMARY KEY CHECK (length(source_digest) = 64),
    verified_backup_id TEXT NOT NULL CHECK (length(verified_backup_id) > 0),
    imported_at INTEGER NOT NULL,
    records_read INTEGER NOT NULL CHECK (records_read >= 0),
    records_quarantined INTEGER NOT NULL CHECK (records_quarantined >= 0)
) STRICT;

CREATE TABLE legacy_meter_import_record (
    source_digest TEXT NOT NULL REFERENCES legacy_meter_import(source_digest),
    source_line INTEGER NOT NULL CHECK (source_line > 0),
    observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    marker_id INTEGER NOT NULL REFERENCES session_account_marker(id),
    PRIMARY KEY (source_digest, source_line),
    UNIQUE (observation_id),
    UNIQUE (marker_id)
) STRICT;

CREATE INDEX idx_legacy_meter_import_observation
    ON legacy_meter_import_record (observation_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_LEGACY_IMPORT_TABLES)
        .map_err(|error| Error::Store(format!("cannot create legacy meter import tables: {error}")))
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
