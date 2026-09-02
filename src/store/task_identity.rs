//! Task-kind identity: candidate persistence, rebuild, and the distribution
//! read the historical-distribution queries group on.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! Candidates are immutable evidence ingested from a tracker read; identities
//! are derived from them under one [`TaskKindMapping`] and are rebuilt, never
//! patched, when the mapping changes. All SQL for both lives here (the
//! SQL-only-in-store boundary rule), and the tracker boundary stays read-only.

use std::collections::BTreeMap;

use rusqlite::params;

use crate::attribution::{
    ResolvedTaskKind, TaskIdentityState, TaskKind, TaskKindCandidate, TaskKindMapping,
    TaskKindOrigin, TrackerTaskReader, TrackerTaskRecord, emit_task_kind_candidates,
    resolve_task_kind,
};
use crate::domain::ids::{NativeTaskId, SourceNamespace, TaskId};
use crate::error::Error;

/// A Beads tracker task reader over the `issues` and `labels` tables. It
/// receives an already-open connection and exposes only the read-only
/// [`TrackerTaskReader`] interface.
///
/// The selected columns are the tracker's structured kind evidence and
/// nothing else: the record type cannot carry a title or description, so no
/// candidate can derive from free-form task text.
pub struct BeadsTaskKindReader<'connection> {
    connection: &'connection rusqlite::Connection,
}

impl<'connection> BeadsTaskKindReader<'connection> {
    pub fn new(connection: &'connection rusqlite::Connection) -> Self {
        Self { connection }
    }
}

impl TrackerTaskReader for BeadsTaskKindReader<'_> {
    fn read_tasks(&self) -> Result<Vec<TrackerTaskRecord>, Error> {
        let mut statement = self
            .connection
            .prepare("SELECT id, issue_type FROM issues ORDER BY id")
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot read tracker issues: {error}"))
            })?;
        let mut tasks: Vec<TrackerTaskRecord> = statement
            .query_map([], |row| {
                Ok(TrackerTaskRecord {
                    native: row.get(0)?,
                    issue_type: row.get(1)?,
                    labels: Vec::new(),
                })
            })
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot query tracker issues: {error}"))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot decode tracker issue: {error}"))
            })?;

        let mut statement = self
            .connection
            .prepare("SELECT issue_id, label FROM labels ORDER BY issue_id, label")
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot read tracker labels: {error}"))
            })?;
        let mut label_rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot query tracker labels: {error}"))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                Error::IngestIncomplete(format!("cannot decode tracker label: {error}"))
            })?;
        label_rows.sort();
        let mut labels_by_task: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (issue_id, label) in label_rows {
            labels_by_task.entry(issue_id).or_default().push(label);
        }
        for task in &mut tasks {
            if let Some(labels) = labels_by_task.remove(&task.native) {
                task.labels = labels;
            }
        }
        Ok(tasks)
    }
}

/// Counts one candidate-ingestion pass. Destination uniqueness makes the
/// operation incremental and safe to repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskKindIngestSummary {
    pub candidates_inserted: u64,
    pub candidates_already_present: u64,
}

/// Persists every candidate the tracker reader exposes, raw and unmapped:
/// normalization applies at rebuild time so a mapping change re-evaluates the
/// same evidence.
pub fn ingest_task_kind_candidates<R: TrackerTaskReader>(
    connection: &rusqlite::Connection,
    tracker_source: SourceNamespace,
    reader: &R,
) -> Result<TaskKindIngestSummary, Error> {
    let mut summary = TaskKindIngestSummary {
        candidates_inserted: 0,
        candidates_already_present: 0,
    };
    for record in reader.read_tasks()? {
        for candidate in emit_task_kind_candidates(tracker_source.clone(), record) {
            if insert_candidate(connection, &candidate)? {
                summary.candidates_inserted += 1;
            } else {
                summary.candidates_already_present += 1;
            }
        }
    }
    Ok(summary)
}

