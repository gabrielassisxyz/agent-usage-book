//! Explicit transcript ingestion as an operation in its own right (`aub-lqe.11`,
//! PLAN.md 6, 17.2, 27, 34.16).
//!
//! Parsing and reporting being one inseparable step costs operational ability:
//! CI cannot validate parsers without running a report, a scheduler cannot
//! precompute ingestion, `doctor` cannot name a concrete repair action, and no
//! report can be reproduced against a fixed ingestion generation. This module
//! is the separated half: it walks the configured sources, parses what the
//! caller asked for, deduplicates, and lands the batch through
//! [`crate::store::ingest::persist_ingest_batch`] in one transaction, advancing
//! the transcript ingestion generation the transaction's rows belong to.
//!
//! Two modes. The default pass parses every discovered file whole, and each
//! file's fresh parse replaces its previous contribution, so a parser-version
//! change or an in-place rewrite never leaves a stale second opinion beside
//! the current one. `changed_only` consults the index first: files the
//! watermark classifies [`ChangeClass::Unchanged`] are skipped and reported,
//! appended files resume from the stored consumed offset with their earlier
//! contribution kept, and everything else parses whole. Both modes converge on
//! the same rows because canonical event-level deduplication collapses replays
//! across passes.
//!
//! A trailing partial line is never consumed, whole or resumed: the pass
//! consumes up to the last complete line, so a half-written JSON record is
//! parsed only once it completes, never quarantined at an offset it can never
//! recover from.
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//! - calibration, cost models, rate cards or meter observations

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use crate::config::{Config, TranscriptConfig};
use crate::dedup::{
    HeuristicKeyCollision, canonical_identity, canonical_payload_digest, deduplicate,
};
use crate::domain::ids::SourceNamespace;
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::store::ingest::PersistEvent;
use crate::store::ingest_quarantine::{DedupCollisionDescriptor, NewQuarantineItem};
use crate::store::session::NewSession;
use crate::transcripts::watermark::{ChangeClass, FileState, classify, last_complete_line_offset};
use crate::transcripts::{
    DiscoveryError, DiscoveryOptions, NormalizedUsageEvent, SourceLocation, discover,
    namespace_for_format, parser_for_format,
};

/// The knobs one ingest pass accepts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngestOptions {
    /// Only ingest the configured source with this name. `None` ingests every
    /// configured source. An unknown name is a usage error naming the sources
    /// that exist, never an empty pass pretending the name matched nothing.
    pub source: Option<String>,
    /// Skip files the index classifies unchanged, and resume appended files
    /// from the stored offset instead of replacing their contribution.
    pub changed_only: bool,
}

/// What one ingestion pass did, for the ingest report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    /// The configured sources the pass covered.
    pub sources: Vec<String>,
    /// Files discovered across the covered sources.
    pub files_scanned: u64,
    /// Files parsed this pass, whole or resumed.
    pub files_parsed: u64,
    /// Files skipped as unchanged by `--changed-only`.
    pub files_skipped: u64,
    /// Files that could not be read, named rather than silently dropped.
    pub unreadable_files: Vec<String>,
    /// Quarantine records this pass emitted: parser records plus heuristic-key
    /// collision pairs. What `persist_ingest_batch` actually recorded is its
    /// own count inside `outcome`.
    pub quarantined: u64,
    /// The generation the pass landed as, from the transaction's advance.
    pub generation: crate::store::ingestion_generation::Generation,
    /// Rows landed, as the persistence path counted them.
    pub outcome: crate::store::ingest::PersistOutcome,
}

