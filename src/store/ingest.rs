//! The transcript ingest persistence path (`aub-lqe.11`, PLAN.md 12.9, 12.10, 17.2).
//!
//! Lands one deduplicated parse batch in the rebuildable materialization
//! tables inside a single transaction: canonical events once, their token
//! components as child rows, one occurrence per unique identity, session
//! bounds merged, quarantine recorded, file watermarks updated, and the
//! ingestion generation advanced atomically with the rows it describes.
//!
//! The write is idempotent per identity and replay-growing per component.
//! A canonical event already present is kept, never rewritten: an occurrence
//! whose identity already exists is a replay across passes and contributes
//! nothing new. A component present with a smaller count is raised to the
//! incoming count and never lowered, because a replayed message grows its
//! output across transcript lines and a later pass over the same file must
//! converge on the largest snapshot rather than keep a stale one. These tables
//! are rebuildable materializations, not evidence, so the component merge and
//! the per-file replacement below are the update paths this module carries;
//! the evidence tables keep their no-update convention untouched.
//!
//! A file parsed whole this pass replaces its previous contribution. The
//! canonical events that file's earlier parse produced are dropped when their
//! last occurrence went away, which is what keeps a parser-version change or
//! an in-place rewrite from accumulating a stale second opinion beside the
//! current one: the store holds the file's contribution under the parser that
//! produced it, never both. Occurrences survive only inside the one
//! transaction, so a crash between the delete and the insert leaves the store
//! as it was.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//! - transcript parsing: the batch arrives fully resolved, identities included

use rusqlite::{OptionalExtension, params};

use crate::domain::ids::SourceNamespace;
use crate::domain::rows::RowCount;
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::store::ingest_quarantine::{
    DedupCollisionDescriptor, NewQuarantineItem, record_dedup_collision, record_quarantine,
};
use crate::store::ingestion_generation::{self, Generation};
use crate::store::session::NewSession;
use crate::transcripts::NormalizedUsageEvent;
use crate::transcripts::watermark::Watermark;

/// One canonical event of a parsed batch, with the identity fields the store
/// persists it under. Computed by the orchestrator through
/// [`crate::dedup::canonical_identity`], so the store never re-derives what the
/// deduplication framework has already decided.
pub struct PersistEvent {
    /// The canonical (deduplicated) event itself.
    pub event: NormalizedUsageEvent,
    /// The source namespace the file's parser attributes events under.
    pub namespace: SourceNamespace,
    /// The canonical identity, as `crate::dedup::canonical_identity` resolved it.
    pub canonical_event_id: String,
    pub native_event_id: Option<String>,
    pub heuristic_key: Option<String>,
    /// The fingerprint algorithm version, for heuristic-domain occurrences.
    pub heuristic_algorithm_version: Option<String>,
    /// The stable digest over the event's semantic payload.
    pub canonical_payload_digest: String,
    /// The file's path relative to its configured root, for the occurrence's
    /// index reference. Machine-specific absolute paths stay out of this column.
    pub relative_path: Option<String>,
}

/// One completed parse batch, ready to land in one transaction.
pub struct IngestPass {
    pub events: Vec<PersistEvent>,
    /// Session rows the pass's canonical events imply, bounds aggregated over
    /// the pass. Merged into stored rows, never replacing them wholesale.
    pub sessions: Vec<NewSession>,
    /// The watermark every successfully parsed file records, whole or resumed.
    pub watermarks: Vec<Watermark>,
    /// Quarantine records the parsers emitted.
    pub quarantined: Vec<NewQuarantineItem>,
    /// Heuristic-key collision pairs the dedup framework excluded.
    pub collisions: Vec<DedupCollisionDescriptor>,
    /// The provenance source strings of files parsed whole this pass: their
    /// previous contribution is replaced before the fresh parse lands.
    pub whole_file_sources: Vec<String>,
    /// The time the pass ran: the `created_at` every row of the pass carries,
    /// so the batch's rows name one landing, not one timestamp per event.
    pub created_at: UtcTimestamp,
}

/// What one persisted pass did, for the ingest report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistOutcome {
    /// Canonical event rows this pass inserted.
    pub events_written: RowCount,
    /// Canonical events already in the store under the same identity: replays
    /// across passes, which merge into the stored event's components.
    pub events_already_ingested: RowCount,
    /// Occurrence rows this pass inserted.
    pub occurrences_written: RowCount,
    /// Occurrence identities already stored: replayed occurrences of events
    /// an earlier pass landed.
    pub occurrences_already_ingested: RowCount,
    /// Component rows written or raised by the merge.
    pub components_written: RowCount,
    /// Session rows inserted or extended by the merge.
    pub sessions_upserted: RowCount,
    /// Rows removed by whole-file replacement and orphan cleanup.
    pub rows_replaced: RowCount,
    /// Quarantine rows recorded or extended this pass.
    pub quarantined_recorded: RowCount,
    /// The ingestion generation this pass landed as.
    pub generation: Generation,
}

