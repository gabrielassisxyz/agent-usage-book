//! Schema step: durable normalized issue-tracker events (`aub-eu7.1`).
//!
//! Both accepted events and timestamp quarantines carry the tracker source and
//! upstream event id. That identity makes re-ingestion idempotent even though the
//! tracker itself may later rebuild its own history.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 7;

const CREATE_TASK_EVENT_TABLES: &str = "\
CREATE TABLE task_event (
    id INTEGER PRIMARY KEY,
    tracker_source TEXT NOT NULL,
    tracker_event_id INTEGER NOT NULL,
    task_source TEXT NOT NULL,
    task_native TEXT NOT NULL,
    occurred_at INTEGER NOT NULL,
    event_kind TEXT NOT NULL,
    agent_association TEXT,
    UNIQUE (tracker_source, tracker_event_id),
    CHECK (length(tracker_source) > 0),
    CHECK (length(task_source) > 0),
    CHECK (length(task_native) > 0),
    CHECK (length(event_kind) > 0)
) STRICT;

CREATE INDEX idx_task_event_task_time ON task_event (
    task_source,
    task_native,
    occurred_at,
    tracker_event_id
);

CREATE TABLE task_event_quarantine (
    id INTEGER PRIMARY KEY,
    tracker_source TEXT NOT NULL,
    tracker_event_id INTEGER NOT NULL,
    raw_timestamp TEXT NOT NULL,
    reason TEXT NOT NULL,
    UNIQUE (tracker_source, tracker_event_id),
    CHECK (length(tracker_source) > 0),
    CHECK (length(reason) > 0)
) STRICT;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_TASK_EVENT_TABLES)
        .map_err(|error| Error::Store(format!("cannot create task event tables: {error}")))
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