/// Reads every configured transcript source and lands the parsed batch.
///
/// One pass, one transaction: every row the pass writes, and the generation
/// it is reported under, commit together or not at all. A file that cannot be
/// read is named in the report and the remaining files still land; the caller
/// decides the exit class from `unreadable_files`.
pub fn run(
    conn: &mut Connection,
    config: &Config,
    options: &IngestOptions,
    now: UtcTimestamp,
) -> Result<IngestReport, Error> {
    let sources = ingest_sources(config, options)?;
    let discovered = discover(&sources, &DiscoveryOptions::default()).map_err(discovery_error)?;

    let mut files_scanned = 0u64;
    let mut files_parsed = 0u64;
    let mut files_skipped = 0u64;
    let mut unreadable_files = Vec::new();
    let mut quarantined_items: Vec<NewQuarantineItem> = Vec::new();
    let mut events: Vec<NormalizedUsageEvent> = Vec::new();
    let mut watermarks: Vec<crate::transcripts::watermark::Watermark> = Vec::new();
    let mut whole_file_sources: Vec<String> = Vec::new();
    // The relative index path and the source namespace of every file this pass
    // opened, keyed by the exact source string the parser wrote into the
    // events: an occurrence's index reference and namespace must resolve
    // through the same key the watermark uses.
    let mut relative_by_source_file: BTreeMap<String, String> = BTreeMap::new();
    let mut namespace_by_source_file: BTreeMap<String, SourceNamespace> = BTreeMap::new();

    for source in &discovered {
        let source_config = source_config(config, &source.source)?;
        let namespace = namespace_for_format(source_config.format.as_deref().unwrap_or(""))
            .ok_or_else(|| unknown_format(&source.source, source_config.format.as_deref()))?;
        let parser = parser_for_format(source_config.format.as_deref().unwrap_or(""))
            .ok_or_else(|| unknown_format(&source.source, source_config.format.as_deref()))?;
        let parser_version = parser.parser_version().as_str().to_string();
        files_scanned += source.files.len() as u64;

        for file in &source.files {
            let file_str = file.display().to_string();
            let relative_path = relative_to_root(file, &source_config.root);
            relative_by_source_file.insert(file_str.clone(), relative_path.clone());
            namespace_by_source_file.insert(file_str.clone(), SourceNamespace::new(namespace));
            let current = match FileState::read(file) {
                Ok(state) => state,
                Err(_) => {
                    unreadable_files.push(file_str.clone());
                    continue;
                }
            };

            // The index's answer for this file, read before the pass touches
            // anything: `changed_only` skips on it, and an appended file
            // resumes from its stored offset.
            let stored =
                crate::store::transcript_file::watermark_for(conn, &source.source, &relative_path)?;
            let class = classify(stored.as_ref(), &current, &parser_version);
            if options.changed_only && class == ChangeClass::Unchanged {
                files_skipped += 1;
                continue;
            }

            let contents = match std::fs::read_to_string(file) {
                Ok(text) => text,
                Err(_) => {
                    unreadable_files.push(file_str.clone());
                    continue;
                }
            };
            // Never consume a trailing partial line: whole or resumed, the
            // pass consumes up to the last complete line, so a half-written
            // record is parsed only once it completes.
            let new_offset = last_complete_line_offset(&contents, contents.len() as u64);
            let (slice, start_line) = match (options.changed_only, &class) {
                (true, ChangeClass::Appended) => {
                    let offset = stored
                        .as_ref()
                        .map(|watermark| watermark.consumed_offset)
                        .unwrap_or(0)
                        .min(new_offset) as usize;
                    let start_line = 1 + contents[..offset].matches('\n').count() as u64;
                    (&contents[offset..], start_line)
                }
                _ => (&contents[..], 1),
            };

            let output = parser.parse(slice, &SourceLocation::new(file_str.clone(), start_line));
            quarantined_items.extend(
                output
                    .quarantined()
                    .iter()
                    .map(|record| NewQuarantineItem::from_record(record, now)),
            );
            events.extend(output.events().iter().cloned());
            files_parsed += 1;

            // A resumed append keeps the file's earlier contribution; every
            // other parse replaces it, so the store holds the file's
            // contribution under the parser that produced it, never both.
            if !options.changed_only || class != ChangeClass::Appended {
                whole_file_sources.push(file_str.clone());
            }
            watermarks.push(crate::transcripts::watermark::Watermark {
                source_key: source.source.clone(),
                relative_path,
                size: current.size,
                mtime_nanos: current.mtime_nanos,
                identity: current.identity,
                parser_version: parser_version.clone(),
                consumed_offset: new_offset,
            });
        }
    }

    // One deduplication over the whole pass, exactly like the report path:
    // one message replayed across two configured roots is still one message.
    let deduplicated = deduplicate(events);
    let collisions = collision_descriptors(&deduplicated.heuristic_collisions, now);
    let quarantined = quarantined_items.len() as u64 + collisions.len() as u64;

    let mut persist_events = Vec::with_capacity(deduplicated.canonical.len());
    for event in &deduplicated.canonical {
        let identity = canonical_identity(event);
        let relative_path = relative_by_source_file.get(event.source_file()).cloned();
        let event_namespace = namespace_by_source_file.get(event.source_file()).cloned();
        persist_events.push(PersistEvent {
            event: event.clone(),
            namespace: event_namespace.ok_or_else(|| {
                Error::Internal(format!(
                    "event {} names source file {} which this pass did not open",
                    identity.canonical_event_id,
                    event.source_file()
                ))
            })?,
            canonical_event_id: identity.canonical_event_id,
            native_event_id: identity.native_event_id,
            heuristic_key: identity.heuristic_key,
            heuristic_algorithm_version: None,
            canonical_payload_digest: canonical_payload_digest(event),
            relative_path,
        });
    }

    let pass = crate::store::ingest::IngestPass {
        events: persist_events,
        sessions: session_pass(&deduplicated.canonical),
        watermarks,
        quarantined: quarantined_items,
        collisions,
        whole_file_sources,
        created_at: now,
    };
    let outcome = crate::store::ingest::persist_ingest_batch(conn, &pass)?;
    Ok(IngestReport {
        sources: discovered
            .iter()
            .map(|source| source.source.clone())
            .collect(),
        files_scanned,
        files_parsed,
        files_skipped,
        unreadable_files,
        quarantined,
        generation: outcome.generation,
        outcome,
    })
}

