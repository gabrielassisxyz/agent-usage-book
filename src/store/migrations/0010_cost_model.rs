//! Schema step: the immutable `cost_model`, `cost_model_term` and
//! `cost_model_lifecycle` tables (`aub-ai3.1`, PLAN.md 12.13, 12.14, 22).
//!
//! A witness row that gets edited in place cannot answer what `aub` would have
//! said last month, and that question has to stay answerable. The two times
//! this system distinguishes everywhere are therefore both present and both
//! immutable: `valid_from`/`valid_until` say when the model describes the
//! physical world, and `published_at` says when `aub` learned it.
//!
//! Immutability is enforced by triggers, not by convention: an `UPDATE` or
//! `DELETE` against `cost_model` or `cost_model_term` raises `ABORT` at the
//! database, and the lifecycle table accepts appends only. A repository that
//! exposed no update path would still let a stray statement mutate the table;
//! the trigger closes that at the one authority every statement passes.
//!
//! Activation and supersession are rows in the lifecycle table, never a
//! column on the model. One row records one transition: the first model is
//! activated with no predecessor, every later model supersedes the model that
//! was active at its event instant. The active model at a past instant is the
//! `cost_model_id` of the latest lifecycle row at or before that instant, and
//! a superseded model stays queryable because nothing is ever deleted.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 10;

const CREATE_COST_MODEL: &str = "\
CREATE TABLE cost_model (
    id INTEGER PRIMARY KEY,
    cost_model_id TEXT NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    model_id TEXT,
    billing_semantics_id TEXT NOT NULL,
    plan_scope TEXT,
    version TEXT NOT NULL,
    valid_from INTEGER NOT NULL,
    valid_until INTEGER NOT NULL,
    published_at INTEGER NOT NULL,
    provenance_digest TEXT NOT NULL,
    provenance_input_count INTEGER NOT NULL,
    CHECK (length(cost_model_id) > 0),
    CHECK (length(provider) > 0),
    CHECK (scope_kind IN ('model_class', 'model')),
    CHECK (scope_kind = 'model_class' OR length(model_id) > 0),
    CHECK (scope_kind = 'model' OR model_id IS NULL),
    CHECK (length(billing_semantics_id) > 0),
    CHECK (plan_scope IS NULL OR length(plan_scope) > 0),
    CHECK (length(version) > 0),
    CHECK (valid_until >= valid_from),
    CHECK (length(provenance_digest) = 16),
    CHECK (provenance_input_count >= 0)
) STRICT";

const CREATE_COST_MODEL_TERM: &str = "\
CREATE TABLE cost_model_term (
    id INTEGER PRIMARY KEY,
    cost_model_id INTEGER NOT NULL REFERENCES cost_model(id),
    token_kind TEXT NOT NULL,
    credits_per_token_micros INTEGER NOT NULL,
    uncertainty_low_micros INTEGER,
    uncertainty_high_micros INTEGER,
    derivation_method TEXT NOT NULL,
    evidence_experiment TEXT,
    CHECK (token_kind IN ('input', 'output', 'cache_read', 'cache_write')),
    CHECK (length(derivation_method) > 0),
    CHECK (evidence_experiment IS NULL OR length(evidence_experiment) > 0),
    CHECK (
        (uncertainty_low_micros IS NULL AND uncertainty_high_micros IS NULL)
        OR (uncertainty_low_micros IS NOT NULL AND uncertainty_high_micros IS NOT NULL
            AND uncertainty_high_micros >= uncertainty_low_micros)
    ),
    UNIQUE (cost_model_id, token_kind)
) STRICT";

const CREATE_COST_MODEL_LIFECYCLE: &str = "\
CREATE TABLE cost_model_lifecycle (
    id INTEGER PRIMARY KEY,
    cost_model_id INTEGER NOT NULL REFERENCES cost_model(id),
    event_kind TEXT NOT NULL,
    event_at INTEGER NOT NULL,
    supersedes_model_id INTEGER REFERENCES cost_model(id),
    CHECK (event_kind IN ('activation', 'supersession')),
    CHECK (
        (event_kind = 'activation' AND supersedes_model_id IS NULL)
        OR (event_kind = 'supersession' AND supersedes_model_id IS NOT NULL)
    ),
    UNIQUE (event_at, cost_model_id)
) STRICT";

