//! Durable issue-tracker event ingestion.
//!
//! This module owns both writes into `aub`'s task-event tables and reads from a
//! Beads tracker connection. The tracker boundary remains read-only: no function
//! here creates, updates, or closes a tracker issue.

use rusqlite::params;

use crate::attribution::segment::{
    ClaimBoundary, ClaimResolutionCounts, ClaimSessionResolution, resolve_claim_session,
};
use crate::attribution::{
    TaskEvent, TaskEventKind, TaskEventQuarantine, TrackerEventReader, TrackerEventRecord,
    normalize_tracker_event,
};
use crate::domain::ids::{NativeTaskId, SessionId, SourceNamespace, TaskId};
use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::error::Error;
use crate::store::connection::{self, AccessMode, PragmaPolicy};

/// A Beads `events` table reader. It receives an already-open connection and only
/// exposes the read-only [`TrackerEventReader`] interface to callers.
pub struct BeadsEventReader<'connection> {
    connection: &'connection rusqlite::Connection,
}

impl<'connection> BeadsEventReader<'connection> {
    pub fn new(connection: &'connection rusqlite::Connection) -> Self {
        Self { connection }
    }
}

impl TrackerEventReader for BeadsEventReader<'_> {
    fn read_events(&self) -> Result<Vec<TrackerEventRecord>, Error> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, issue_id, event_type, old_value, new_value, created_at, actor
                 FROM events ORDER BY id",
            )
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot read tracker events: {error}"))
            })?;
        statement
            .query_map([], |row| {
                Ok(TrackerEventRecord {
                    upstream_id: row.get(0)?,
                    task_native: row.get(1)?,
                    event_type: row.get(2)?,
                    old_value: row.get(3)?,
                    new_value: row.get(4)?,
                    occurred_at: row.get(5)?,
                    actor: row.get(6)?,
                })
            })
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot query tracker events: {error}"))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot decode tracker event: {error}"))
            })
    }
}

/// Counts one ingestion pass. Duplicate rows prove an unchanged export cannot grow
/// the durable event history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestSummary {
    pub events_inserted: u64,
    pub events_already_present: u64,
    pub quarantines_inserted: u64,
    pub quarantines_already_present: u64,
}

/// Ingests every record a tracker reader exposes. Destination-table uniqueness makes
/// the operation incremental and safe to repeat.
pub fn ingest<R: TrackerEventReader>(
    state: &rusqlite::Connection,
    tracker_source: SourceNamespace,
    reader: &R,
) -> Result<IngestSummary, Error> {
    let mut summary = IngestSummary {
        events_inserted: 0,
        events_already_present: 0,
        quarantines_inserted: 0,
        quarantines_already_present: 0,
    };
    for record in reader.read_events()? {
        match normalize_tracker_event(tracker_source.clone(), record) {
            Ok(event) => {
                if insert_event(state, &event)? {
                    summary.events_inserted += 1;
                } else {
                    summary.events_already_present += 1;
                }
            }
            Err(quarantine) => {
                if insert_quarantine(state, &quarantine)? {
                    summary.quarantines_inserted += 1;
                } else {
                    summary.quarantines_already_present += 1;
                }
            }
        }
    }
    Ok(summary)
}

/// Opens the tracker's own SQLite database read-only, for wrapping in
/// [`BeadsEventReader`].
///
/// Routed through [`connection::open`] with [`AccessMode::ForeignReadOnly`], which
/// applies no pragma policy: that policy verifies journal mode, synchronous level
/// and foreign-key enforcement this project's own ledger is built to hold, and the
/// tracker database belongs to a different project entirely. Enforcing `aub`'s
/// pragma policy against a schema and file it does not own would refuse a healthy
/// tracker database for disagreeing with settings it was never asked to hold. The
/// busy timeout below is unused for this mode; it exists only because `open`
/// takes one policy value for every mode.
pub fn open_tracker_database(path: &std::path::Path) -> Result<rusqlite::Connection, Error> {
    connection::open(
        path,
        AccessMode::ForeignReadOnly,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_seconds(0),
        },
    )
}

/// Every boundary one scan read, and what the scan made of the actors behind
/// them. The counts travel with the boundaries because they are two views of
/// one pass: recomputing them anywhere else would mean resolving every actor
/// twice and risking the two answers drifting apart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskBoundaryScan {
    pub boundaries: Vec<ClaimBoundary>,
    pub resolution: ClaimResolutionCounts,
}

