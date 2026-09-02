//! The `usage_event` table repository (`aub-lqe.8`).
//!
//! Stores the canonical logical event once, independent of how many times it was
//! replayed in transcript history (PLAN.md 12.9, 12.10).

use rusqlite::{OptionalExtension, params};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;

/// An event row's identity, by SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(i64);

impl EventId {
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A new canonical usage event to be inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUsageEvent<'a> {
    pub canonical_event_id: &'a str,
    pub session_id: Option<&'a str>,
    pub event_timestamp: Option<UtcTimestamp>,
    pub model_id: Option<&'a str>,
    pub evidence_kind: &'a str,
    pub source_provenance: &'a str,
    pub parser_version: &'a str,
    pub created_at: UtcTimestamp,
}

/// A retrieved canonical usage event row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEventRow {
    pub id: EventId,
    pub canonical_event_id: String,
    pub session_id: Option<String>,
    pub event_timestamp: Option<UtcTimestamp>,
    pub model_id: Option<String>,
    pub evidence_kind: String,
    pub source_provenance: String,
    pub parser_version: String,
    pub created_at: UtcTimestamp,
}

/// Inserts one canonical usage event, returning its generated `EventId`.
pub fn insert_event(
    conn: &rusqlite::Connection,
    event: &NewUsageEvent<'_>,
) -> Result<EventId, Error> {
    let timestamp_nanos = event.event_timestamp.map(|t| t.unix_nanos());
    let created_nanos = event.created_at.unix_nanos();

    conn.query_row(
        "INSERT INTO usage_event (
            canonical_event_id,
            session_id,
            event_timestamp,
            model_id,
            evidence_kind,
            source_provenance,
            parser_version,
            created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        RETURNING id",
        params![
            event.canonical_event_id,
            event.session_id,
            timestamp_nanos,
            event.model_id,
            event.evidence_kind,
            event.source_provenance,
            event.parser_version,
            created_nanos,
        ],
        |row| row.get(0),
    )
    .map(EventId::new)
    .map_err(|e| Error::Store(format!("cannot insert usage event: {e}")))
}

/// Retrieves a canonical usage event by its row ID.
pub fn get_event(conn: &rusqlite::Connection, id: EventId) -> Result<Option<UsageEventRow>, Error> {
    conn.query_row(
        "SELECT
            id,
            canonical_event_id,
            session_id,
            event_timestamp,
            model_id,
            evidence_kind,
            source_provenance,
            parser_version,
            created_at
        FROM usage_event
        WHERE id = ?1",
        params![id.value()],
        row_to_event,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot get usage event by id: {e}")))
}

/// Retrieves a canonical usage event by its unique canonical identifier.
pub fn get_event_by_canonical_id(
    conn: &rusqlite::Connection,
    canonical_event_id: &str,
) -> Result<Option<UsageEventRow>, Error> {
    conn.query_row(
        "SELECT
            id,
            canonical_event_id,
            session_id,
            event_timestamp,
            model_id,
            evidence_kind,
            source_provenance,
            parser_version,
            created_at
        FROM usage_event
        WHERE canonical_event_id = ?1",
        params![canonical_event_id],
        row_to_event,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot get usage event by canonical id: {e}")))
}

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageEventRow> {
    let id_val: i64 = row.get(0)?;
    let canonical_event_id: String = row.get(1)?;
    let session_id: Option<String> = row.get(2)?;
    let timestamp_nanos: Option<i64> = row.get(3)?;
    let model_id: Option<String> = row.get(4)?;
    let evidence_kind: String = row.get(5)?;
    let source_provenance: String = row.get(6)?;
    let parser_version: String = row.get(7)?;
    let created_nanos: i64 = row.get(8)?;

    Ok(UsageEventRow {
        id: EventId::new(id_val),
        canonical_event_id,
        session_id,
        event_timestamp: timestamp_nanos.map(UtcTimestamp::from_unix_nanos),
        model_id,
        evidence_kind,
        source_provenance,
        parser_version,
        created_at: UtcTimestamp::from_unix_nanos(created_nanos),
    })
}
