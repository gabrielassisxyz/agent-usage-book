//! Migration to schema version 1 (`aub-sth.5`): the `account`, `sample_run` and
//! `sampling_policy_snapshot` tables.
//!
//! Additive only, so `rewrites_irreplaceable` is `false`: this is the first schema
//! migration and there is no prior data any of these tables could destroy.

use crate::error::Error;
use crate::store::migrate::Migration;

const CREATE_TABLES_SQL: &str = "\
CREATE TABLE account (
    id INTEGER PRIMARY KEY,
    logical_name TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    first_observed_at INTEGER NOT NULL,
    last_observed_at INTEGER NOT NULL,
    UNIQUE (provider_key, logical_name),
    CHECK (last_observed_at >= first_observed_at)
) STRICT;

CREATE TABLE sample_run (
    id INTEGER PRIMARY KEY,
    trigger TEXT NOT NULL CHECK (trigger IN ('timer', 'hook', 'manual', 'live')),
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    aub_version TEXT NOT NULL,
    configuration_fingerprint TEXT NOT NULL,
    CHECK (ended_at IS NULL OR ended_at >= started_at)
) STRICT;

CREATE TABLE sampling_policy_snapshot (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id),
    effective_at INTEGER NOT NULL,
    ordinary_cadence_nanos INTEGER NOT NULL,
    freshness_horizon_nanos INTEGER NOT NULL,
    reset_edge_policy TEXT NOT NULL,
    retry_backoff_policy TEXT NOT NULL,
    command_budget_nanos INTEGER NOT NULL,
    policy_algorithm_version TEXT NOT NULL,
    UNIQUE (account_id, effective_at)
) STRICT;
";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_TABLES_SQL).map_err(|e| {
        Error::Store(format!(
            "cannot create account/sample_run/policy tables: {e}"
        ))
    })
}

/// The migration this file contributes to the registry: schema version 1.
pub fn migration() -> Migration {
    Migration {
        version: 1,
        rewrites_irreplaceable: false,
        apply,
    }
}
