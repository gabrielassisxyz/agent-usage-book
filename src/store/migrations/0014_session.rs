//! Schema step: the normalized `session` table (`aub-lqe.12`, PLAN.md 12.8, 19.1,
//! 19.3).
//!
//! The session is the join that makes everything else possible, so it is normalized
//! immediately and namespaced by its source: `UNIQUE (source, native_session_id)`
//! makes two textually identical native identifiers from different CLIs distinct
//! rows, which is the collision the design names as a real risk once two tools pick
//! the same identifier format.
//!
//! The row deliberately carries no mandatory account column: account assignment
//! changes over time and belongs to the marker timeline, and a column here would
//! silently pick one account for a session that used two (PLAN.md 12.8, 19.2).
//!
//! Project and repository are typed logical identities resolved through configured
//! aliases, never absolute machine paths: the columns store the logical key, and
//! unmapped work lands in the `unknown-project` / `unknown-repository` buckets so it
//! stays inside totals rather than disappearing from them (PLAN.md 19.3).
//!
//! Sessions are rebuildable from usage events, so this table carries no immutability
//! trigger: the rebuild path deletes and re-creates rows, and the repository exposes
//! that path explicitly.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 14;

const CREATE_SESSION: &str = "\
CREATE TABLE session (
    id INTEGER PRIMARY KEY,
    source TEXT NOT NULL,
    native_session_id TEXT NOT NULL,
    start INTEGER NOT NULL,
    end INTEGER,
    project_key TEXT NOT NULL,
    repository_key TEXT NOT NULL,
    run_id TEXT,
    CHECK (length(source) > 0),
    CHECK (length(native_session_id) > 0),
    CHECK (length(project_key) > 0),
    CHECK (length(repository_key) > 0),
    CHECK (run_id IS NULL OR length(run_id) > 0),
    CHECK (end IS NULL OR end >= start),
    UNIQUE (source, native_session_id)
) STRICT";

/// This step, for the registry.
///
/// Not a rewrite: it creates a table that did not exist, so no irreplaceable data is
/// at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_SESSION)
        .map_err(|e| Error::Store(format!("cannot create the session table: {e}")))
}