fn insert_candidate(
    connection: &rusqlite::Connection,
    candidate: &TaskKindCandidate,
) -> Result<bool, Error> {
    connection
        .execute(
            "INSERT INTO task_kind_candidate (
                task_source, task_native, origin, raw_value
            ) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT (task_source, task_native, origin, raw_value) DO NOTHING",
            params![
                candidate.task_id.source().as_str(),
                candidate.task_id.native().as_str(),
                candidate.origin.provenance_id(),
                candidate.raw_value,
            ],
        )
        .map(|rows| rows == 1)
        .map_err(|error| Error::Store(format!("cannot insert task-kind candidate: {error}")))
}

/// Counts one rebuild pass over immutable candidate evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskIdentityRebuildSummary {
    pub identities_written: u64,
}

/// Re-evaluates every task's identity from the persisted candidates under the
/// given mapping, replacing the derived identity table wholesale inside one
/// transaction. Candidates are never mutated; only the derived state moves.
///
/// A task with no candidate rows is not in the identity table at all: absence
/// means nothing was ever ingested about the task, which is a different fact
/// from evidence that mapped to no kind.
pub fn rebuild_task_identities(
    connection: &rusqlite::Connection,
    mapping: &TaskKindMapping,
) -> Result<TaskIdentityRebuildSummary, Error> {
    let groups = read_candidate_groups(connection)?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| Error::Store(format!("cannot begin identity rebuild: {error}")))?;
    transaction
        .execute("DELETE FROM task_identity", [])
        .map_err(|error| Error::Store(format!("cannot clear task identities: {error}")))?;
    let mut written = 0u64;
    for (task_source, task_native, candidates) in &groups {
        let resolved = resolve_task_kind(candidates, mapping);
        insert_identity(
            &transaction,
            task_source,
            task_native,
            resolved,
            mapping.version(),
        )?;
        written += 1;
    }
    transaction
        .commit()
        .map_err(|error| Error::Store(format!("cannot commit identity rebuild: {error}")))?;
    Ok(TaskIdentityRebuildSummary {
        identities_written: written,
    })
}

/// Reads candidate rows grouped by task, in a deterministic order so the
/// rebuild's evidence rendering cannot depend on rowid order.
fn read_candidate_groups(
    connection: &rusqlite::Connection,
) -> Result<Vec<(String, String, Vec<TaskKindCandidate>)>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT task_source, task_native, origin, raw_value
             FROM task_kind_candidate
             ORDER BY task_source, task_native, origin, raw_value",
        )
        .map_err(|error| Error::Store(format!("cannot read task-kind candidates: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| Error::Store(format!("cannot query task-kind candidates: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode task-kind candidate: {error}")))?;

    let mut groups: Vec<(String, String, Vec<TaskKindCandidate>)> = Vec::new();
    for (task_source, task_native, origin_raw, raw_value) in rows {
        let origin = TaskKindOrigin::from_provenance_id(&origin_raw).ok_or_else(|| {
            Error::Store(format!(
                "task-kind candidate carries an unparseable origin: {origin_raw}"
            ))
        })?;
        let candidate = TaskKindCandidate {
            task_id: TaskId::new(
                SourceNamespace::new(task_source.clone()),
                NativeTaskId::new(task_native.clone()),
            ),
            origin,
            raw_value,
        };
        match groups.last_mut() {
            Some((source, native, list)) if *source == task_source && *native == task_native => {
                list.push(candidate);
            }
            _ => groups.push((task_source, task_native, vec![candidate])),
        }
    }
    Ok(groups)
}

fn insert_identity(
    connection: &rusqlite::Connection,
    task_source: &str,
    task_native: &str,
    resolved: ResolvedTaskKind,
    normalization_version: u32,
) -> Result<(), Error> {
    let state = resolved.state().state_label();
    let (kind, winner, evidence) = match resolved {
        ResolvedTaskKind::Resolved {
            kind,
            winner,
            evidence,
        } => (Some(kind.as_str()), Some(winner.provenance_id()), evidence),
        ResolvedTaskKind::Unknown { evidence } | ResolvedTaskKind::Conflict { evidence } => {
            (None, None, evidence)
        }
    };
    connection
        .execute(
            "INSERT INTO task_identity (
                task_source, task_native, state, kind, winner_origin,
                evidence, normalization_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task_source,
                task_native,
                state,
                kind,
                winner,
                evidence,
                normalization_version,
            ],
        )
        .map(|_| ())
        .map_err(|error| Error::Store(format!("cannot insert task identity: {error}")))
}

/// One task's persisted identity, read back from the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIdentityRow {
    pub task_id: TaskId,
    pub state: TaskIdentityState,
    pub kind: Option<TaskKind>,
    pub winner: Option<TaskKindOrigin>,
    pub evidence: String,
    pub normalization_version: u32,
}

