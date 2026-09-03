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
//! One call is one bounded batch: the caller (the ingest orchestrator, or a
//! test driving the primitive directly) splits its resolved events into
//! transactions of a documented maximum size (`config.ingest.max_batch_events`),
//! so no single batch can monopolize the single SQLite writer slot the way one
//! pass-sized transaction would (PLAN.md section 11.2). This module states the
//! per-batch writer-slot budget the measurements are judged against and
//! measures what each batch actually held, in [`PersistOutcome::writer_slot`].
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//! - transcript parsing: the batch arrives fully resolved, identities included

use rusqlite::{OptionalExtension, params};

use crate::domain::ids::SourceNamespace;
use crate::domain::rows::RowCount;
use crate::domain::time::{Clock, MonotonicDuration, UtcTimestamp};
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
/// deduplication framework has already decided. Clone because a pass splits its
/// resolved events into bounded batches, and each batch carries its slice.
#[derive(Clone)]
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
    /// How long this batch held the SQLite writer slot: from the moment its
    /// write transaction was granted to the moment its commit returned. The
    /// wait for a slot somebody else held is deliberately not part of this
    /// number; the hold is what the budget below judges.
    pub writer_slot: MonotonicDuration,
}

/// The stated per-batch writer-slot budget (PLAN.md section 11.2): no single
/// ingest batch may hold the SQLite writer slot longer than this, because a
/// meter write that arrives mid-batch must be served after at most one batch's
/// hold. The value is the ordinary writer's own default busy bound
/// (`sampling.request_timeout`, 5s), so a meter write arriving mid-batch
/// waits at most one batch's hold plus its own wait, both inside the bound it
/// already carries. The bound caps the transaction
/// `config.ingest.max_batch_events` produces at the documented default; a
/// batch that measures over it says so in its diagnostic rather than silently
/// stretching the meter's worst-case wait. This is a measurement target the
/// tests assert, not a runtime abort: killing a batch that already holds
/// valid rows would discard work to enforce a number.
pub const WRITER_SLOT_BUDGET_PER_BATCH: MonotonicDuration = MonotonicDuration::from_millis(5_000);

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
    clock: &impl Clock,
) -> Result<PersistOutcome, Error> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| {
            Error::Store(format!(
                "another writer holds the ledger database; ingest refuses to land partially: {e}"
            ))
        })?;
    // The writer slot is held from here, not from the BEGIN IMMEDIATE above:
    // the wait for a slot somebody else holds is the other writer's business,
    // and what the budget judges is this batch's own hold.
    let slot_start = clock.monotonic_now();

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
    let writer_slot = clock.monotonic_now().duration_since(slot_start);

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
        writer_slot,
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::dedup::{canonical_identity, canonical_payload_digest};
    use crate::domain::ids::NativeSessionId;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::evidence::{CoverageCompleteness, EvidenceQuality, Provenance};
    use crate::store::connection::PragmaPolicy;
    use crate::transcripts::parser::{
        EvidenceClassification, ParserVersion, STRONG_IDENTITY_PREFIX, SourceLocation,
    };
    use crate::{
        domain::tokens::{
            CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
            UsageVector,
        },
        transcripts::NormalizedUsageEvent,
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-ingest-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_conn() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("ingest.db");
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = crate::store::connection::open(
            &db_path,
            crate::store::connection::AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    /// A strong-identity event the caller shapes: one source file, one native
    /// identifier, one session, and the token counts given.
    fn strong_event(
        id: &str,
        file: &str,
        occurred_nanos: i64,
        input: u64,
        output: u64,
    ) -> NormalizedUsageEvent {
        let usage = UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(0),
                CacheWriteTokens::new(0),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        NormalizedUsageEvent::new(
            usage,
            EvidenceClassification::Reported,
            Provenance::new(vec![
                file.to_string(),
                format!("{STRONG_IDENTITY_PREFIX}{id}"),
            ]),
            ParserVersion::new("test-1"),
        )
        .with_occurred_at(UtcTimestamp::from_unix_nanos(occurred_nanos))
        .with_session(crate::domain::ids::SessionId::new(
            SourceNamespace::new("test"),
            NativeSessionId::new("s1"),
        ))
    }

    /// Builds the persist events exactly the orchestrator builds them, through
    /// the shared identity framework, so a test never invents identities the
    /// real path would not produce.
    fn persist_events(events: &[NormalizedUsageEvent]) -> Vec<PersistEvent> {
        events
            .iter()
            .map(|event| {
                let identity = canonical_identity(event);
                PersistEvent {
                    event: event.clone(),
                    namespace: SourceNamespace::new("test"),
                    canonical_event_id: identity.canonical_event_id,
                    native_event_id: identity.native_event_id,
                    heuristic_key: identity.heuristic_key,
                    heuristic_algorithm_version: None,
                    canonical_payload_digest: canonical_payload_digest(event),
                    relative_path: Some(event.source_file().to_string()),
                }
            })
            .collect()
    }

    fn pass_of(events: Vec<NormalizedUsageEvent>, now: UtcTimestamp) -> IngestPass {
        IngestPass {
            events: persist_events(&events),
            sessions: Vec::new(),
            watermarks: Vec::new(),
            quarantined: Vec::new(),
            collisions: Vec::new(),
            whole_file_sources: Vec::new(),
            created_at: now,
        }
    }

    fn count(conn: &rusqlite::Connection, table: &str) -> u64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("row count must be readable") as u64
    }

    /// Lands one batch through the store primitive under a fixture clock, so
    /// every call site names the behaviour instead of the clock plumbing.
    fn land(conn: &mut rusqlite::Connection, pass: &IngestPass) -> Result<PersistOutcome, Error> {
        persist_ingest_batch(
            conn,
            pass,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
    }

    fn component_count(
        conn: &rusqlite::Connection,
        canonical_event_id: &str,
        token_class: &str,
    ) -> u64 {
        conn.query_row(
            "SELECT c.count FROM usage_component c
             JOIN usage_event e ON e.id = c.event_id
             WHERE e.canonical_event_id = ?1 AND c.token_class = ?2",
            rusqlite::params![canonical_event_id, token_class],
            |row| row.get::<_, i64>(0),
        )
        .expect("component must be readable") as u64
    }

    /// The unit test the bead names: one persisted pass advances the transcript
    /// ingestion generation and reports the value it landed as, and the counter
    /// agrees with the report. The planted negative is the naive implementation
    /// that reports the pre-advance value: the first pass would say zero, or a
    /// value one behind the counter, and both fail here.
    #[test]
    fn a_persisted_pass_advances_and_reports_the_generation() {
        let (_scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);
        assert_eq!(
            ingestion_generation::current(&conn).unwrap(),
            Generation::new(0),
            "a fresh database has completed no ingestion pass"
        );

        let first = land(
            &mut conn,
            &pass_of(
                vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
                now,
            ),
        )
        .unwrap();
        assert_eq!(first.generation, Generation::new(1));
        assert_eq!(
            ingestion_generation::current(&conn).unwrap(),
            Generation::new(1)
        );

        let second = land(
            &mut conn,
            &pass_of(vec![strong_event("m2", "corpus/a.jsonl", 2_000, 7, 3)], now),
        )
        .unwrap();
        assert_eq!(second.generation, Generation::new(2));
        assert_eq!(
            ingestion_generation::current(&conn).unwrap(),
            Generation::new(2)
        );
    }

    /// A rollback of the pass rolls the generation back too: the advance is
    /// part of the pass's transaction, not a side effect that survives it. The
    /// competing writer holds the slot, the pass refuses, and the counter is
    /// exactly where it was.
    #[test]
    fn a_refused_pass_advances_nothing() {
        let (scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);
        conn.busy_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        let mut holder = crate::store::connection::open(
            &scratch.path().join("ingest.db"),
            crate::store::connection::AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(100),
            },
        )
        .unwrap();
        let _held = holder
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();

        let err = land(
            &mut conn,
            &pass_of(
                vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
                now,
            ),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("another writer holds"),
            "the refusal must name the held writer: {err}"
        );
        assert_eq!(
            ingestion_generation::current(&conn).unwrap(),
            Generation::new(0),
            "a refused pass must leave the generation where it was"
        );
    }

    /// Replay across passes contributes nothing new: the same batch persisted
    /// twice reports the second landing as replays, and the store holds one
    /// event and one occurrence, not two. Without this, the rebuild-then-ingeest
    /// reproduction could not hold, because re-ingesting the same corpus would
    /// double every row.
    #[test]
    fn a_replayed_batch_writes_no_new_rows_and_reports_replays() {
        let (_scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);
        let batch = pass_of(
            vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
            now,
        );

        let first = land(&mut conn, &batch).unwrap();
        assert_eq!(first.events_written.value(), 1);
        assert_eq!(first.occurrences_written.value(), 1);
        assert_eq!(first.events_already_ingested.value(), 0);
        assert_eq!(first.occurrences_already_ingested.value(), 0);

        let replay = land(&mut conn, &batch).unwrap();
        assert_eq!(
            replay.events_written.value(),
            0,
            "a replay writes no new event"
        );
        assert_eq!(
            replay.occurrences_written.value(),
            0,
            "a replay writes no new occurrence"
        );
        assert_eq!(replay.events_already_ingested.value(), 1);
        assert_eq!(replay.occurrences_already_ingested.value(), 1);
        assert_eq!(count(&conn, "usage_event"), 1);
        assert_eq!(count(&conn, "usage_occurrence"), 1);
        // input and output: both components survive the replay untouched.
        assert_eq!(count(&conn, "usage_component"), 2);
    }

    /// A replayed message grows its output across transcript lines: the stored
    /// component count becomes the larger of the two and a smaller later count
    /// never lowers it. The planted negative is the replace-on-conflict merge,
    /// which would leave the store reporting 5 after the pass that saw 15.
    #[test]
    fn a_grown_component_count_is_raised_and_a_smaller_one_never_lowers_it() {
        let (_scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);

        land(
            &mut conn,
            &pass_of(
                vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
                now,
            ),
        )
        .unwrap();
        assert_eq!(component_count(&conn, "event-id:m1", "output"), 5);

        land(
            &mut conn,
            &pass_of(
                vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 15)],
                now,
            ),
        )
        .unwrap();
        assert_eq!(component_count(&conn, "event-id:m1", "output"), 15);

        land(
            &mut conn,
            &pass_of(
                vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
                now,
            ),
        )
        .unwrap();
        assert_eq!(
            component_count(&conn, "event-id:m1", "output"),
            15,
            "a smaller replay must not lower the stored count"
        );
    }

    /// A file parsed whole this pass replaces its previous contribution: the
    /// earlier parse's occurrences go, canonical events left with no occurrence
    /// follow them, and the fresh parse is the store's only opinion about that
    /// file. The planted negative is an accumulate-both merge, which would hold
    /// two events for one file after a parser-version change.
    #[test]
    fn a_whole_file_reparse_replaces_the_file_contribution() {
        let (_scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);

        land(
            &mut conn,
            &pass_of(
                vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
                now,
            ),
        )
        .unwrap();
        assert_eq!(count(&conn, "usage_event"), 1);

        let mut replacement = pass_of(vec![strong_event("m2", "corpus/a.jsonl", 2_000, 8, 4)], now);
        replacement.whole_file_sources = vec!["corpus/a.jsonl".to_string()];
        let outcome = land(&mut conn, &replacement).unwrap();
        // The old occurrence goes, and the canonical event it was the last
        // occurrence of follows it: two deletions, both reported here.
        assert_eq!(
            outcome.rows_replaced.value(),
            2,
            "the old occurrence and its orphaned event go"
        );

        assert_eq!(
            count(&conn, "usage_event"),
            1,
            "the orphaned event follows its occurrence"
        );
        assert_eq!(count(&conn, "usage_occurrence"), 1);
        let kept: String = conn
            .query_row("SELECT canonical_event_id FROM usage_event", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            kept, "event-id:m2",
            "the fresh parse is the store's only opinion"
        );
    }

    /// Session bounds merge rather than replace: a pass that sees only part of
    /// a session narrows nothing, and the stored row keeps the widest bounds
    /// any pass has seen, in both directions.
    #[test]
    fn session_bounds_merge_wider_and_never_narrower() {
        let (_scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);

        let session = |start: i64, end: Option<i64>| NewSession {
            source: SourceNamespace::new("test"),
            native_session_id: NativeSessionId::new("s1"),
            start: UtcTimestamp::from_unix_nanos(start),
            end: end.map(UtcTimestamp::from_unix_nanos),
            project_key: crate::sessions::ProjectKey::new("p"),
            repository_key: crate::sessions::RepositoryKey::new("r"),
            run_id: None,
        };

        let mut first = pass_of(
            vec![strong_event("m1", "corpus/a.jsonl", 5_000, 10, 5)],
            now,
        );
        first.sessions = vec![session(5_000, Some(9_000))];
        land(&mut conn, &first).unwrap();

        // A narrower pass must not shrink the stored bounds.
        let mut second = pass_of(
            vec![strong_event("m2", "corpus/a.jsonl", 6_000, 10, 5)],
            now,
        );
        second.sessions = vec![session(6_000, Some(7_000))];
        land(&mut conn, &second).unwrap();

        let (start, end): (i64, i64) = conn
            .query_row(
                "SELECT start, end FROM session WHERE native_session_id = 's1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            start, 5_000,
            "a narrower pass must not move the start later"
        );
        assert_eq!(end, 9_000, "a narrower pass must not pull the end earlier");

        // A wider pass extends in both directions.
        let mut third = pass_of(
            vec![strong_event("m3", "corpus/a.jsonl", 12_000, 10, 5)],
            now,
        );
        third.sessions = vec![session(1_000, Some(12_000))];
        land(&mut conn, &third).unwrap();
        let (start, end): (i64, i64) = conn
            .query_row(
                "SELECT start, end FROM session WHERE native_session_id = 's1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(start, 1_000);
        assert_eq!(end, 12_000);
    }

    /// A pass quarantines what its parsers emitted, and a replayed quarantine
    /// item merges into the stored row rather than duplicating it, so the
    /// reproduction property holds for the quarantine table too.
    #[test]
    fn quarantine_records_merge_on_replay() {
        let (_scratch, mut conn) = fixture_conn();
        let now = UtcTimestamp::from_unix_nanos(1_000_000);
        let item = NewQuarantineItem {
            source_file: "corpus/a.jsonl".to_string(),
            byte_offset: None,
            line_number: Some(3),
            parser: "test-1".to_string(),
            failure_class: "wrong_field_type".to_string(),
            excerpt_hash: "hash-a".to_string(),
            excerpt: None,
            observed_at: now,
        };
        let mut batch = pass_of(
            vec![strong_event("m1", "corpus/a.jsonl", 1_000, 10, 5)],
            now,
        );
        batch.quarantined = vec![item.clone()];
        land(&mut conn, &batch).unwrap();
        land(&mut conn, &batch).unwrap();
        assert_eq!(
            count(&conn, "ingest_quarantine"),
            1,
            "a replayed quarantine item must merge, not duplicate"
        );
        // Silence the unused-variable lint SourceLocation would otherwise carry:
        // the import exists for the parse-shaped record construction above.
        let _ = SourceLocation::new("x", 1);
    }
}
