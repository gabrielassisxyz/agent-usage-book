//! Schema step: the `account_attribution_segment` table (`aub-mgv.1`,
//! PLAN.md 12.6, 19.2, 34.17).
//!
//! One row per (session, target) the marker-interval segmentation algorithm
//! in `attribution::account_segment` assigned usage to: either a named
//! account or the explicit unknown-account bucket. Rebuildable, not
//! append-only evidence: the segmentation algorithm is pure over account
//! markers and usage events, so re-running it after the marker set changes
//! is expected to change the attribution, and the repository's write path
//! replaces a session's rows wholesale rather than patching them.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 21;

const CREATE_ACCOUNT_ATTRIBUTION_SEGMENT_TABLE: &str = "\
CREATE TABLE account_attribution_segment (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    logical_account TEXT,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    computed_at INTEGER NOT NULL,
    CHECK (length(session_id) > 0),
    CHECK (target_kind IN ('account', 'unknown_account')),
    CHECK (
        (target_kind = 'account' AND logical_account IS NOT NULL)
        OR
        (target_kind = 'unknown_account' AND logical_account IS NULL)
    ),
    CHECK (input_tokens >= 0),
    CHECK (output_tokens >= 0),
    CHECK (cache_read_tokens >= 0),
    CHECK (cache_write_tokens >= 0),
    UNIQUE (session_id, target_kind, logical_account)
) STRICT;

CREATE INDEX idx_account_attribution_segment_session ON account_attribution_segment (session_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_ACCOUNT_ATTRIBUTION_SEGMENT_TABLE)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create the account_attribution_segment table: {error}"
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
