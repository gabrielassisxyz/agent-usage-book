//! Assembly of the export report from the ledger (`aub-xus.7`).
//!
//! The store owns the read side (`store::export::assemble_export`); this
//! module wraps its typed records in the report model with the metadata and
//! the provenance node, so the renderer receives a finished report object and
//! computes nothing. The export is a direct read of the ledger, so the
//! knowledge time is the generation time: there is no separate witness set to
//! name.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

use rusqlite::Connection;

use crate::domain::provenance::QuerySemantics;
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::report::models::{ExportReport, IngestionGeneration, LedgerGeneration, ReportMetadata};
use crate::report::provenance::{ProvenanceNode, ValueArithmetic};
use crate::store::export::{ExportKey, assemble_export};

/// Builds the export report: the store assembles the records, this module
/// wraps them in the report model with the generations the export was produced
/// from and the provenance node for the row count.
pub fn assemble(
    conn: &Connection,
    key: ExportKey,
    include_logical_ids: bool,
    generated_at: UtcTimestamp,
) -> Result<ExportReport, Error> {
    let data = assemble_export(conn, key, include_logical_ids)?;
    let metadata = ReportMetadata::new(
        generated_at,
        generated_at,
        LedgerGeneration::new(data.ledger_generation),
        Some(IngestionGeneration::new(data.ingestion_generation)),
    );
    // The row count is a count of the ledger rows the export read, bound to
    // the ledger state the generations name.
    let node = ProvenanceNode::new(
        [] as [crate::domain::provenance::EvidenceId; 0],
        [] as [crate::domain::provenance::WitnessId; 0],
        QuerySemantics::new("export", key.as_str()),
        data.rows.len() as u64,
        data.rows.len() as u64,
        ValueArithmetic::Count,
    );
    Ok(ExportReport::new(
        metadata,
        data.key,
        data.included_logical_ids,
        data.rows,
        data.unresolved_events,
        node,
    ))
}
