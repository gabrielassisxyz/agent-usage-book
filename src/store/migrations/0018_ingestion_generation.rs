//! Schema step: the ingestion generation counter (`aub-xus.7`).
//!
//! Mirrors the ledger generation counter (`0003_ledger_generation.rs`): one
//! row, seeded at zero, never reset. The export header carries both
//! generations so a consumer can tell which data state an export was produced
//! from; the ingest path advances this counter when it lands (`aub-lqe.11`).

use crate::error::Error;
use crate::store::ingestion_generation;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 18;

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
    ingestion_generation::create_table(conn)
}
