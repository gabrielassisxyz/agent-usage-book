//! Schema step: task-kind candidates and the derived task-kind identity
//! (`aub-eu7.5`).
//!
//! `task_kind_candidate` is immutable source evidence: one row per structured
//! tracker assertion (the categorical identity field, one per tag), exactly as
//! read, before any mapping applies. `task_identity` is the derived, rebuildable
//! resolution of those candidates under one versioned mapping, one row per
//! task. Keeping the two apart is what lets a rebuild re-evaluate every task
//! from unchanged evidence when the mapping changes, and what makes the
//! persisted identity state name its normalization version instead of silently
//! mixing results from two mappings.
//!
//! The identity row's state is exhaustive (`resolved`, `unknown`, `conflict`)
//! and the database refuses the impossible combinations: a resolved row must
//! carry both the kind and the winning origin, and an unknown or conflicting
//! row must carry neither. A conflict persists without a winner selected, so
//! no column can quietly hold the winner of an equal-rank disagreement.

use crate::error::Error;
use crate::store::migrate::Migration;

pub const VERSION: u32 = 9;

const CREATE_TASK_IDENTITY_TABLES: &str = "\
CREATE TABLE task_kind_candidate (
    task_source TEXT NOT NULL,
    task_native TEXT NOT NULL,
    origin TEXT NOT NULL,
    raw_value TEXT NOT NULL,
    UNIQUE (task_source, task_native, origin, raw_value),
    CHECK (length(task_source) > 0),
    CHECK (length(task_native) > 0),
    CHECK (length(origin) > 0),
    CHECK (length(raw_value) > 0)
) STRICT;

CREATE INDEX idx_task_kind_candidate_task ON task_kind_candidate (
    task_source,
    task_native
);

CREATE TABLE task_identity (
    task_source TEXT NOT NULL,
    task_native TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('resolved', 'unknown', 'conflict')),
    kind TEXT,
    winner_origin TEXT,
    evidence TEXT NOT NULL,
    normalization_version INTEGER NOT NULL CHECK (normalization_version > 0),
    UNIQUE (task_source, task_native),
    CHECK (length(task_source) > 0),
    CHECK (length(task_native) > 0),
    CHECK (length(evidence) > 0),
    CHECK ((state = 'resolved') = (kind IS NOT NULL)),
    CHECK ((state = 'resolved') = (winner_origin IS NOT NULL))
) STRICT;";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_TASK_IDENTITY_TABLES)
        .map_err(|error| Error::Store(format!("cannot create task identity tables: {error}")))
}

pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