/// Reads one task's identity, or `None` when the task has none (either never
/// ingested or no candidates exist for it). A stored value the current code
/// cannot parse is a store failure naming the value, never a silent fallback.
pub fn read_task_identity(
    connection: &rusqlite::Connection,
    task_id: &TaskId,
) -> Result<Option<TaskIdentityRow>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT state, kind, winner_origin, evidence, normalization_version
             FROM task_identity
             WHERE task_source = ?1 AND task_native = ?2",
        )
        .map_err(|error| Error::Store(format!("cannot read task identity: {error}")))?;
    let mut rows = statement
        .query_map(
            params![task_id.source().as_str(), task_id.native().as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u32>(4)?,
                ))
            },
        )
        .map_err(|error| Error::Store(format!("cannot query task identity: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode task identity: {error}")))?;
    let Some((state, kind, winner, evidence, version)) = rows.pop() else {
        return Ok(None);
    };
    if rows.len() > 1 {
        return Err(Error::Store(
            "task identity UNIQUE constraint was violated at the database".to_owned(),
        ));
    }
    let state = TaskIdentityState::parse(&state).ok_or_else(|| {
        Error::Store(format!(
            "task identity carries an unknown state label: {state}"
        ))
    })?;
    let kind = match kind.as_deref() {
        None => None,
        Some(raw) => Some(TaskKind::parse(raw).ok_or_else(|| {
            Error::Store(format!("task identity carries an unknown kind: {raw}"))
        })?),
    };
    let winner = match winner.as_deref() {
        None => None,
        Some(raw) => Some(TaskKindOrigin::from_provenance_id(raw).ok_or_else(|| {
            Error::Store(format!(
                "task identity carries an unparseable winner origin: {raw}"
            ))
        })?),
    };
    Ok(Some(TaskIdentityRow {
        task_id: task_id.clone(),
        state,
        kind,
        winner,
        evidence,
        normalization_version: version,
    }))
}

/// The historical-distribution input over task kinds: grouped counts by
/// justified kind plus the unknown and conflict counts, in typed states.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskKindDistribution {
    /// Count of tasks resolved to each kind, ordered by kind.
    pub resolved: Vec<(TaskKind, u64)>,
    pub unknown: u64,
    pub conflict: u64,
}

impl TaskKindDistribution {
    /// The count for one kind, or zero when no task resolved to it.
    pub fn count_for(&self, kind: TaskKind) -> u64 {
        self.resolved
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    }
}

