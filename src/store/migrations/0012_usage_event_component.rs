//! Schema step: the `usage_event`, `usage_component`, and expanded `usage_occurrence` tables (`aub-lqe.8`).
//!
//! PLAN.md sections 12.9, 12.10, 18:
//!
//! A canonical logical event is stored once in `usage_event`, its token
//! components are stored as child rows in `usage_component` (preserving unknown
//! token classes without schema modifications), and every place the event was
//! observed is recorded in `usage_occurrence`.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 12;

const CREATE_USAGE_EVENT_TABLES: &str = "\
CREATE TABLE usage_event (
    id INTEGER PRIMARY KEY,
    canonical_event_id TEXT NOT NULL,
    session_id TEXT,
    event_timestamp INTEGER,
    model_id TEXT,
    evidence_kind TEXT NOT NULL CHECK (length(evidence_kind) > 0),
    source_provenance TEXT NOT NULL CHECK (length(source_provenance) > 0),
    parser_version TEXT NOT NULL CHECK (length(parser_version) > 0),
    created_at INTEGER NOT NULL,
    CHECK (length(canonical_event_id) > 0),
    CHECK (session_id IS NULL OR length(session_id) > 0),
    CHECK (model_id IS NULL OR length(model_id) > 0),
    UNIQUE (canonical_event_id)
) STRICT;

CREATE TABLE usage_component (
    id INTEGER PRIMARY KEY,
    event_id INTEGER NOT NULL REFERENCES usage_event(id) ON DELETE CASCADE,
    token_class TEXT NOT NULL CHECK (length(token_class) > 0),
    count INTEGER NOT NULL CHECK (count >= 0),
    UNIQUE (event_id, token_class)
) STRICT;

ALTER TABLE usage_occurrence ADD COLUMN event_id INTEGER REFERENCES usage_event(id) ON DELETE CASCADE;
ALTER TABLE usage_occurrence ADD COLUMN transcript_file_id TEXT;
ALTER TABLE usage_occurrence ADD COLUMN source_location TEXT;
ALTER TABLE usage_occurrence ADD COLUMN canonical_fingerprint TEXT;
ALTER TABLE usage_occurrence ADD COLUMN identity_strength TEXT NOT NULL DEFAULT 'strong' CHECK (identity_strength IN ('strong', 'heuristic'));
ALTER TABLE usage_occurrence ADD COLUMN heuristic_algorithm_version TEXT;
ALTER TABLE usage_occurrence ADD COLUMN canonical_payload_digest TEXT;
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_USAGE_EVENT_TABLES)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create usage event and component tables: {error}"
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
