//! Schema step: the session heartbeat table (`aub-mgv.5`, PLAN.md 19.2, 43 Workflow 2).
//!
//! A heartbeat is contemporary liveness evidence, independent of which account a
//! session ran under (`session_account_marker` owns that fact). It answers a
//! different question: is the session still there. Unlike a marker, only the most
//! recent heartbeat matters, so the table keeps one row per session and advances it
//! in place rather than appending history the live report never reads.
//!
//! Operational, not measurement evidence: disposable, and outside the irreplaceable
//! evidence chain, the same standing `sampling_lease` has.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 25;

const CREATE_SESSION_HEARTBEAT: &str = "CREATE TABLE session_heartbeat (
    id INTEGER PRIMARY KEY,
    session_source TEXT NOT NULL,
    session_native TEXT NOT NULL,
    last_heartbeat_at INTEGER NOT NULL,
    heartbeat_source TEXT NOT NULL,
    UNIQUE (session_source, session_native),
    CHECK (length(session_source) > 0),
    CHECK (length(session_native) > 0),
    CHECK (length(heartbeat_source) > 0)
) STRICT;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_SESSION_HEARTBEAT)
        .map_err(|e| Error::Store(format!("cannot create session_heartbeat table: {e}")))
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