/// Reads the task-kind distribution input from the persisted identities.
pub fn task_kind_distribution(
    connection: &rusqlite::Connection,
) -> Result<TaskKindDistribution, Error> {
    let mut statement = connection
        .prepare("SELECT state, kind, COUNT(*) FROM task_identity GROUP BY state, kind")
        .map_err(|error| Error::Store(format!("cannot read task-kind distribution: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })
        .map_err(|error| Error::Store(format!("cannot query task-kind distribution: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode task-kind distribution: {error}")))?;

    let mut distribution = TaskKindDistribution::default();
    for (state, kind, count) in rows {
        match state.as_str() {
            "resolved" => {
                let raw = kind.ok_or_else(|| {
                    Error::Store(
                        "resolved identity row carries no kind, though the database forbids it"
                            .to_owned(),
                    )
                })?;
                let kind = TaskKind::parse(&raw).ok_or_else(|| {
                    Error::Store(format!(
                        "task-kind distribution carries an unknown kind: {raw}"
                    ))
                })?;
                distribution.resolved.push((kind, count));
            }
            "unknown" => distribution.unknown += count,
            "conflict" => distribution.conflict += count,
            other => {
                return Err(Error::Store(format!(
                    "task-kind distribution carries an unknown state label: {other}"
                )));
            }
        }
    }
    distribution.resolved.sort();
    Ok(distribution)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-task-identity-test-{}-{suffix}",
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
        let mut connection = open(
            &scratch.path().join("state.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(1_000),
            },
        )
        .unwrap();
        crate::store::migrate::run_migrations(
            &mut connection,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, connection)
    }

    #[derive(Clone)]
    struct FixtureReader(Vec<TrackerTaskRecord>);

    impl TrackerTaskReader for FixtureReader {
        fn read_tasks(&self) -> Result<Vec<TrackerTaskRecord>, Error> {
            Ok(self.0.clone())
        }
    }

    fn record(native: &str, issue_type: &str, labels: &[&str]) -> TrackerTaskRecord {
        TrackerTaskRecord {
            native: native.to_owned(),
            issue_type: issue_type.to_owned(),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        }
    }

    #[test]
    fn reingesting_the_same_tasks_is_idempotent() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![record("aub-1", "task", &["testing", "decision"])]);

        let first =
            ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader)
                .unwrap();
        assert_eq!(first.candidates_inserted, 3);
        assert_eq!(first.candidates_already_present, 0);

        let second =
            ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader)
                .unwrap();
        assert_eq!(second.candidates_inserted, 0);
        assert_eq!(second.candidates_already_present, 3);
    }

    #[test]
    fn a_second_identity_row_for_one_task_fails_at_the_database() {
        let (_scratch, connection) = fixture_connection();
        insert_identity(
            &connection,
            "beads-a",
            "aub-1",
            ResolvedTaskKind::Resolved {
                kind: TaskKind::Task,
                winner: TaskKindOrigin::TrackerField("issue_type".to_owned()),
                evidence: "tracker_field:issue_type=task".to_owned(),
            },
            1,
        )
        .unwrap();
        let second = insert_identity(
            &connection,
            "beads-a",
            "aub-1",
            ResolvedTaskKind::Unknown {
                evidence: "tracker_label:testing=testing".to_owned(),
            },
            1,
        );
        let error = second.unwrap_err();
        assert!(error.to_string().contains("UNIQUE"));
    }

    #[test]
    fn rebuild_replaces_the_derived_state_and_round_trips() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![record("aub-1", "task", &["testing"])]);
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();

        let first = rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();
        assert_eq!(first.identities_written, 1);

        let task_id = TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new("aub-1"));
        let row = read_task_identity(&connection, &task_id).unwrap().unwrap();
        assert_eq!(row.state, TaskIdentityState::Resolved);
        assert_eq!(row.kind, Some(TaskKind::Task));
        assert_eq!(
            row.winner,
            Some(TaskKindOrigin::TrackerField("issue_type".to_owned()))
        );
        assert_eq!(row.normalization_version, 1);

        let second = rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();
        assert_eq!(second.identities_written, 1);
        let after = read_task_identity(&connection, &task_id).unwrap().unwrap();
        assert_eq!(row, after);
    }

    #[test]
    fn a_mapping_change_changes_the_rebuild_while_candidates_stay_put() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![record("aub-1", "task", &["testing"])]);
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();

        rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();
        let task_id = TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new("aub-1"));
        let before = read_task_identity(&connection, &task_id).unwrap().unwrap();
        assert_eq!(before.kind, Some(TaskKind::Task));

        let v2 = TaskKindMapping::new(
            2,
            [
                ("task", TaskKind::Docs),
                ("epic", TaskKind::Epic),
                ("bug", TaskKind::Bug),
                ("docs", TaskKind::Docs),
                ("question", TaskKind::Question),
                ("testing", TaskKind::Bug),
            ]
            .into_iter()
            .map(|(raw, kind)| (raw.to_owned(), kind)),
        )
        .unwrap();
        rebuild_task_identities(&connection, &v2).unwrap();
        let after = read_task_identity(&connection, &task_id).unwrap().unwrap();
        // The identity-field candidate ("task") still wins on rank, but it now
        // normalizes to the versioned mapping's answer, and the version moved.
        assert_eq!(after.kind, Some(TaskKind::Docs));
        assert_eq!(after.normalization_version, 2);
        assert_eq!(before.evidence, after.evidence);
    }

    #[test]
    fn distribution_reports_resolved_unknown_and_conflict_counts() {
        let (_scratch, connection) = fixture_connection();
        let reader = FixtureReader(vec![
            // Resolved by the identity field at rank 1.
            record("aub-1", "task", &[]),
            // Identity field value outside the mapping and no tags: unknown.
            record("aub-2", "spike", &[]),
            record("aub-3", "docs", &[]),
            // Identity field says nothing, two equal-rank tags disagree:
            // a typed conflict with no winner, whatever the ingest order.
            record("aub-4", "spike", &["alpha", "beta"]),
        ]);
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
        let mapping = TaskKindMapping::new(
            3,
            [
                ("task", TaskKind::Task),
                ("epic", TaskKind::Epic),
                ("bug", TaskKind::Bug),
                ("docs", TaskKind::Docs),
                ("question", TaskKind::Question),
                ("alpha", TaskKind::Bug),
                ("beta", TaskKind::Docs),
            ]
            .into_iter()
            .map(|(raw, kind)| (raw.to_owned(), kind)),
        )
        .unwrap();
        rebuild_task_identities(&connection, &mapping).unwrap();

        let distribution = task_kind_distribution(&connection).unwrap();
        assert_eq!(distribution.count_for(TaskKind::Task), 1);
        assert_eq!(distribution.count_for(TaskKind::Docs), 1);
        assert_eq!(distribution.unknown, 1);
        assert_eq!(distribution.conflict, 1);
    }

    #[test]
    fn a_task_with_no_candidates_has_no_identity_row() {
        let (_scratch, connection) = fixture_connection();
        rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();
        let task_id = TaskId::new(
            SourceNamespace::new("beads-a"),
            NativeTaskId::new("aub-absent"),
        );
        assert!(read_task_identity(&connection, &task_id).unwrap().is_none());
    }

    #[test]
    fn the_beads_reader_maps_issue_and_label_columns_without_free_form_text() {
        let (_scratch, connection) = fixture_connection();
        connection
            .execute_batch(
                "CREATE TABLE issues (
                    id TEXT PRIMARY KEY,
                    issue_type TEXT NOT NULL DEFAULT 'task',
                    title TEXT NOT NULL DEFAULT ''
                );
                CREATE TABLE labels (
                    issue_id TEXT NOT NULL,
                    label TEXT NOT NULL,
                    PRIMARY KEY (issue_id, label)
                );
                INSERT INTO issues (id, issue_type, title) VALUES
                    ('aub-7', 'bug', 'a title that must never become evidence');
                INSERT INTO labels VALUES ('aub-7', 'testing');",
            )
            .unwrap();
        let tasks = BeadsTaskKindReader::new(&connection).read_tasks().unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].native, "aub-7");
        assert_eq!(tasks[0].issue_type, "bug");
        assert_eq!(tasks[0].labels, vec!["testing".to_owned()]);
        // The candidate set is exactly the categorical values, so the title the
        // fixture carries never becomes evidence.
        let candidates =
            emit_task_kind_candidates(SourceNamespace::new("beads-a"), tasks[0].clone());
        let raw_values: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.raw_value.as_str())
            .collect();
        assert_eq!(raw_values, vec!["bug", "testing"]);
    }
}
