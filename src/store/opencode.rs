//! Read-only access to the opencode session database.
//!
//! opencode keeps every session in one SQLite database (`opencode.db`) instead
//! of the line-delimited transcript files the other sources write, so the
//! transcript layer cannot glob and parse it as text. This module owns the one
//! narrow thing the transcript layer needs from that file: opening it
//! read-only and reading the usage-carrying rows of its `message` table. The
//! payload stays raw JSON here; the token vocabulary inside it is the parser's
//! business (`crate::transcripts::native`), the way the tracker reader in
//! `task_event.rs` returns records a normalizer elsewhere interprets.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::path::Path;

use crate::domain::time::MonotonicDuration;
use crate::error::Error;
use crate::store::connection::{self, AccessMode, PragmaPolicy};

/// One usage-carrying row of the opencode `message` table: the stable
/// identifiers and the row timestamp as columns, the per-message payload as
/// raw JSON text. The transcript parser decides which roles carry usage and
/// how the token fields map; this module never interprets either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeMessageRow {
    /// The stable message identifier (`message.id`), the canonical event id.
    pub message_id: String,
    /// The session the message belongs to (`message.session_id`).
    pub session_id: String,
    /// The row timestamp in milliseconds since the Unix epoch.
    pub time_created_ms: i64,
    /// The raw `data` JSON: role, model, timestamps and the `tokens` object.
    pub data: String,
}

/// Opens the opencode session database read-only, for reading message rows.
///
/// Routed through [`connection::open`] with [`AccessMode::ForeignReadOnly`],
/// the same shape as the tracker reader in `task_event.rs`: no pragma policy
/// is applied or verified, because the journal mode and durability settings
/// this project's own ledger is built to hold encode assumptions about a
/// schema this project controls, and the opencode database belongs to a
/// different program. The busy timeout below is unused for this mode; it
/// exists only because `open` takes one policy value for every mode. Never
/// opened for write: this is the live store of a program the operator uses.
pub fn open_opencode_database(path: &Path) -> Result<rusqlite::Connection, Error> {
    connection::open(
        path,
        AccessMode::ForeignReadOnly,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_seconds(0),
        },
    )
}

/// Reads every row of the opencode `message` table in insertion order, so a
/// parse over the rows is deterministic across runs. User messages, assistant
/// messages and malformed payloads all come back here; filtering and
/// quarantining them is the parser's job, because only the parser knows which
/// roles carry usage.
pub fn read_message_rows(
    connection: &rusqlite::Connection,
) -> Result<Vec<OpencodeMessageRow>, Error> {
    let mut statement = connection
        .prepare("SELECT id, session_id, time_created, data FROM message ORDER BY rowid")
        .map_err(|error| {
            Error::IngestIncomplete(format!("cannot read opencode messages: {error}"))
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok(OpencodeMessageRow {
                message_id: row.get(0)?,
                session_id: row.get(1)?,
                time_created_ms: row.get(2)?,
                data: row.get(3)?,
            })
        })
        .map_err(|error| {
            Error::IngestIncomplete(format!("cannot query opencode messages: {error}"))
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
        Error::IngestIncomplete(format!("cannot decode opencode message row: {error}"))
    })
}
