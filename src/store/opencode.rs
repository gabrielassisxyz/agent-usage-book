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

use std::collections::BTreeMap;
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

/// One message part of an opencode session transcript: the message identity
/// and role from the `message` row, the raw `part.data` JSON beside it. The
/// role is the only interpreted field here (it decides which conversation
/// side a part belongs to); every part shape stays raw JSON, and the
/// transcript renderer owns how each part type maps, the way the usage parser
/// owns the token vocabulary inside `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeTranscriptRow {
    /// The stable message identifier (`message.id`), grouping parts of one turn.
    pub message_id: String,
    /// The message role from `message.data.role` (`user` or `assistant`),
    /// empty when the row carries none.
    pub role: String,
    /// The stable part identifier (`part.id`).
    pub part_id: String,
    /// The raw `part.data` JSON: the part `type` and its payload.
    pub part_data: String,
}

/// Reads every part of one opencode session in conversation order: messages by
/// their row time, parts within a message by theirs, ties broken by id so the
/// read is deterministic across runs. A session pruned from the database reads
/// as no rows; the caller tells that apart from an empty session through
/// [`opencode_session_exists`].
pub fn read_session_transcript_rows(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<OpencodeTranscriptRow>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT m.id, m.data, p.id, p.data FROM message m \
             JOIN part p ON p.message_id = m.id \
             WHERE m.session_id = ?1 \
             ORDER BY m.time_created, m.id, p.time_created, p.id",
        )
        .map_err(|error| {
            Error::IngestIncomplete(format!("cannot read opencode transcript: {error}"))
        })?;
    let rows = statement
        .query_map([session_id], |row| {
            let message_data: String = row.get(1)?;
            let role = serde_json::from_str::<serde_json::Value>(&message_data)
                .ok()
                .and_then(|data| data.get("role")?.as_str().map(str::to_string))
                .unwrap_or_default();
            Ok(OpencodeTranscriptRow {
                message_id: row.get(0)?,
                role,
                part_id: row.get(2)?,
                part_data: row.get(3)?,
            })
        })
        .map_err(|error| {
            Error::IngestIncomplete(format!("cannot query opencode transcript: {error}"))
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|error| {
        Error::IngestIncomplete(format!("cannot decode opencode transcript row: {error}"))
    })
}

/// Whether the opencode `session` table holds the id: a ledger session with no
/// rows from [`read_session_transcript_rows`] and no session row here was
/// pruned from the database, which the transcript export reports by id rather
/// than rendering an empty document.
pub fn opencode_session_exists(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<bool, Error> {
    let mut statement = connection
        .prepare("SELECT 1 FROM session WHERE id = ?1")
        .map_err(|error| {
            Error::IngestIncomplete(format!("cannot read opencode sessions: {error}"))
        })?;
    let mut rows = statement.query([session_id]).map_err(|error| {
        Error::IngestIncomplete(format!("cannot query opencode sessions: {error}"))
    })?;
    rows.next().map(|row| row.is_some()).map_err(|error| {
        Error::IngestIncomplete(format!("cannot decode opencode session row: {error}"))
    })
}
///
/// The working directories the opencode `session` table states, by session id.
///
/// The message rows carry no directory; the session row does
/// (`session.directory`), so the transcript parser joins them here rather
/// than guessing. An empty directory reads as absent, the way an absent one
/// does.
///
/// A database without a readable `session` table yields no directories rather
/// than refusing the parse: the directory is additive context for project
/// resolution, and its absence must not discard the usage the message table
/// holds.
pub fn read_session_directories(connection: &rusqlite::Connection) -> BTreeMap<String, String> {
    let mut statement = match connection.prepare("SELECT id, directory FROM session") {
        Ok(statement) => statement,
        Err(_) => return BTreeMap::new(),
    };
    let rows = match statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        Ok(rows) => rows,
        Err(_) => return BTreeMap::new(),
    };
    let mut directories = BTreeMap::new();
    for row in rows {
        if let Ok((id, directory)) = row
            && !directory.is_empty()
        {
            directories.insert(id, directory);
        }
    }
    directories
}