/// The configured sources this pass covers, filtered by the options.
///
/// No configured source at all is a usage error, because an empty pass would
/// report a zero that came from configuration, not from evidence. A source
/// filter that names nothing configured is a usage error naming what exists,
/// never a silently empty pass.
fn ingest_sources(
    config: &Config,
    options: &IngestOptions,
) -> Result<Vec<TranscriptConfig>, Error> {
    if config.transcripts.is_empty() {
        return Err(Error::Usage(
            "no [[transcripts]] sources are configured; ingest has nothing to read".into(),
        ));
    }
    match &options.source {
        None => Ok(config.transcripts.clone()),
        Some(name) => {
            let source = config
                .transcripts
                .iter()
                .find(|source| &source.name == name)
                .ok_or_else(|| {
                    Error::Usage(format!(
                        "unknown transcript source {name}; configured sources are: {}",
                        config
                            .transcripts
                            .iter()
                            .map(|source| source.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            Ok(vec![source.clone()])
        }
    }
}

fn source_config<'a>(config: &'a Config, name: &str) -> Result<&'a TranscriptConfig, Error> {
    config
        .transcripts
        .iter()
        .find(|source| source.name == name)
        .ok_or_else(|| {
            Error::Internal(format!(
                "discovered source {name} is not configured; discovery returned a source the configuration does not declare"
            ))
        })
}

fn unknown_format(source: &str, format: Option<&str>) -> Error {
    Error::Usage(format!(
        "transcript source {source} declares unknown format {}; known formats are claude-code, codex, pi",
        format.unwrap_or("(none)")
    ))
}

fn discovery_error(error: DiscoveryError) -> Error {
    match error {
        DiscoveryError::RootMissing { source, path } => Error::Usage(format!(
            "transcript source {source}: root {} is not a directory",
            path.display()
        )),
        DiscoveryError::UnsupportedPattern { source, pattern } => Error::Usage(format!(
            "transcript source {source}: pattern {pattern} is not supported"
        )),
        DiscoveryError::UnreadableDirectory { path } => Error::IngestIncomplete(format!(
            "transcript directory {} could not be read",
            path.display()
        )),
    }
}

/// The file's path relative to its configured root, for the index key. A file
/// discovery produced beneath the root always strips; the fallback keeps the
/// full display string rather than inventing a shorter key.
fn relative_to_root(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| file.display().to_string())
}

/// Heuristic-key collision pairs become quarantine records: both occurrences
/// are excluded from the canonical set, so the pass must say where they went.
fn collision_descriptors(
    collisions: &[HeuristicKeyCollision],
    observed_at: UtcTimestamp,
) -> Vec<DedupCollisionDescriptor> {
    collisions
        .iter()
        .map(|collision| {
            let [first, second] = collision.occurrences();
            DedupCollisionDescriptor {
                parser: collision.parser_version().as_str().to_string(),
                heuristic_key: collision.heuristic_key().as_str().to_string(),
                first_file: first.source_file().to_string(),
                first_payload_digest: canonical_payload_digest(first),
                second_payload_digest: canonical_payload_digest(second),
                observed_at,
            }
        })
        .collect()
}

/// Session rows the pass's canonical events imply, bounds aggregated over the
/// pass. A session whose events all lack a timestamp has no bounds to state,
/// so it produces no row: an invented bound would be a fabricated fact.
fn session_pass(events: &[NormalizedUsageEvent]) -> Vec<NewSession> {
    let mut bounds: BTreeMap<(String, String), (Option<UtcTimestamp>, Option<UtcTimestamp>)> =
        BTreeMap::new();
    for event in events {
        let Some(session) = event.session() else {
            continue;
        };
        let key = (
            session.source().as_str().to_string(),
            session.native().as_str().to_string(),
        );
        let entry = bounds.entry(key).or_insert((None, None));
        if let Some(at) = event.occurred_at() {
            entry.0 = Some(entry.0.map_or(at, |start| start.min(at)));
            entry.1 = Some(entry.1.map_or(at, |end| end.max(at)));
        }
    }
    bounds
        .into_iter()
        .filter_map(|((namespace, native), (start, end))| {
            Some(NewSession {
                source: SourceNamespace::new(namespace),
                native_session_id: crate::domain::ids::NativeSessionId::new(native),
                start: start?,
                end,
                project_key: crate::sessions::ProjectKey::new(crate::sessions::UNKNOWN_PROJECT),
                repository_key: crate::sessions::RepositoryKey::new(
                    crate::sessions::UNKNOWN_REPOSITORY,
                ),
                run_id: None,
            })
        })
        .collect()
}
