//! Schema step: the two-stage meter attempt tables (`aub-sth.6`).
//!
//! `meter_attempt` is the durable start of one collection attempt, committed
//! before any outbound network I/O begins (PLAN.md invariants 23 and 24, sections
//! 12.3, 13, 30). `meter_attempt_result` is the separate, later terminal fact. A
//! started attempt with no result survives as evidence in its own right: past the
//! command's execution horizon it reads as collector interruption, never as an
//! endpoint timeout and never as a missing attempt.
//!
//! Both tables are irreplaceable evidence, so triggers reject every `UPDATE` and
//! `DELETE` against them and the repository exposes insert and read only. The
//! result's ordering against its start is asserted the same way: a result at or
//! after its start for every row, with an explicit clock-anomaly marker for a
//! clock that demonstrably ran backwards. SQLite bans subqueries inside CHECK
//! constraints, so the cross-table half of that assertion is an insert trigger
//! with the same refusal semantics.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 8;

const CREATE_METER_ATTEMPT_TABLES: &str = "
CREATE TABLE meter_attempt (
    id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES sample_run(id),
    account_id INTEGER NOT NULL REFERENCES account(id),
    provider TEXT NOT NULL,
    request_started_at INTEGER NOT NULL,
    credential_context_id TEXT,
    policy_snapshot_id INTEGER NOT NULL REFERENCES sampling_policy_snapshot(id),
    due_at INTEGER NOT NULL,
    due_reason TEXT NOT NULL CHECK (
        due_reason IN ('ordinary_cadence', 'reset_edge', 'post_reset_confirmation', 'forced_or_manual')
    ),
    due_basis_attempt_id INTEGER REFERENCES meter_attempt(id),
    due_basis_result_id INTEGER REFERENCES meter_attempt_result(attempt_id),
    provider_contract_id TEXT NOT NULL,
    meter_semantics_id TEXT NOT NULL,
    CHECK (length(provider) > 0),
    CHECK (credential_context_id IS NULL OR length(credential_context_id) > 0),
    CHECK (length(provider_contract_id) > 0),
    CHECK (length(meter_semantics_id) > 0),
    CHECK (due_basis_attempt_id IS NULL OR due_basis_result_id IS NULL)
) STRICT;

CREATE TABLE meter_attempt_result (
    attempt_id INTEGER NOT NULL PRIMARY KEY REFERENCES meter_attempt(id),
    completed_at INTEGER NOT NULL,
    elapsed_nanos INTEGER NOT NULL CHECK (elapsed_nanos >= 0),
    outcome TEXT NOT NULL CHECK (outcome IN ('success', 'auth_required', 'unreachable')),
    failure_class TEXT,
    retry_after_nanos INTEGER,
    sanitized_error_classification TEXT,
    retry_index INTEGER CHECK (retry_index IS NULL OR retry_index >= 0),
    clock_anomaly INTEGER NOT NULL CHECK (clock_anomaly IN (0, 1)),
    CHECK (failure_class IS NULL OR outcome = 'unreachable'),
    CHECK (failure_class IS NOT NULL OR outcome != 'unreachable'),
    CHECK (retry_after_nanos IS NULL OR failure_class = 'rate_limited')
) STRICT;

-- The ordering assertion runs in an insert trigger rather than a column CHECK:
-- SQLite prohibits subqueries inside CHECK constraints, and the assertion
-- must read the start row for the comparison. Semantics are the same shape a
-- CHECK would give: every result is at or after its start, unless the explicit
-- clock-anomaly marker says the clock demonstrably ran backwards, which is the
-- recorded case rather than a relaxed constraint.
CREATE TRIGGER meter_attempt_result_orders_after_start
BEFORE INSERT ON meter_attempt_result
WHEN NEW.clock_anomaly = 0
    AND NEW.completed_at < (
        SELECT ma.request_started_at FROM meter_attempt ma WHERE ma.id = NEW.attempt_id
    )
BEGIN
    SELECT RAISE(ABORT, 'a meter_attempt_result may not precede its attempt start unless clock_anomaly records the anomaly explicitly');
END;

CREATE TRIGGER meter_attempt_rejects_update BEFORE UPDATE ON meter_attempt
BEGIN
    SELECT RAISE(ABORT, 'meter_attempt is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_attempt_rejects_delete BEFORE DELETE ON meter_attempt
BEGIN
    SELECT RAISE(ABORT, 'meter_attempt is irreplaceable evidence; rows are never deleted');
END;

CREATE TRIGGER meter_attempt_result_rejects_update BEFORE UPDATE ON meter_attempt_result
BEGIN
    SELECT RAISE(ABORT, 'meter_attempt_result is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_attempt_result_rejects_delete BEFORE DELETE ON meter_attempt_result
BEGIN
    SELECT RAISE(ABORT, 'meter_attempt_result is irreplaceable evidence; rows are never deleted');
END;

CREATE INDEX idx_meter_attempt_run ON meter_attempt (run_id, request_started_at);

CREATE INDEX idx_meter_attempt_open ON meter_attempt (account_id, request_started_at);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_METER_ATTEMPT_TABLES)
        .map_err(|e| Error::Store(format!("cannot create the meter attempt tables: {e}")))
}

/// This step, for the registry.
///
/// Additive only: it creates tables that did not exist, so no irreplaceable
/// data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}
