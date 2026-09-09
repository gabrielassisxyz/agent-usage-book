//! Migration 0036: the subscription-identity change record (`aub-iwkg`).
//!
//! When the subscription behind an account's credential path changes, the
//! sampler refuses to attribute the new subscription's readings to the old
//! logical name. The refusal needs a durable, queryable fact of its own:
//! the attempt result carries the outcome (`Unreachable`, class
//! `subscription_changed`), and this table carries the identity pair, so an
//! interval spanning the change can be found later and excluded from
//! calibration evidence without rewriting any meter row.
//!
//! Each row is one event in an account's subscription history. The first
//! sighting of an account's subscription inserts `established` (no previous
//! identity, no previous observation); a reading whose identity differs from
//! the established one inserts `changed` and its observation is never
//! stored. The kind-pairing CHECK makes the illegal states unrepresentable:
//! an establishment names nothing previous, a change names both. Rows are
//! irreplaceable evidence: the triggers reject every `UPDATE` and `DELETE`,
//! the same construction migration 0013 uses for the observation tables.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 36;

const CREATE_SUBSCRIPTION_CHANGE_TABLE: &str = "
CREATE TABLE meter_subscription_change (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id),
    kind TEXT NOT NULL CHECK (kind IN ('established', 'changed')),
    previous_identity TEXT,
    current_identity TEXT NOT NULL CHECK (length(current_identity) > 0),
    detecting_attempt_id INTEGER NOT NULL REFERENCES meter_attempt(id),
    previous_observation_id INTEGER REFERENCES meter_observation(id),
    detected_at INTEGER NOT NULL,
    CHECK (
        (kind = 'established' AND previous_identity IS NULL AND previous_observation_id IS NULL)
        OR (kind = 'changed' AND previous_identity IS NOT NULL)
    )
) STRICT;

CREATE TRIGGER meter_subscription_change_rejects_update BEFORE UPDATE ON meter_subscription_change
BEGIN
    SELECT RAISE(ABORT, 'meter_subscription_change is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_subscription_change_rejects_delete BEFORE DELETE ON meter_subscription_change
BEGIN
    SELECT RAISE(ABORT, 'meter_subscription_change is irreplaceable evidence; rows are never deleted');
END;

CREATE INDEX idx_meter_subscription_change_account ON meter_subscription_change (account_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_SUBSCRIPTION_CHANGE_TABLE)
        .map_err(|e| Error::Store(format!("cannot create the subscription change table: {e}")))
}

/// This step, for the registry.
///
/// Additive only: it creates a table that did not exist, so no irreplaceable
/// data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        rebuilds_referenced_table: false,
        apply,
    }
}
