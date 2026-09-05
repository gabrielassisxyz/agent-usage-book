//! Task identity: candidate persistence, rebuild, and the distribution read
//! the historical-distribution queries group on.
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
    ResolvedTaskDifficulty, ResolvedTaskKind, ResolvedTaskSize, TaskDifficulty, TaskIdentityState,
    TaskKind, TaskKindCandidate, TaskKindMapping, TaskKindOrigin, TaskSize, TrackerTaskReader,
    TrackerTaskRecord, emit_task_kind_candidates, resolve_task_difficulty, resolve_task_kind,
    resolve_task_size,
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
        let resolved_kind = resolve_task_kind(candidates, mapping);
        let resolved_size = resolve_task_size(candidates, mapping);
        let resolved_difficulty = resolve_task_difficulty(candidates, mapping);
        insert_identity_with_classification(
            &transaction,
            task_source,
            task_native,
            resolved_kind,
            resolved_size,
            resolved_difficulty,
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
            Some(_) | None => groups.push((task_source, task_native, vec![candidate])),
        }
    }
    Ok(groups)
}

#[cfg(test)]
fn insert_identity(
    connection: &rusqlite::Connection,
    task_source: &str,
    task_native: &str,
    resolved: ResolvedTaskKind,
    normalization_version: u32,
) -> Result<(), Error> {
    insert_identity_with_classification(
        connection,
        task_source,
        task_native,
        resolved,
        ResolvedTaskSize::Unknown {
            evidence: String::new(),
        },
        ResolvedTaskDifficulty::Unknown {
            evidence: String::new(),
        },
        normalization_version,
    )
}

fn insert_identity_with_classification(
    connection: &rusqlite::Connection,
    task_source: &str,
    task_native: &str,
    resolved_kind: ResolvedTaskKind,
    resolved_size: ResolvedTaskSize,
    resolved_difficulty: ResolvedTaskDifficulty,
    normalization_version: u32,
) -> Result<(), Error> {
    let kind_state = resolved_kind.state().state_label();
    let (kind, winner, kind_evidence) = match resolved_kind {
        ResolvedTaskKind::Resolved {
            kind,
            winner,
            evidence,
        } => (Some(kind.as_str()), Some(winner.provenance_id()), evidence),
        ResolvedTaskKind::Unknown { evidence } | ResolvedTaskKind::Conflict { evidence } => {
            (None, None, evidence)
        }
    };
    let size_state = resolved_size.state().state_label();
    let (size, size_evidence) = match resolved_size {
        ResolvedTaskSize::Resolved { size, evidence, .. } => (Some(size.as_str()), evidence),
        ResolvedTaskSize::Unknown { evidence } | ResolvedTaskSize::Conflict { evidence } => {
            (None, evidence)
        }
    };
    let difficulty_state = resolved_difficulty.state().state_label();
    let (difficulty, difficulty_evidence) = match resolved_difficulty {
        ResolvedTaskDifficulty::Resolved {
            difficulty,
            evidence,
            ..
        } => (Some(difficulty.as_str()), evidence),
        ResolvedTaskDifficulty::Unknown { evidence }
        | ResolvedTaskDifficulty::Conflict { evidence } => (None, evidence),
    };
    connection
        .execute(
            "INSERT INTO task_identity (
                task_source, task_native, state, kind, winner_origin,
                evidence, normalization_version, size_state, size, size_evidence,
                difficulty_state, difficulty, difficulty_evidence
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                task_source,
                task_native,
                kind_state,
                kind,
                winner,
                kind_evidence,
                normalization_version,
                size_state,
                size,
                size_evidence,
                difficulty_state,
                difficulty,
                difficulty_evidence,
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
    pub size_state: TaskIdentityState,
    pub size: Option<TaskSize>,
    pub size_evidence: String,
    pub difficulty_state: TaskIdentityState,
    pub difficulty: Option<TaskDifficulty>,
    pub difficulty_evidence: String,
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
            "SELECT state, kind, winner_origin, evidence, normalization_version,
                    size_state, size, size_evidence,
                    difficulty_state, difficulty, difficulty_evidence
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
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                ))
            },
        )
        .map_err(|error| Error::Store(format!("cannot query task identity: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode task identity: {error}")))?;
    let Some((
        state,
        kind,
        winner,
        evidence,
        version,
        size_state,
        size,
        size_evidence,
        difficulty_state,
        difficulty,
        difficulty_evidence,
    )) = rows.pop()
    else {
        return Ok(None);
    };
    if !rows.is_empty() {
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
    let size_state = TaskIdentityState::parse(&size_state).ok_or_else(|| {
        Error::Store(format!(
            "task identity carries an unknown size state label: {size_state}"
        ))
    })?;
    let size = parse_identity_axis_value("size", size_state, size.as_deref(), TaskSize::parse)?;
    let difficulty_state = TaskIdentityState::parse(&difficulty_state).ok_or_else(|| {
        Error::Store(format!(
            "task identity carries an unknown difficulty state label: {difficulty_state}"
        ))
    })?;
    let difficulty = parse_identity_axis_value(
        "difficulty",
        difficulty_state,
        difficulty.as_deref(),
        TaskDifficulty::parse,
    )?;
    Ok(Some(TaskIdentityRow {
        task_id: task_id.clone(),
        state,
        kind,
        winner,
        evidence,
        normalization_version: version,
        size_state,
        size,
        size_evidence,
        difficulty_state,
        difficulty,
        difficulty_evidence,
    }))
}