/// Lands one parse batch in the rebuildable materialization tables.
///
/// The write lock is taken before anything is touched, so a pass that starts
/// while another mutating command holds the writer refuses whole: no row of
/// the batch lands partially. The generation advance is the last write before
/// the commit, so a generation N in the counter means pass N's rows are all
/// committed and no earlier pass is half-landed.
pub fn persist_ingest_batch(
    conn: &mut rusqlite::Connection,
    pass: &IngestPass,
) -> Result<PersistOutcome, Error> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| {
            Error::Store(format!(
                "another writer holds the ledger database; ingest refuses to land partially: {e}"
            ))
        })?;

    let mut rows_replaced: usize = 0;

    // Whole-file replacement: the reparsed files' earlier occurrences go, and
    // canonical events left with no occurrence at all follow them. Components
    // cascade off their event. The fresh parse below re-lands whatever the
    // current parser still reports.
    for source_file in &pass.whole_file_sources {
        rows_replaced += tx
            .execute(
                "DELETE FROM usage_occurrence WHERE source_file = ?1",
                params![source_file],
            )
            .map_err(|e| {
                Error::Store(format!("cannot replace {source_file}'s occurrences: {e}"))
            })?;
    }
    rows_replaced += tx
        .execute(
            "DELETE FROM usage_event WHERE NOT EXISTS (
                 SELECT 1 FROM usage_occurrence o WHERE o.event_id = usage_event.id
             )",
            [],
        )
        .map_err(|e| Error::Store(format!("cannot drop orphaned canonical events: {e}")))?;

    let mut events_written: u64 = 0;
    let mut events_already_ingested: u64 = 0;
    let mut occurrences_written: u64 = 0;
    let mut occurrences_already_ingested: u64 = 0;
    let mut components_written: u64 = 0;

    for persist in &pass.events {
        let event = &persist.event;
        let occurred_at_nanos = event.occurred_at().map(|t| t.unix_nanos());

        // The canonical event row: inserted once per identity, kept as found
        // when an earlier pass already landed it.
        let event_id = match tx
            .query_row(
                "INSERT INTO usage_event (
                    canonical_event_id, session_id, event_timestamp, model_id,
                    evidence_kind, source_provenance, parser_version, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT (canonical_event_id) DO NOTHING
                RETURNING id",
                params![
                    persist.canonical_event_id,
                    event.session().map(|session| session.native().as_str()),
                    occurred_at_nanos,
                    model_id_of(event),
                    evidence_kind_of(event),
                    event
                        .provenance()
                        .sources()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(";"),
                    event.parser_version().as_str(),
                    pass.created_at.unix_nanos(),
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| Error::Store(format!("cannot land the canonical event: {e}")))?
        {
            Some(id) => {
                events_written += 1;
                crate::store::usage_event::EventId::new(id)
            }
            None => {
                events_already_ingested += 1;
                existing_event_id(&tx, &persist.canonical_event_id)?
            }
        };

        // Components merge upward: a replayed message grows its output across
        // transcript lines, so the stored count becomes the larger of the two
        // and never the smaller.
        for (token_class, count) in event_components(event) {
            tx.execute(
                "INSERT INTO usage_component (event_id, token_class, count)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (event_id, token_class) DO UPDATE SET
                    count = MAX(count, excluded.count)",
                params![event_id.value(), token_class, count as i64],
            )
            .map_err(|e| Error::Store(format!("cannot land the usage component: {e}")))?;
            components_written += 1;
        }

        // One occurrence per unique identity. A conflict is a replay of an
        // event an earlier pass already attributed: counted, not re-inserted.
        let inserted = tx
            .query_row(
                "INSERT INTO usage_occurrence (
                    source_namespace, native_event_id, parser_version, heuristic_key,
                    source_file, occurred_at, event_id, transcript_file_id, source_location,
                    canonical_fingerprint, identity_strength, heuristic_algorithm_version,
                    canonical_payload_digest
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT DO NOTHING
                RETURNING id",
                params![
                    persist.namespace.as_str(),
                    persist.native_event_id,
                    event.parser_version().as_str(),
                    persist.heuristic_key,
                    event.source_file(),
                    occurred_at_nanos,
                    event_id.value(),
                    persist.relative_path,
                    Option::<String>::None,
                    persist.canonical_event_id,
                    if persist.native_event_id.is_some() {
                        "strong"
                    } else {
                        "heuristic"
                    },
                    persist.heuristic_algorithm_version,
                    persist.canonical_payload_digest,
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| Error::Store(format!("cannot land the usage occurrence: {e}")))?;
        match inserted {
            Some(_) => occurrences_written += 1,
            None => occurrences_already_ingested += 1,
        }
    }

    // Session bounds merge rather than replace: a pass that sees only part of
    // a session's events narrows nothing, and the stored row keeps the widest
    // bounds any pass has seen. Attribution columns stay as first recorded.
    let mut sessions_upserted: u64 = 0;
    for session in &pass.sessions {
        tx.execute(
            "INSERT INTO session (
                source, native_session_id, start, end, project_key, repository_key, run_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT (source, native_session_id) DO UPDATE SET
                start = MIN(start, excluded.start),
                end = CASE
                    WHEN end IS NULL THEN excluded.end
                    WHEN excluded.end IS NULL THEN end
                    ELSE MAX(end, excluded.end)
                END",
            params![
                session.source.as_str(),
                session.native_session_id.as_str(),
                session.start.unix_nanos(),
                session.end.map(|t| t.unix_nanos()),
                session.project_key.as_str(),
                session.repository_key.as_str(),
                session.run_id.as_ref().map(|id| id.as_str()),
            ],
        )
        .map_err(|e| Error::Store(format!("cannot land the session row: {e}")))?;
        sessions_upserted += 1;
    }

    for watermark in &pass.watermarks {
        crate::store::transcript_file::upsert(&tx, watermark)?;
    }

    let mut quarantined_recorded: u64 = 0;
    for item in &pass.quarantined {
        record_quarantine(&tx, item)?;
        quarantined_recorded += 1;
    }
    for collision in &pass.collisions {
        record_dedup_collision(&tx, collision)?;
        quarantined_recorded += 1;
    }

    let generation = ingestion_generation::advance(&tx)?;

    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the ingest pass: {e}")))?;

    Ok(PersistOutcome {
        events_written: RowCount::new(events_written),
        events_already_ingested: RowCount::new(events_already_ingested),
        occurrences_written: RowCount::new(occurrences_written),
        occurrences_already_ingested: RowCount::new(occurrences_already_ingested),
        components_written: RowCount::new(components_written),
        sessions_upserted: RowCount::new(sessions_upserted),
        rows_replaced: RowCount::new(rows_replaced as u64),
        quarantined_recorded: RowCount::new(quarantined_recorded),
        generation,
    })
}

/// The id of the canonical event an already-ingested identity maps to. A
/// conflict on the canonical id means the row is there: a missing lookup here
/// is a defect, not an empty result, because the conflict just proved it.
fn existing_event_id(
    tx: &rusqlite::Transaction<'_>,
    canonical_event_id: &str,
) -> Result<crate::store::usage_event::EventId, Error> {
    tx.query_row(
        "SELECT id FROM usage_event WHERE canonical_event_id = ?1",
        params![canonical_event_id],
        |row| row.get::<_, i64>(0),
    )
    .map(crate::store::usage_event::EventId::new)
    .map_err(|e| {
        Error::Store(format!(
            "cannot resolve the already-ingested canonical event {canonical_event_id}: {e}"
        ))
    })
}

/// The component rows one event lands as: the four known kinds plus any
/// unknown component, each nonzero. An absent row is an unreported component,
/// never a zero pretending a source reported one.
fn event_components(event: &NormalizedUsageEvent) -> Vec<(String, u64)> {
    let known = event.usage().known();
    let mut components = Vec::new();
    for (class, count) in [
        ("input", known.input().value()),
        ("output", known.output().value()),
        ("cache_read", known.cache_read().value()),
        ("cache_write", known.cache_write().value()),
    ] {
        if count > 0 {
            components.push((class.to_string(), count));
        }
    }
    for (name, count) in event.usage().unknown() {
        if count.value() > 0 {
            components.push((name.clone(), count.value()));
        }
    }
    components
}

/// The event's evidence classification as the stored `evidence_kind` string. A
/// reconstructed classification carries its estimator and version, so the
/// estimate's identity survives normalization instead of being erased.
fn evidence_kind_of(event: &NormalizedUsageEvent) -> String {
    match event.classification() {
        crate::transcripts::parser::EvidenceClassification::Reported => "reported".to_string(),
        crate::transcripts::parser::EvidenceClassification::Derived => "derived".to_string(),
        crate::transcripts::parser::EvidenceClassification::Reconstructed {
            estimator,
            version,
        } => {
            format!("reconstructed:{}:{}", estimator.as_str(), version.as_str())
        }
    }
}

/// The event's model identity, when the source wrote one. No parser extracts
/// one today, so this is `None` everywhere; the column stays the parser's
/// contract for the fact rather than a column nobody fills.
fn model_id_of(_event: &NormalizedUsageEvent) -> Option<String> {
    None
}
