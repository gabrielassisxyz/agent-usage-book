//! Schema step: the rebuildable transcript file index (`aub-lqe.2`).
//!
//! One row per (source key, relative path), holding the watermark that
//! decides whether a file is unchanged, was appended to, needs reparsing
//! because the parser changed, or requires a full rebuild (PLAN.md 12.7).
//!
//! The table is a rebuildable cache rather than evidence: transcripts remain
//! authoritative, so deleting the index only forces a full re-ingest, and
//! `aub rebuild` may address it. It is safe to recreate empty.

use crate::error::Error;
use crate::store::migrate::Migration;
use crate::store::transcript_file;

/// The schema version this step produces.
pub const VERSION: u32 = 6;

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
    transcript_file::create_table(conn)
}
