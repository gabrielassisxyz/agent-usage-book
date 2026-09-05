//! Schema step: add the derived size and difficulty axes to task identity.
//!
//! The tracker candidates remain immutable in `task_kind_candidate`; these
//! columns are rebuildable interpretations under the same mapping version as
//! the existing task kind. Old identity rows receive explicit unknown states
//! until the next identity rebuild evaluates their stored labels.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 24;

const ADD_TASK_CLASSIFICATION_AXES: &str = "\
ALTER TABLE task_identity ADD COLUMN size TEXT CHECK (
    size IS NULL OR size IN ('S', 'M', 'L', 'XL')
);
ALTER TABLE task_identity ADD COLUMN size_state TEXT NOT NULL DEFAULT 'unknown' CHECK (
    size_state IN ('resolved', 'unknown', 'conflict')
    AND ((size_state = 'resolved') = (size IS NOT NULL))
);
ALTER TABLE task_identity ADD COLUMN size_evidence TEXT NOT NULL DEFAULT '';
ALTER TABLE task_identity ADD COLUMN difficulty TEXT CHECK (
    difficulty IS NULL OR difficulty IN ('mechanical', 'reasoning', 'critical')
);
ALTER TABLE task_identity ADD COLUMN difficulty_state TEXT NOT NULL DEFAULT 'unknown' CHECK (
    difficulty_state IN ('resolved', 'unknown', 'conflict')
    AND ((difficulty_state = 'resolved') = (difficulty IS NOT NULL))
);
ALTER TABLE task_identity ADD COLUMN difficulty_evidence TEXT NOT NULL DEFAULT '';
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_TASK_CLASSIFICATION_AXES)
        .map_err(|error| {
            Error::Store(format!(
                "cannot add task size and difficulty identity columns: {error}"
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