const TRIGGER_COST_MODEL_NO_UPDATE: &str = "\
CREATE TRIGGER cost_model_no_update BEFORE UPDATE ON cost_model
BEGIN
    SELECT RAISE(ABORT, 'cost_model is immutable: update refused');
END";

const TRIGGER_COST_MODEL_NO_DELETE: &str = "\
CREATE TRIGGER cost_model_no_delete BEFORE DELETE ON cost_model
BEGIN
    SELECT RAISE(ABORT, 'cost_model is immutable: delete refused');
END";

const TRIGGER_COST_MODEL_TERM_NO_UPDATE: &str = "\
CREATE TRIGGER cost_model_term_no_update BEFORE UPDATE ON cost_model_term
BEGIN
    SELECT RAISE(ABORT, 'cost_model_term is immutable: update refused');
END";

const TRIGGER_COST_MODEL_TERM_NO_DELETE: &str = "\
CREATE TRIGGER cost_model_term_no_delete BEFORE DELETE ON cost_model_term
BEGIN
    SELECT RAISE(ABORT, 'cost_model_term is immutable: delete refused');
END";

const TRIGGER_LIFECYCLE_NO_UPDATE: &str = "\
CREATE TRIGGER cost_model_lifecycle_no_update BEFORE UPDATE ON cost_model_lifecycle
BEGIN
    SELECT RAISE(ABORT, 'cost_model_lifecycle is append-only: update refused');
END";

const TRIGGER_LIFECYCLE_NO_DELETE: &str = "\
CREATE TRIGGER cost_model_lifecycle_no_delete BEFORE DELETE ON cost_model_lifecycle
BEGIN
    SELECT RAISE(ABORT, 'cost_model_lifecycle is append-only: delete refused');
END";

/// This step, for the registry.
///
/// Not a rewrite: it creates three tables that did not exist, so no
/// irreplaceable data is at risk and the verified-backup guard does not apply.
pub fn migration() -> Migration {
    Migration {
        version: VERSION,
        rewrites_irreplaceable: false,
        apply,
    }
}

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(CREATE_COST_MODEL)
        .map_err(|e| Error::Store(format!("cannot create the cost_model table: {e}")))?;
    conn.execute_batch(CREATE_COST_MODEL_TERM)
        .map_err(|e| Error::Store(format!("cannot create the cost_model_term table: {e}")))?;
    conn.execute_batch(CREATE_COST_MODEL_LIFECYCLE)
        .map_err(|e| Error::Store(format!("cannot create the cost_model_lifecycle table: {e}")))?;
    conn.execute_batch(TRIGGER_COST_MODEL_NO_UPDATE)
        .map_err(|e| Error::Store(format!("cannot create the cost_model update guard: {e}")))?;
    conn.execute_batch(TRIGGER_COST_MODEL_NO_DELETE)
        .map_err(|e| Error::Store(format!("cannot create the cost_model delete guard: {e}")))?;
    conn.execute_batch(TRIGGER_COST_MODEL_TERM_NO_UPDATE)
        .map_err(|e| {
            Error::Store(format!(
                "cannot create the cost_model_term update guard: {e}"
            ))
        })?;
    conn.execute_batch(TRIGGER_COST_MODEL_TERM_NO_DELETE)
        .map_err(|e| {
            Error::Store(format!(
                "cannot create the cost_model_term delete guard: {e}"
            ))
        })?;
    conn.execute_batch(TRIGGER_LIFECYCLE_NO_UPDATE)
        .map_err(|e| Error::Store(format!("cannot create the lifecycle update guard: {e}")))?;
    conn.execute_batch(TRIGGER_LIFECYCLE_NO_DELETE)
        .map_err(|e| Error::Store(format!("cannot create the lifecycle delete guard: {e}")))?;
    Ok(())
}
