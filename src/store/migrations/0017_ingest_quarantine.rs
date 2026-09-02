//! Schema step: the `ingest_quarantine` table (`aub-lqe.6`, PLAN.md 12.11).
//!
//! Captures source material that could not be normalized, keyed by an excerpt
//! hash rather than the excerpt text by default (settled by aub-2r3,
//! 2026-08-25: hash and byte offset stored, no excerpt text by default; bounded
//! redacted excerpt only under explicit diagnostic policy).
//!
//! A quarantine row is never cleared by the clearing verb (`aub-smqu`), but
//! is rebuildable and addressable by `aub rebuild`.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 17;

const CREATE_INGEST_QUARANTINE_TABLE: &str = "\
CREATE TABLE ingest_quarantine (
    id INTEGER PRIMARY KEY,
    source_file TEXT NOT NULL,
    byte_offset INTEGER,
    line_number INTEGER,
    parser TEXT NOT NULL,
    failure_class TEXT NOT NULL,
    excerpt_hash TEXT NOT NULL,
    excerpt TEXT,
    first_observed INTEGER NOT NULL,
    last_observed INTEGER NOT NULL,
    CHECK (length(source_file) > 0),
    CHECK (byte_offset IS NULL OR byte_offset >= 0),
    CHECK (line_number IS NULL OR line_number >= 0),
    CHECK (length(parser) > 0),
    CHECK (length(failure_class) > 0),
    CHECK (length(excerpt_hash) > 0),
    CHECK (first_observed >= 0),
    CHECK (last_observed >= first_observed),
    UNIQUE (source_file, parser, failure_class, excerpt_hash)
) STRICT;

CREATE INDEX idx_ingest_quarantine_doctor ON ingest_quarantine (parser, failure_class);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_INGEST_QUARANTINE_TABLE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create the ingest_quarantine table: {error}"
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