/// Reads every durably ingested claim/release boundary, tracker-wide and
/// ordered by occurrence, for the segmentation engine in
/// [`crate::attribution::segment`]. A tracker event whose kind normalized to
/// `TaskEventKind::Unknown` was retained at ingest time as durable history
/// but carries no attribution meaning, so it is filtered out here rather than
/// in the segmentation engine: `segment::build_intervals` already documents
/// that it ignores `Unknown` boundaries, and filtering here means a caller
/// never constructs one only to have it silently ignored two layers down.
///
/// Each boundary's stored `agent_association` is resolved against the
/// ledger's own session table here, at read time, by
/// [`resolve_claim_session`]. Nothing about the resolution is persisted: the
/// actor string and the session rows are both immutable inputs, so two reads
/// of an unchanged ledger produce the same binding, and a session ingested
/// later starts binding claims that were already on disk without any
/// rewrite. The counts it returns are over every boundary row, claim and
/// release alike, since a release that binds to no session governs the
/// fallback timeline exactly as an unbound claim does.
pub fn read_boundaries(connection: &rusqlite::Connection) -> Result<TaskBoundaryScan, Error> {
    let known: Vec<SessionId> = crate::store::session::load_all_sessions(connection)?
        .into_iter()
        .map(|session| {
            SessionId::new(
                session.source().clone(),
                session.native_session_id().clone(),
            )
        })
        .collect();
    let mut statement = connection
        .prepare(
            "SELECT task_source, task_native, occurred_at, event_kind, agent_association \
             FROM task_event WHERE event_kind IN ('claim', 'release') \
             ORDER BY occurred_at",
        )
        .map_err(|error| Error::Store(format!("cannot prepare task boundary scan: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|error| Error::Store(format!("cannot query task boundaries: {error}")))?;
    let mut scan = TaskBoundaryScan::default();
    for row in rows {
        let (task_source, task_native, occurred_at, event_kind, agent_association) =
            row.map_err(|error| Error::Store(format!("cannot read task boundary row: {error}")))?;
        let kind = match event_kind.as_str() {
            "claim" => TaskEventKind::Claim,
            "release" => TaskEventKind::Release,
            other => {
                return Err(Error::Store(format!(
                    "task_event carries an unexpected boundary kind: {other}"
                )));
            }
        };
        let resolution = resolve_claim_session(agent_association.as_deref(), &known);
        scan.resolution.tally(&resolution);
        scan.boundaries.push(ClaimBoundary {
            task_id: TaskId::new(
                SourceNamespace::new(task_source),
                NativeTaskId::new(task_native),
            ),
            occurred_at: UtcTimestamp::from_unix_nanos(occurred_at),
            kind,
            agent_association,
            session: match resolution {
                ClaimSessionResolution::Session(session) => Some(session),
                ClaimSessionResolution::Unresolved | ClaimSessionResolution::Ambiguous => None,
            },
        });
    }
    Ok(scan)
}

fn insert_event(connection: &rusqlite::Connection, event: &TaskEvent) -> Result<bool, Error> {
    connection
        .execute(
            "INSERT INTO task_event (
                tracker_source, tracker_event_id, task_source, task_native,
                occurred_at, event_kind, agent_association
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT (tracker_source, tracker_event_id) DO NOTHING",
            params![
                event.tracker_source.as_str(),
                event.upstream_id,
                event.task_id.source().as_str(),
                event.task_id.native().as_str(),
                event.occurred_at.unix_nanos(),
                event.kind.as_str(),
                event.agent_association,
            ],
        )
        .map(|rows| rows == 1)
        .map_err(|error| Error::Store(format!("cannot insert task event: {error}")))
}

fn insert_quarantine(
    connection: &rusqlite::Connection,
    quarantine: &TaskEventQuarantine,
) -> Result<bool, Error> {
    connection
        .execute(
            "INSERT INTO task_event_quarantine (
                tracker_source, tracker_event_id, raw_timestamp, reason
            ) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT (tracker_source, tracker_event_id) DO NOTHING",
            params![
                quarantine.tracker_source.as_str(),
                quarantine.upstream_id,
                quarantine.raw_timestamp,
                quarantine.reason,
            ],
        )
        .map(|rows| rows == 1)
        .map_err(|error| Error::Store(format!("cannot quarantine task event: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-task-event-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
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

    fn fixture_connection() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let connection = crate::store::test_schema::open_migrated(
            &scratch.path().join("state.db"),
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        );
        (scratch, connection)
    }

    #[derive(Clone)]
    struct FixtureReader(Vec<TrackerEventRecord>);

    impl TrackerEventReader for FixtureReader {
        fn read_events(&self) -> Result<Vec<TrackerEventRecord>, Error> {
            Ok(self.0.clone())
        }
    }

    fn record(
        upstream_id: i64,
        task_native: &str,
        event_type: &str,
        old_value: Option<&str>,
        new_value: Option<&str>,
        occurred_at: &str,
    ) -> TrackerEventRecord {
        TrackerEventRecord {
            upstream_id,
            task_native: task_native.into(),
            event_type: event_type.into(),
            old_value: old_value.map(str::to_owned),
            new_value: new_value.map(str::to_owned),
            occurred_at: occurred_at.into(),
            actor: Some("agent-1".into()),
        }
    }

    #[test]
    fn reingesting_an_unchanged_export_is_idempotent_and_quarantines_bad_time() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![
            record(
                1,
                "aub-1",
                "status_changed",
                Some("open"),
                Some("in_progress"),
                "2026-08-31T19:11:34.47746272Z",
            ),
            record(
                2,
                "aub-1",
                "status_changed",
                Some("in_progress"),
                Some("open"),
                "2026-08-31T19:12:34.47746272Z",
            ),
            record(3, "aub-1", "commented", None, None, "2026-08-31T19:13:34Z"),
            record(
                4,
                "aub-1",
                "status_changed",
                None,
                Some("in_progress"),
                "bad",
            ),
        ]);

        let first = ingest(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
        assert_eq!(first.events_inserted, 3);
        assert_eq!(first.quarantines_inserted, 1);

        let second = ingest(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.quarantines_inserted, 0);
        assert_eq!(second.events_already_present, 3);
        assert_eq!(second.quarantines_already_present, 1);

        let events: i64 = connection
            .query_row("SELECT COUNT(*) FROM task_event", [], |row| row.get(0))
            .unwrap();
        let quarantines: i64 = connection
            .query_row("SELECT COUNT(*) FROM task_event_quarantine", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(events, 3);
        assert_eq!(quarantines, 1);
    }

    #[test]
    fn identical_native_task_ids_from_two_sources_remain_distinct() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![record(
            1,
            "same-task",
            "status_changed",
            Some("open"),
            Some("in_progress"),
            "2026-08-31T19:11:34Z",
        )]);

        ingest(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
        ingest(&connection, SourceNamespace::new("beads-b"), &reader).unwrap();

        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM task_event WHERE task_native = 'same-task'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn the_tracker_reader_interface_exposes_reads_only() {
        fn accepts_reader(_: &dyn TrackerEventReader) {}

        accepts_reader(&FixtureReader(vec![]));
    }

    #[test]
    fn beads_reader_maps_the_live_event_columns_without_a_write_surface() {
        let (_scratch, connection) = fixture_connection();
        connection
            .execute_batch(
                "CREATE TABLE events (
                    id INTEGER PRIMARY KEY,
                    issue_id TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    actor TEXT NOT NULL,
                    old_value TEXT,
                    new_value TEXT,
                    created_at TEXT NOT NULL
                );
                INSERT INTO events VALUES (
                    7, 'aub-7', 'status_changed', 'agent-7', 'open',
                    'in_progress', '2026-08-31T19:11:34Z'
                );",
            )
            .unwrap();
        let records = BeadsEventReader::new(&connection).read_events().unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].upstream_id, 7);
        assert_eq!(records[0].task_native, "aub-7");
        assert_eq!(records[0].actor.as_deref(), Some("agent-7"));
    }

    #[test]
    fn open_tracker_database_opens_read_only_and_refuses_a_missing_file() {
        let scratch = ScratchDir::new();
        let path = scratch.path().join("beads.db");
        open(
            &path,
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        )
        .unwrap()
        .execute_batch("CREATE TABLE probe (id INTEGER PRIMARY KEY);")
        .unwrap();

        let conn = open_tracker_database(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        // The planted negative: a write attempt against the read-only handle
        // must fail rather than silently succeed, proving the flags actually
        // took effect and this is not merely a connection that happens not
        // to have been written to yet.
        assert!(
            conn.execute("INSERT INTO probe DEFAULT VALUES", [])
                .is_err()
        );

        let missing = scratch.path().join("does-not-exist.db");
        assert!(open_tracker_database(&missing).is_err());
    }

    #[test]
    fn read_boundaries_returns_claim_and_release_events_ordered_and_drops_unknown() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![
            record(
                1,
                "aub-1",
                "status_changed",
                Some("open"),
                Some("in_progress"),
                "2026-08-31T19:20:00Z",
            ),
            record(2, "aub-1", "commented", None, None, "2026-08-31T19:10:00Z"),
            record(
                3,
                "aub-1",
                "status_changed",
                Some("in_progress"),
                Some("open"),
                "2026-08-31T19:05:00Z",
            ),
        ]);
        ingest(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();

        let boundaries = read_boundaries(&connection).unwrap().boundaries;

        // The planted negative: a naive reader that forgot the `event_kind`
        // filter would return three rows, including the `commented` event
        // whose kind carries no attribution meaning.
        assert_eq!(boundaries.len(), 2);
        assert_eq!(boundaries[0].kind, TaskEventKind::Release);
        assert_eq!(
            boundaries[0].occurred_at,
            UtcTimestamp::parse_rfc3339("2026-08-31T19:05:00Z").unwrap()
        );
        assert_eq!(boundaries[1].kind, TaskEventKind::Claim);
        assert_eq!(
            boundaries[1].occurred_at,
            UtcTimestamp::parse_rfc3339("2026-08-31T19:20:00Z").unwrap()
        );
        assert_eq!(boundaries[0].task_id.native().as_str(), "aub-1");
        // The actor the tracker recorded travels with the boundary: without
        // it there is nothing to resolve a session from two layers up.
        assert_eq!(boundaries[0].agent_association.as_deref(), Some("agent-1"));
    }

    fn seed_session(connection: &rusqlite::Connection, native: &str) {
        crate::store::session::insert_session(
            connection,
            &crate::store::session::NewSession {
                source: SourceNamespace::new("claude-code"),
                native_session_id: crate::domain::ids::NativeSessionId::new(native),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: crate::sessions::ProjectKey::new("project-a"),
                repository_key: crate::sessions::RepositoryKey::new("repository-a"),
                working_directory: None,
                parent_native_session_id: None,
                run_id: None,
            },
        )
        .unwrap();
    }

    fn claim_by(upstream_id: i64, actor: &str, occurred_at: &str) -> TrackerEventRecord {
        TrackerEventRecord {
            upstream_id,
            task_native: "aub-1".into(),
            event_type: "status_changed".into(),
            old_value: Some("open".into()),
            new_value: Some("in_progress".into()),
            occurred_at: occurred_at.into(),
            actor: Some(actor.into()),
        }
    }

    #[test]
    fn a_boundary_binds_to_the_session_its_actor_names() {
        let (_scratch, connection) = fixture_connection();
        seed_session(&connection, "c4ba2d16-8f0c-4a11-9d2e-000000000001");
        ingest(
            &connection,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![claim_by(
                1,
                "drive-beads-c4ba2d16",
                "2026-08-31T19:20:00Z",
            )]),
        )
        .unwrap();

        let scan = read_boundaries(&connection).unwrap();

        assert_eq!(
            scan.boundaries[0]
                .session
                .as_ref()
                .map(|session| session.native().as_str().to_string()),
            Some("c4ba2d16-8f0c-4a11-9d2e-000000000001".to_string())
        );
        assert_eq!(scan.resolution, ClaimResolutionCounts::default());
    }

    /// Criterion 4: a fragment fitting two sessions binds to neither and is
    /// counted as ambiguous, with the exact count asserted. The unresolved
    /// count stays at its own value so the two are not one bucket wearing two
    /// names.
    #[test]
    fn an_actor_fitting_two_sessions_binds_to_neither_and_is_counted() {
        let (_scratch, connection) = fixture_connection();
        seed_session(&connection, "eb1b1539-aaaa-4a11-9d2e-000000000001");
        seed_session(&connection, "eb1b1539-bbbb-4a11-9d2e-000000000002");
        ingest(
            &connection,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![
                claim_by(1, "implementer-eb1b1539", "2026-08-31T19:20:00Z"),
                claim_by(2, "gabriel", "2026-08-31T19:30:00Z"),
            ]),
        )
        .unwrap();

        let scan = read_boundaries(&connection).unwrap();

        assert_eq!(scan.boundaries.len(), 2);
        assert!(
            scan.boundaries
                .iter()
                .all(|boundary| boundary.session.is_none()),
            "neither an ambiguous nor a session-less actor may bind to a session"
        );
        assert_eq!(scan.resolution.ambiguous, 1);
        assert_eq!(scan.resolution.unresolved, 1);
    }

    /// Criterion 6, at the layer that could break it: the binding is derived
    /// from immutable rows at read time, so a second ingest of the same
    /// tracker leaves the scan byte-identical.
    #[test]
    fn a_second_ingest_of_the_same_tracker_leaves_the_scan_identical() {
        let (_scratch, connection) = fixture_connection();
        seed_session(&connection, "c4ba2d16-8f0c-4a11-9d2e-000000000001");
        let reader = FixtureReader(vec![
            claim_by(1, "drive-beads-c4ba2d16", "2026-08-31T19:20:00Z"),
            claim_by(2, "gabriel", "2026-08-31T19:30:00Z"),
        ]);
        ingest(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
        let first = read_boundaries(&connection).unwrap();

        ingest(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
        let second = read_boundaries(&connection).unwrap();

        assert_eq!(first, second);
    }
}
