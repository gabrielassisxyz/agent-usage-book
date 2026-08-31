//! Schema step: the session and account marker table (`aub-sth.16`).
//!
//! Markers record which account a session was running under (PLAN.md sections 6,
//! 11.5, 12.6, 19.2, 32). They are irreplaceable evidence: hook invocations and
//! launcher markers cannot be reconstructed after the fact.
//!
//! The table is append-only in ordinary operation and is declared in the retention
//! policy as a forever class.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 4;

const CREATE_SESSION_ACCOUNT_MARKER: &str = "CREATE TABLE session_account_marker (
    id INTEGER PRIMARY KEY,
    session_source TEXT NOT NULL,
    session_native TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    source_ordering_key INTEGER,
    logical_account TEXT NOT NULL,
    resolved_account_id INTEGER REFERENCES account(id),
    marker_source TEXT NOT NULL,
    run_source TEXT,
    run_native TEXT,
    evidence_designation TEXT NOT NULL,
    CHECK (length(session_source) > 0),
    CHECK (length(session_native) > 0),
    CHECK (length(logical_account) > 0),
    CHECK (length(marker_source) > 0),
    CHECK (length(evidence_designation) > 0)
) STRICT;

CREATE INDEX idx_session_account_marker_lookup ON session_account_marker (
    session_source,
    session_native,
    observed_at,
    source_ordering_key
);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_SESSION_ACCOUNT_MARKER)
        .map_err(|e| Error::Store(format!("cannot create session_account_marker table: {e}")))
}

/// This step, for the registry.
///
/// Additive only: it creates a table that did not exist, so no irreplaceable
/// data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
