//! Schema step: the `attribution_segment` table (`aub-eu7.2`, PLAN.md 21,
//! 34.18).
//!
//! One row per (session, target) the segmentation algorithm in
//! `attribution::segment` assigned usage to: either a task (identified the
//! same way `task_event` identifies one, by source and native id) or a named
//! overhead bucket. Rebuildable, not append-only evidence: the segmentation
//! algorithm is pure over claim events and usage windows, so re-running it
//! after the tracker data changes is expected to change the attribution, and
//! the repository's write path replaces a session's rows wholesale rather
//! than patching them.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 16;

const CREATE_ATTRIBUTION_SEGMENT_TABLE: &str = "\
CREATE TABLE attribution_segment (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    task_source TEXT,
    task_native TEXT,
    overhead_reason TEXT,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    computed_at INTEGER NOT NULL,
    CHECK (length(session_id) > 0),
    CHECK (target_kind IN ('task', 'overhead')),
    CHECK (
        (target_kind = 'task' AND task_source IS NOT NULL AND task_native IS NOT NULL
            AND overhead_reason IS NULL)
        OR
        (target_kind = 'overhead' AND overhead_reason IS NOT NULL
            AND task_source IS NULL AND task_native IS NULL)
    ),
    CHECK (input_tokens >= 0),
    CHECK (output_tokens >= 0),
    CHECK (cache_read_tokens >= 0),
    CHECK (cache_write_tokens >= 0),
    UNIQUE (session_id, target_kind, task_source, task_native, overhead_reason)
) STRICT;

CREATE INDEX idx_attribution_segment_session ON attribution_segment (session_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_ATTRIBUTION_SEGMENT_TABLE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create the attribution_segment table: {error}"
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
