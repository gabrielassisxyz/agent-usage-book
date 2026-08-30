//! Schema step: the per-account sampling lease table (`aub-me5.4`).
//!
//! The lease is operational metadata rather than measurement evidence, which
//! decides three things about the table below. It carries no foreign key to the
//! account table: a lease has to be acquirable before any account row exists,
//! and dropping every lease can never damage an evidence chain. It has one row
//! per account, so the account key is the primary key and the row is replaced
//! rather than accumulated, because no lease state is worth reconstructing.
//! And it is safe to recreate empty, which is what lets `doctor --fix` clear
//! expired rows and lets rebuild ignore the table entirely (PLAN.md section
//! 14.2).

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 2;

/// One row per account, holding at most one live lease.
///
/// Times are Unix nanoseconds, the representation
/// [`crate::domain::time::UtcTimestamp`] round-trips without loss. The
/// `expires_at > acquired_at` check makes a zero-length lease unrepresentable
/// in the table as well as refused at the API, so a lease that expires the
/// instant it is granted cannot exist even if a caller reaches SQLite by
/// another path.
const CREATE_SAMPLING_LEASE: &str = "\
CREATE TABLE sampling_lease (
    account_name TEXT PRIMARY KEY,
    holder TEXT NOT NULL,
    acquired_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    CHECK (length(account_name) > 0),
    CHECK (length(holder) > 0),
    CHECK (expires_at > acquired_at)
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
    conn.execute_batch(CREATE_SAMPLING_LEASE)
        .map_err(|e| Error::Store(format!("cannot create the sampling_lease table: {e}")))
}