fn parse_identity_axis_value<T>(
    axis: &str,
    state: TaskIdentityState,
    raw: Option<&str>,
    parse: fn(&str) -> Option<T>,
) -> Result<Option<T>, Error> {
    match state {
        TaskIdentityState::Resolved => {
            let raw = raw.ok_or_else(|| {
                Error::Store(format!("resolved task identity {axis} carries no value"))
            })?;
            parse(raw)
                .ok_or_else(|| {
                    Error::Store(format!(
                        "task identity carries an unknown {axis} value: {raw}"
                    ))
                })
                .map(Some)
        }
        TaskIdentityState::Unknown | TaskIdentityState::Conflict => {
            if raw.is_some() {
                return Err(Error::Store(format!(
                    "unresolved task identity {axis} carries a value"
                )));
            }
            Ok(None)
        }
    }
}

/// The difficulty composition inside one size group.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskDifficultyMix {
    pub resolved: Vec<(TaskDifficulty, u64)>,
    pub unknown: u64,
    pub conflict: u64,
}

impl TaskDifficultyMix {
    pub fn count_for(&self, difficulty: TaskDifficulty) -> u64 {
        self.resolved
            .iter()
            .find(|(candidate, _)| *candidate == difficulty)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    }
}

/// One reference-distribution group. Unknown and conflicting difficulty
/// states remain in the mix because difficulty is a filterable secondary axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSizeGroup {
    pub size: TaskSize,
    pub sample_count: u64,
    pub difficulty_mix: TaskDifficultyMix,
}

impl TaskSizeGroup {
    pub fn count(&self) -> u64 {
        self.sample_count
    }
}

/// The historical-distribution input. Size is the grouping axis; task kind is
/// retained as a filter and as refusal evidence for the older identity axis.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskKindDistribution {
    pub by_size: Vec<TaskSizeGroup>,
    pub unknown_size: u64,
    pub conflict_size: u64,
    /// Counts retained for callers that inspect the optional task-kind filter.
    pub resolved: Vec<(TaskKind, u64)>,
    pub unknown: u64,
    pub conflict: u64,
}

impl TaskKindDistribution {
    pub fn count_for(&self, kind: TaskKind) -> u64 {
        self.resolved
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map(|(_, count)| *count)
            .unwrap_or(0)
    }

    pub fn count_for_size(&self, size: TaskSize) -> u64 {
        self.by_size
            .iter()
            .find(|group| group.size == size)
            .map(TaskSizeGroup::count)
            .unwrap_or(0)
    }

    pub fn group_for_size(&self, size: TaskSize) -> Option<&TaskSizeGroup> {
        self.by_size.iter().find(|group| group.size == size)
    }

    /// The exclusion counts that keep unresolved size outside the reference
    /// distribution while leaving the kind diagnostics available alongside it.
    pub fn exclusions(&self) -> TaskDistributionExclusions {
        TaskDistributionExclusions {
            unknown_kind: self.unknown,
            conflict_kind: self.conflict,
            unknown_size: self.unknown_size,
            conflict_size: self.conflict_size,
        }
    }
}

/// Refusal evidence for both the historical grouping axis and the retained
/// task-kind filter axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskDistributionExclusions {
    pub unknown_kind: u64,
    pub conflict_kind: u64,
    pub unknown_size: u64,
    pub conflict_size: u64,
}

/// Reads the size-grouped distribution with no task-kind filter.
pub fn task_kind_distribution(
    connection: &rusqlite::Connection,
) -> Result<TaskKindDistribution, Error> {
    task_size_distribution(connection, None)
}

/// Reads the size-grouped distribution, optionally restricting the reference
/// population to one resolved task kind.
pub fn task_size_distribution(
    connection: &rusqlite::Connection,
    kind_filter: Option<TaskKind>,
) -> Result<TaskKindDistribution, Error> {
    task_size_distribution_with_filters(connection, kind_filter, None)
}

