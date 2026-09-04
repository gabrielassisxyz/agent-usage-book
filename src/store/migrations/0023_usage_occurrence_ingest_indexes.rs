//! Schema step: two indexes on `usage_occurrence` the ingest persist path
//! needs to stay cheap (`aub-mh1c`).
//!
//! Profiled 2026-09-04 over a real transcript corpus: `persist_ingest_batch`'s
//! own orphan cleanup, `DELETE FROM usage_event WHERE NOT EXISTS (SELECT 1
//! FROM usage_occurrence o WHERE o.event_id = usage_event.id)`, ran a full
//! scan of `usage_occurrence` for every row of `usage_event` with no index on
//! `usage_occurrence.event_id` to search instead
//! (`sqlite_autoindex_usage_event_1` covers `usage_event`'s own uniqueness,
//! not this lookup). On a ledger already holding tens of thousands of rows
//! that scan dominated the batch: over 98% of one profiled ingest pass's
//! measured time, growing with the square of the ledger's size within a
//! single pass since both tables grow across the pass's own batches. The
//! per-file replacement delete, `DELETE FROM usage_occurrence WHERE
//! source_file = ?1`, carried the same shape on `source_file`.
//!
//! Both are additive: no existing row changes, and the query plans that used
//! `SCAN usage_occurrence` before this migration now read `SEARCH ... USING
//! INDEX`.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 23;

const CREATE_USAGE_OCCURRENCE_INGEST_INDEXES: &str = "\
CREATE INDEX idx_usage_occurrence_event_id ON usage_occurrence (event_id);
CREATE INDEX idx_usage_occurrence_source_file ON usage_occurrence (source_file);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_USAGE_OCCURRENCE_INGEST_INDEXES)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create the usage_occurrence ingest indexes: {error}"
            ))
        })
}

/// This step, for the registry.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
