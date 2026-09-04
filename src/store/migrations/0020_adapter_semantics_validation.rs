//! Schema step: the authoritative-surface comparison and the adapter-semantics
//! annotations (`aub-eun.12`).
//!
//! A comparison records one recorded reading of the provider's own
//! authoritative usage surface against one `meter_window` the adapter
//! interpreted, plus the verdict: agrees within the surface's documented
//! granularity, or unresolved mismatch. There is no third verdict and no
//! tolerance column.
//!
//! An annotation is an immutable mismatch, correction, or exclusion note. A
//! correction links to the annotation it corrects and never overwrites it; a
//! mismatch stays stored and stays open until a correction references it. The
//! exclusion kind is persisted here and consumed by calibration eligibility
//! (`aub-c0b.7`).
//!
//! Both tables are irreplaceable validation evidence: their triggers reject
//! every `UPDATE` and `DELETE`, and the durable-class taxonomy retains them
//! forever.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 20;

const CREATE_ADAPTER_SEMANTICS_VALIDATION_TABLES: &str = "
CREATE TABLE authoritative_surface_comparison (
    id INTEGER PRIMARY KEY,
    observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    window_id INTEGER NOT NULL REFERENCES meter_window(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    authoritative_surface TEXT NOT NULL CHECK (length(authoritative_surface) > 0),
    documented_granularity_ppm INTEGER NOT NULL
        CHECK (documented_granularity_ppm >= 0 AND documented_granularity_ppm <= 1000000),
    adapter_quota_used_ppm INTEGER NOT NULL
        CHECK (adapter_quota_used_ppm >= 0 AND adapter_quota_used_ppm <= 1000000),
    authoritative_quota_used_ppm INTEGER NOT NULL
        CHECK (authoritative_quota_used_ppm >= 0 AND authoritative_quota_used_ppm <= 1000000),
    read_at INTEGER NOT NULL,
    verdict TEXT NOT NULL
        CHECK (verdict IN ('agrees_within_granularity', 'unresolved_mismatch'))
) STRICT;

CREATE TABLE adapter_semantics_annotation (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('mismatch', 'correction', 'exclusion')),
    comparison_id INTEGER NOT NULL REFERENCES authoritative_surface_comparison(id),
    observation_id INTEGER NOT NULL REFERENCES meter_observation(id),
    semantic_key TEXT NOT NULL CHECK (length(semantic_key) > 0),
    adapter_quota_used_ppm INTEGER NOT NULL
        CHECK (adapter_quota_used_ppm >= 0 AND adapter_quota_used_ppm <= 1000000),
    authoritative_quota_used_ppm INTEGER NOT NULL
        CHECK (authoritative_quota_used_ppm >= 0 AND authoritative_quota_used_ppm <= 1000000),
    corrects_annotation_id INTEGER REFERENCES adapter_semantics_annotation(id),
    detail TEXT NOT NULL CHECK (length(detail) > 0),
    created_at INTEGER NOT NULL,
    CHECK (
        (kind = 'correction' AND corrects_annotation_id IS NOT NULL)
        OR (kind <> 'correction' AND corrects_annotation_id IS NULL)
    )
) STRICT;

CREATE TRIGGER authoritative_surface_comparison_rejects_update
    BEFORE UPDATE ON authoritative_surface_comparison
BEGIN
    SELECT RAISE(ABORT, 'authoritative_surface_comparison is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER authoritative_surface_comparison_rejects_delete
    BEFORE DELETE ON authoritative_surface_comparison
BEGIN
    SELECT RAISE(ABORT, 'authoritative_surface_comparison is irreplaceable evidence; rows are never deleted');
END;

CREATE TRIGGER adapter_semantics_annotation_rejects_update
    BEFORE UPDATE ON adapter_semantics_annotation
BEGIN
    SELECT RAISE(ABORT, 'adapter_semantics_annotation is irreplaceable evidence; rows are never updated');
END;

CREATE TRIGGER adapter_semantics_annotation_rejects_delete
    BEFORE DELETE ON adapter_semantics_annotation
BEGIN
    SELECT RAISE(ABORT, 'adapter_semantics_annotation is irreplaceable evidence; rows are never deleted');
END;

CREATE INDEX idx_authoritative_surface_comparison_observation
    ON authoritative_surface_comparison (observation_id);

CREATE INDEX idx_adapter_semantics_annotation_comparison
    ON adapter_semantics_annotation (comparison_id);

CREATE INDEX idx_adapter_semantics_annotation_corrects
    ON adapter_semantics_annotation (corrects_annotation_id);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_ADAPTER_SEMANTICS_VALIDATION_TABLES)
        .map_err(|error| {
            Error::Store(format!(
                "cannot create the adapter semantics validation tables: {error}"
            ))
        })
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