/// Reads the size-grouped distribution with independent task-kind and
/// difficulty filters. A difficulty filter selects only resolved difficulty
/// values; without it, unknown and conflicting difficulty remain in each
/// size group's mix.
pub fn task_size_distribution_with_filters(
    connection: &rusqlite::Connection,
    kind_filter: Option<TaskKind>,
    difficulty_filter: Option<TaskDifficulty>,
) -> Result<TaskKindDistribution, Error> {
    let mut statement = connection
        .prepare(
            "SELECT state, kind, size_state, size, difficulty_state, difficulty
             FROM task_identity
             WHERE (?1 IS NULL OR (state = 'resolved' AND kind = ?1))
               AND (?2 IS NULL OR (
                   difficulty_state = 'resolved' AND difficulty = ?2
               ))
             ORDER BY task_source, task_native",
        )
        .map_err(|error| Error::Store(format!("cannot read task distribution: {error}")))?;
    let kind_filter = kind_filter.map(|kind| kind.as_str().to_owned());
    let difficulty_filter = difficulty_filter.map(|difficulty| difficulty.as_str().to_owned());
    let rows = statement
        .query_map(params![kind_filter, difficulty_filter], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|error| Error::Store(format!("cannot query task distribution: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::Store(format!("cannot decode task distribution: {error}")))?;

    let mut distribution = TaskKindDistribution::default();
    let mut groups = BTreeMap::<TaskSize, TaskSizeGroup>::new();
    for (state, kind, size_state, size, difficulty_state, difficulty) in rows {
        let kind_state = TaskIdentityState::parse(&state).ok_or_else(|| {
            Error::Store(format!(
                "task distribution carries an unknown state label: {state}"
            ))
        })?;
        match kind_state {
            TaskIdentityState::Resolved => {
                let raw = kind.ok_or_else(|| {
                    Error::Store(
                        "resolved identity row carries no kind, though the database forbids it"
                            .to_owned(),
                    )
                })?;
                let kind = TaskKind::parse(&raw).ok_or_else(|| {
                    Error::Store(format!("task distribution carries an unknown kind: {raw}"))
                })?;
                increment_kind_count(&mut distribution.resolved, kind);
            }
            TaskIdentityState::Unknown => distribution.unknown += 1,
            TaskIdentityState::Conflict => distribution.conflict += 1,
        }

        let size_state = TaskIdentityState::parse(&size_state).ok_or_else(|| {
            Error::Store(format!(
                "task distribution carries an unknown size state label: {size_state}"
            ))
        })?;
        let size = parse_identity_axis_value("size", size_state, size.as_deref(), TaskSize::parse)?;
        let difficulty_state = TaskIdentityState::parse(&difficulty_state).ok_or_else(|| {
            Error::Store(format!(
                "task distribution carries an unknown difficulty state label: {difficulty_state}"
            ))
        })?;
        let difficulty = parse_identity_axis_value(
            "difficulty",
            difficulty_state,
            difficulty.as_deref(),
            TaskDifficulty::parse,
        )?;

        let Some(size) = size else {
            match size_state {
                TaskIdentityState::Unknown => distribution.unknown_size += 1,
                TaskIdentityState::Conflict => distribution.conflict_size += 1,
                TaskIdentityState::Resolved => {
                    return Err(Error::Store(
                        "resolved task size carried no parsed value".to_owned(),
                    ));
                }
            }
            continue;
        };
        let group = groups.entry(size).or_insert_with(|| TaskSizeGroup {
            size,
            sample_count: 0,
            difficulty_mix: TaskDifficultyMix::default(),
        });
        group.sample_count += 1;
        match difficulty_state {
            TaskIdentityState::Resolved => {
                let difficulty = difficulty.ok_or_else(|| {
                    Error::Store("resolved task difficulty carried no parsed value".to_owned())
                })?;
                increment_difficulty_count(&mut group.difficulty_mix.resolved, difficulty);
            }
            TaskIdentityState::Unknown => group.difficulty_mix.unknown += 1,
            TaskIdentityState::Conflict => group.difficulty_mix.conflict += 1,
        }
    }
    distribution.by_size = groups.into_values().collect();
    distribution.resolved.sort();
    for group in &mut distribution.by_size {
        group.difficulty_mix.resolved.sort();
    }
    Ok(distribution)
}

fn increment_kind_count(counts: &mut Vec<(TaskKind, u64)>, kind: TaskKind) {
    if let Some((_, count)) = counts.iter_mut().find(|(candidate, _)| *candidate == kind) {
        *count += 1;
    } else {
        counts.push((kind, 1));
    }
}

fn increment_difficulty_count(counts: &mut Vec<(TaskDifficulty, u64)>, difficulty: TaskDifficulty) {
    if let Some((_, count)) = counts
        .iter_mut()
        .find(|(candidate, _)| *candidate == difficulty)
    {
        *count += 1;
    } else {
        counts.push((difficulty, 1));
    }
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
