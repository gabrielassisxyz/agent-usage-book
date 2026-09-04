//! Schema step: add `evidence_class` to `account_attribution_segment` (`aub-mgv.2`,
//! PLAN.md 19.2, 34.17).
//!
//! Adds the typed evidence class column that justifies each attribution segment:
//! explicit marker, provider identity, credential mapping, conservative temporal
//! inference, or unattributed. Rebuildable materialization.

use crate::error::Error;
use crate::store::migrate::Migration;

/// The schema version this step produces.
pub const VERSION: u32 = 22;

const ADD_EVIDENCE_CLASS_COLUMN: &str = "\
ALTER TABLE account_attribution_segment ADD COLUMN evidence_class TEXT NOT NULL DEFAULT 'unattributed' CHECK (
    evidence_class IN (
        'explicit_launcher_or_hook',
        'launcher_or_hook',
        'explicit_provider_identity',
        'provider_identity',
        'configured_credential_mapping',
        'credential_mapping',
        'conservative_temporal_inference',
        'temporal_inference',
        'inferred',
        'unattributed'
    )
);";

fn apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(ADD_EVIDENCE_CLASS_COLUMN)
        .map_err(|error| {
            Error::Store(format!(
                "cannot add evidence_class column to account_attribution_segment: {error}"
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
