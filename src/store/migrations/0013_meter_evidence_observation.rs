//! Schema step: the response evidence, observation, window and preference
//! selector tables (`aub-sth.7`).
//!
//! Evidence and interpretation are separate records (PLAN.md sections 6,
//! 12.4, 12.5, 13.1, invariant 25): `meter_response_evidence` holds the
//! sanitized quota evidence capsule exactly as captured, `meter_observation`
//! is one immutable interpretation of it by one adapter and semantics version,
//! and `meter_window` holds the provider's reported values exactly as
//! expressed. A corrected adapter writes a new interpretation against the same
//! evidence and never overwrites the earlier one; the derived
//! `meter_observation_preference` selector points at the one current
//! interpretation per evidence row and semantics version.
//!
//! The three irreplaceable tables reject every `UPDATE` and `DELETE` through
//! triggers and their repositories expose insert and read only. The selector
//! is derived, so it is the one table here that may be updated - by the
//! narrowly scoped `switch_current` operation, never by a general update API.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 13;

const CREATE_METER_EVIDENCE_TABLES: &str = "
CREATE TABLE meter_response_evidence (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES meter_attempt(id),
    response_classification TEXT NOT NULL CHECK (length(response_classification) > 0),
    received_at INTEGER NOT NULL,
    provider_observed_at_original TEXT,
    evidence_capsule TEXT NOT NULL CHECK (length(evidence_capsule) > 0),
    capsule_schema_version TEXT NOT NULL CHECK (length(capsule_schema_version) > 0),
    sanitizer_version TEXT NOT NULL CHECK (length(sanitizer_version) > 0),
    content_hash TEXT NOT NULL CHECK (length(content_hash) > 0),
    capture_truncated INTEGER NOT NULL CHECK (capture_truncated IN (0, 1))
) STRICT;

CREATE TABLE meter_observation (
    id INTEGER PRIMARY KEY,
    attempt_id INTEGER NOT NULL REFERENCES meter_attempt(id),
    evidence_id INTEGER NOT NULL REFERENCES meter_response_evidence(id),
    account_id INTEGER NOT NULL REFERENCES account(id),
    provider TEXT NOT NULL CHECK (length(provider) > 0),
    provider_observed_at INTEGER,
    received_at INTEGER NOT NULL,
    measurement_basis TEXT NOT NULL CHECK (
        measurement_basis IN ('provider_observed', 'locally_received', 'older_of_the_two')
    ),
    observed_plan TEXT,
    observed_tier TEXT,
    adapter_version TEXT NOT NULL CHECK (length(adapter_version) > 0),
    provider_contract_id TEXT NOT NULL CHECK (length(provider_contract_id) > 0),
    meter_semantics_id TEXT NOT NULL CHECK (length(meter_semantics_id) > 0),
    normalized_fingerprint TEXT NOT NULL CHECK (length(normalized_fingerprint) > 0)
) STRICT;

CREATE TABLE meter_window (
    id INTEGER PRIMARY KEY,
    observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('account_wide', 'model_specific')),
    scoped_model TEXT,
    quota_used_ppm INTEGER NOT NULL CHECK (quota_used_ppm >= 0 AND quota_used_ppm <= 1000000),
    reported_resolution_ppm INTEGER NOT NULL
        CHECK (reported_resolution_ppm > 0 AND reported_resolution_ppm <= 1000000),
    quantization TEXT NOT NULL CHECK (
        quantization IN ('exact', 'rounded_to_nearest', 'rounded_down', 'rounded_up', 'unknown')
    ),
    resets_at INTEGER NOT NULL,
    nominal_duration_nanos INTEGER NOT NULL CHECK (nominal_duration_nanos >= 0),
    CHECK (
        (scope_kind = 'account_wide' AND scoped_model IS NULL)
        OR (scope_kind = 'model_specific' AND scoped_model IS NOT NULL)
    )
) STRICT;

-- The derived current-interpretation pointer: exactly one row per evidence
-- row and semantics version, enforced by the primary key. This is the one
-- table in this step that may be updated, and only through the repository's
-- narrowly scoped switch_current operation.
CREATE TABLE meter_observation_preference (
    evidence_id INTEGER NOT NULL REFERENCES meter_response_evidence(id),
    meter_semantics_id TEXT NOT NULL CHECK (length(meter_semantics_id) > 0),
    current_observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    PRIMARY KEY (evidence_id, meter_semantics_id)
) STRICT;

CREATE TRIGGER meter_response_evidence_rejects_update BEFORE UPDATE ON meter_response_evidence
BEGIN
    SELECT RAISE(ABORT, 'meter_response_evidence is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_response_evidence_rejects_delete BEFORE DELETE ON meter_response_evidence
BEGIN
    SELECT RAISE(ABORT, 'meter_response_evidence is irreplaceable evidence; rows are never deleted');
END;

CREATE TRIGGER meter_observation_rejects_update BEFORE UPDATE ON meter_observation
BEGIN
    SELECT RAISE(ABORT, 'meter_observation is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_observation_rejects_delete BEFORE DELETE ON meter_observation
BEGIN
    SELECT RAISE(ABORT, 'meter_observation is irreplaceable evidence; rows are never deleted');
END;

CREATE TRIGGER meter_window_rejects_update BEFORE UPDATE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER meter_window_rejects_delete BEFORE DELETE ON meter_window
BEGIN
    SELECT RAISE(ABORT, 'meter_window is irreplaceable evidence; rows are never deleted');
END;

CREATE INDEX idx_meter_observation_evidence ON meter_observation (evidence_id, meter_semantics_id);

CREATE INDEX idx_meter_window_observation ON meter_window (observation_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_METER_EVIDENCE_TABLES)
        .map_err(|e| Error::Store(format!("cannot create the meter evidence tables: {e}")))
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
