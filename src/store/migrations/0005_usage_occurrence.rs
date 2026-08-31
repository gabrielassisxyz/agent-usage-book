//! Schema step: the `usage_occurrence` table's identity columns (`aub-lqe.7`).
//!
//! PLAN.md 12.10: the database uniqueness constraint is the final deduplication
//! authority, and strong and heuristic identity use separate uniqueness
//! domains. This migration carries exactly the columns that constraint needs;
//! the remaining occurrence metadata (canonical event ID, parser version
//! beyond the domain key, canonical payload digest) is `aub-lqe.8`'s table
//! work, added by a later migration once `usage_event` exists to reference.
//!
//! `UNIQUE(source_namespace, native_event_id)` is the strong domain: a native
//! identifier is a claim the source made. `UNIQUE(parser_version,
//! heuristic_key)` is the heuristic domain, scoped per parser so one parser's
//! replay-equivalence domain is never read against another's. SQLite treats
//! NULL as distinct from every other NULL in a UNIQUE index, so a
//! heuristic-only row (`native_event_id` NULL) never collides with another
//! heuristic-only row through the strong constraint, and a strong-identity row
//! (`heuristic_key` NULL) never collides through the heuristic one: the two
//! domains do not interact at the database boundary either.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 5;

const CREATE_USAGE_OCCURRENCE: &str = "\
CREATE TABLE usage_occurrence (
    id INTEGER PRIMARY KEY,
    source_namespace TEXT NOT NULL,
    native_event_id TEXT,
    parser_version TEXT NOT NULL,
    heuristic_key TEXT,
    source_file TEXT NOT NULL,
    occurred_at INTEGER,
    CHECK (length(source_namespace) > 0),
    CHECK (length(parser_version) > 0),
    CHECK (length(source_file) > 0),
    CHECK (native_event_id IS NOT NULL OR heuristic_key IS NOT NULL),
    UNIQUE (source_namespace, native_event_id),
    UNIQUE (parser_version, heuristic_key)
) STRICT";

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
    conn.execute_batch(CREATE_USAGE_OCCURRENCE)
        .map_err(|e| Error::Store(format!("cannot create the usage_occurrence table: {e}")))
}
