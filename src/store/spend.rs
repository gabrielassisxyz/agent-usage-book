//! Canonical transcript usage read model for `aub spend` (`aub-lqe.13`).
//!
//! The ledger is the authority for a spend report. This repository reads the
//! canonical event once and attaches its component rows and resolved session
//! labels without re-parsing transcript files.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::sessions::{UNKNOWN_PROJECT, UNKNOWN_REPOSITORY};

/// The session label a canonical event carries when its source or session id
/// could not be resolved. Exported so a caller that needs to tell a resolved
/// session apart from this fallback (`report::spend::task_label_map`, for
/// task attribution's `session_is_mapped` context) reads the same constant
/// [`session_label`] writes, rather than a second copy of the literal.
pub const UNKNOWN_SESSION: &str = "unknown-session";

/// One canonical event ready for report grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalSpendEvent {
    pub canonical_id: String,
    pub occurred_at: UtcTimestamp,
    pub session: String,
    pub project: String,
    pub repository: String,
    pub evidence_kind: String,
    pub sources: BTreeSet<String>,
    pub components: BTreeMap<String, u64>,
    pub vendor: Option<String>,
    pub model: Option<String>,
}

/// Diagnostics that qualify a canonical spend query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpendDiagnostics {
    pub quarantined_by_class: BTreeMap<String, u64>,
    pub replayed_occurrences: u64,
    pub heuristic_identities: u64,
}

/// Reads canonical events in `[since, until)`, with exactly one attribution
/// source chosen for a replayed event. The canonical event remains one row even
/// when the transcript recorded it more than once.
pub fn canonical_events(
    conn: &Connection,
    since: UtcTimestamp,
    until: UtcTimestamp,
) -> Result<Vec<CanonicalSpendEvent>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.canonical_event_id, e.event_timestamp, e.session_id, \
                    e.evidence_kind, e.source_provenance, \
                    (SELECT MIN(o.source_namespace) FROM usage_occurrence o WHERE o.event_id = e.id), \
                    e.model_id \
             FROM usage_event e \
             WHERE e.event_timestamp >= ?1 AND e.event_timestamp < ?2 \
             ORDER BY e.event_timestamp, e.canonical_event_id",
        )
        .map_err(|error| Error::Store(format!("cannot prepare canonical spend scan: {error}")))?;
    let rows = stmt
        .query_map(params![since.unix_nanos(), until.unix_nanos()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })
        .map_err(|error| Error::Store(format!("cannot query canonical spend rows: {error}")))?;

    let mut events = Vec::new();
    for row in rows {
        let (
            event_id,
            canonical_id,
            occurred_at,
            session_id,
            evidence_kind,
            provenance,
            source,
            model_id,
        ) =
            row.map_err(|error| Error::Store(format!("cannot read canonical spend row: {error}")))?;
        let (project, repository) = session_labels(conn, source.as_deref(), session_id.as_deref())?;
        let vendor = match source.as_deref() {
            Some("claude-code" | "anthropic") => Some("anthropic".to_string()),
            Some("codex" | "openai") => Some("openai".to_string()),
            Some(other) => Some(other.to_string()),
            None => {
                if let Some(m) = &model_id {
                    if m.starts_with("claude") {
                        Some("anthropic".to_string())
                    } else if m.starts_with("gpt") {
                        Some("openai".to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };
        events.push(CanonicalSpendEvent {
            canonical_id,
            occurred_at: UtcTimestamp::from_unix_nanos(occurred_at),
            session: session_label(source.as_deref(), session_id.as_deref()),
            project,
            repository,
            evidence_kind,
            sources: provenance
                .split(';')
                .filter(|source| !source.is_empty())
                .map(str::to_string)
                .collect(),
            components: components(conn, event_id)?,
            vendor,
            model: model_id,
        });
    }
    Ok(events)
}

/// Reads durable replay, heuristic and quarantine diagnostics separately from
/// the canonical totals, so `--explain` never changes the reported usage.
pub fn diagnostics(conn: &Connection) -> Result<SpendDiagnostics, Error> {
    let replayed_occurrences = conn
        .query_row(
            "SELECT CASE WHEN COUNT(*) > COUNT(DISTINCT event_id) \
                     THEN COUNT(*) - COUNT(DISTINCT event_id) ELSE 0 END \
             FROM usage_occurrence WHERE event_id IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| Error::Store(format!("cannot count replayed occurrences: {error}")))?
        as u64;
    let heuristic_identities = conn
        .query_row(
            "SELECT COUNT(DISTINCT heuristic_key) FROM usage_occurrence \
             WHERE heuristic_key IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| Error::Store(format!("cannot count heuristic identities: {error}")))?
        as u64;
    let mut quarantined_by_class = BTreeMap::new();
    for group in crate::store::ingest_quarantine::quarantine_summary(conn)? {
        *quarantined_by_class.entry(group.failure_class).or_insert(0) += group.count;
    }
    Ok(SpendDiagnostics {
        quarantined_by_class,
        replayed_occurrences,
        heuristic_identities,
    })
}

fn components(conn: &Connection, event_id: i64) -> Result<BTreeMap<String, u64>, Error> {
    let mut stmt = conn
        .prepare("SELECT token_class, count FROM usage_component WHERE event_id = ?1")
        .map_err(|error| Error::Store(format!("cannot prepare spend components: {error}")))?;
    let rows = stmt
        .query_map(params![event_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| Error::Store(format!("cannot query spend components: {error}")))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (kind, count) =
            row.map_err(|error| Error::Store(format!("cannot read spend component: {error}")))?;
        out.insert(kind, count as u64);
    }
    Ok(out)
}

fn session_labels(
    conn: &Connection,
    source: Option<&str>,
    session: Option<&str>,
) -> Result<(String, String), Error> {
    let (Some(source), Some(session)) = (source, session) else {
        return Ok((UNKNOWN_PROJECT.to_string(), UNKNOWN_REPOSITORY.to_string()));
    };
    match conn.query_row(
        "SELECT project_key, repository_key FROM session \
         WHERE source = ?1 AND native_session_id = ?2",
        params![source, session],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ) {
        Ok(labels) => Ok(labels),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            Ok((UNKNOWN_PROJECT.to_string(), UNKNOWN_REPOSITORY.to_string()))
        }
        Err(error) => Err(Error::Store(format!(
            "cannot read spend session labels: {error}"
        ))),
    }
}

fn session_label(source: Option<&str>, session: Option<&str>) -> String {
    match (source, session) {
        (Some(source), Some(session)) => format!("{source}:{session}"),
        _ => UNKNOWN_SESSION.to_string(),
    }
}
