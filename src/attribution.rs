//! Account, project, repository, and task attribution.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

pub mod account_segment;
pub mod segment;

pub use account_segment::AccountEvidenceClass;

use std::collections::BTreeMap;

use crate::domain::ids::{NativeTaskId, SourceNamespace, TaskId};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;

/// One event read from an issue tracker before `aub` assigns it domain meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerEventRecord {
    pub upstream_id: i64,
    pub task_native: String,
    pub event_type: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub occurred_at: String,
    pub actor: Option<String>,
}

/// Read-only boundary for a tracker source. It intentionally exposes no write
/// operation: task attribution reads tracker history but never manages issues.
pub trait TrackerEventReader {
    fn read_events(&self) -> Result<Vec<TrackerEventRecord>, Error>;
}

/// The normalized kinds that establish task-attribution boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEventKind {
    Claim,
    Release,
    Unknown(String),
}

impl TaskEventKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Claim => "claim",
            Self::Release => "release",
            Self::Unknown(kind) => kind,
        }
    }
}

/// A timestamped tracker event ready for durable ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEvent {
    pub tracker_source: SourceNamespace,
    pub upstream_id: i64,
    pub task_id: TaskId,
    pub occurred_at: UtcTimestamp,
    pub kind: TaskEventKind,
    pub agent_association: Option<String>,
}

/// An upstream record that cannot safely establish an attribution boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEventQuarantine {
    pub tracker_source: SourceNamespace,
    pub upstream_id: i64,
    pub raw_timestamp: String,
    pub reason: &'static str,
}

/// The closed vocabulary of task kinds, one variant per resolvable grouping
/// key over historical tasks.
///
/// The vocabulary is grounded on what the Beads tracker actually writes in its
/// categorical identity column (verified against this repository's tracker on
/// 2026-09-02: `bug`, `docs`, `epic`, `question`, `task`); the versioned
/// [`TaskKindMapping`] normalizes tracker evidence onto it, and nothing else
/// mints a kind. There is deliberately no `Unknown` variant here: an unknown
/// task-kind state is a property of the evidence for one task, carried by
/// [`TaskIdentityState`], not a kind anything asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskKind {
    /// Ordinary unit of work.
    Task,
    /// Container over other tasks; excluded from leaf distributions downstream.
    Epic,
    /// Defect repair.
    Bug,
    /// Documentation-only work.
    Docs,
    /// Question or discussion thread.
    Question,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Epic => "epic",
            Self::Bug => "bug",
            Self::Docs => "docs",
            Self::Question => "question",
        }
    }

    /// The kind for a raw tracker value, or `None` when the value is outside
    /// the vocabulary. The only path from raw tracker text to a kind.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "task" => Some(Self::Task),
            "epic" => Some(Self::Epic),
            "bug" => Some(Self::Bug),
            "docs" => Some(Self::Docs),
            "question" => Some(Self::Question),
            _ => None,
        }
    }
}

/// Where one task-kind candidate came from, and the precedence rank that
/// decides disagreements between candidates for the same task.
///
/// Precedence is documented once, here: the tracker's own categorical identity
/// field outranks an auxiliary tracker tag, because the identity field is what
/// the tracker considers the issue's nature while a tag is an annotation.
/// Ranks are comparable only within one task, and two candidates of equal rank
/// that normalize to different kinds are a conflict, never a winner by order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskKindOrigin {
    /// A categorical column of the tracker's own issue record.
    TrackerField(String),
    /// A tag attached to the issue.
    TrackerLabel(String),
}

impl TaskKindOrigin {
    /// The precedence rank; lower wins.
    pub fn rank(&self) -> u8 {
        match self {
            Self::TrackerField(_) => 1,
            Self::TrackerLabel(_) => 2,
        }
    }

    /// The stable provenance identifier persisted with the candidate and the
    /// resolved identity. Tagged, so it parses back losslessly.
    pub fn provenance_id(&self) -> String {
        match self {
            Self::TrackerField(field) => format!("tracker_field:{field}"),
            Self::TrackerLabel(label) => format!("tracker_label:{label}"),
        }
    }

    /// Rebuilds the origin from its persisted provenance identifier, or `None`
    /// when the tag is unrecognized (a stored value from a newer format).
    pub fn from_provenance_id(raw: &str) -> Option<Self> {
        let (tag, detail) = raw.split_once(':')?;
        match tag {
            "tracker_field" => Some(Self::TrackerField(detail.to_owned())),
            "tracker_label" => Some(Self::TrackerLabel(detail.to_owned())),
            _ => None,
        }
    }
}

/// One piece of structured tracker evidence that may assert a task kind.
///
/// The record deliberately carries no free-form text: a candidate can only
/// come from a categorical tracker field or a tag, so no code path in this
/// module can guess a kind from a task title or description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskKindCandidate {
    pub task_id: TaskId,
    pub origin: TaskKindOrigin,
    /// The raw tracker value exactly as read, before normalization.
    pub raw_value: String,
}

/// One task read from a tracker before `aub` assigns kinds to it.
///
/// Fields are the tracker's structured kind evidence only. The absence of a
/// title or description field is the guarantee that candidates never derive
/// from free-form task text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerTaskRecord {
    pub native: String,
    /// The tracker's categorical identity field value, exactly as read.
    pub issue_type: String,
    pub labels: Vec<String>,
}

/// Read-only boundary for the task records a tracker exposes. It exposes no
/// write operation, matching [`TrackerEventReader`].
pub trait TrackerTaskReader {
    fn read_tasks(&self) -> Result<Vec<TrackerTaskRecord>, Error>;
}

/// Emits every candidate one tracker task carries, each with its source
/// provenance: one from the categorical identity field, one per tag.
///
/// Emitting is not resolving: a raw value becomes a kind only through a
/// [`TaskKindMapping`], at rebuild time, so changing the mapping re-evaluates
/// the same immutable candidates.
pub fn emit_task_kind_candidates(
    tracker_source: SourceNamespace,
    record: TrackerTaskRecord,
) -> Vec<TaskKindCandidate> {
    let task_id = TaskId::new(tracker_source, NativeTaskId::new(record.native));
    let mut candidates = vec![TaskKindCandidate {
        task_id: task_id.clone(),
        origin: TaskKindOrigin::TrackerField("issue_type".to_owned()),
        raw_value: record.issue_type,
    }];
    candidates.extend(record.labels.into_iter().map(|label| TaskKindCandidate {
        task_id: task_id.clone(),
        origin: TaskKindOrigin::TrackerLabel(label.clone()),
        raw_value: label,
    }));
    candidates
}

/// The versioned mapping from raw tracker values to task kinds.
///
/// One mapping applies to every candidate regardless of origin: normalization
/// is source-independent, while precedence between origins is decided by
/// [`TaskKindOrigin::rank`]. The version is persisted with every resolved
/// identity so a rebuild under a newer mapping is distinguishable from the
/// result of the older one, and re-running a rebuild under an unchanged
/// mapping is a no-op on the persisted state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskKindMapping {
    version: u32,
    entries: BTreeMap<String, TaskKind>,
}

impl TaskKindMapping {
    /// Validates and builds a mapping. Refuses version zero and any raw value
    /// that is empty or carries surrounding whitespace: a padded key would
    /// make normalization order-sensitive against the same value trimmed.
    pub fn new(
        version: u32,
        entries: impl IntoIterator<Item = (String, TaskKind)>,
    ) -> Result<Self, Error> {
        if version == 0 {
            return Err(Error::Usage(
                "task-kind mapping version must be at least 1".to_owned(),
            ));
        }
        let mut table = BTreeMap::new();
        for (raw, kind) in entries {
            if raw.is_empty() || raw.trim() != raw {
                return Err(Error::Usage(format!(
                    "task-kind mapping has an empty or padded raw value: {raw:?}"
                )));
            }
            table.insert(raw, kind);
        }
        Ok(Self {
            version,
            entries: table,
        })
    }

    /// The mapping grounded on the Beads tracker's observed vocabulary.
    /// Identity-preserving: every kind the tracker names has its own variant,
    /// so the default normalization invents nothing.
    pub fn default_v1() -> Self {
        Self {
            version: 1,
            entries: [
                ("task", TaskKind::Task),
                ("epic", TaskKind::Epic),
                ("bug", TaskKind::Bug),
                ("docs", TaskKind::Docs),
                ("question", TaskKind::Question),
            ]
            .into_iter()
            .map(|(raw, kind)| (raw.to_owned(), kind))
            .collect(),
        }
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// Normalizes one raw value. `None` means this candidate asserts no kind;
    /// the candidate is still evidence, and is never dropped.
    pub fn normalize(&self, raw: &str) -> Option<TaskKind> {
        self.entries.get(raw).copied()
    }
}

/// The task-kind state one task resolves to, with no winner chosen by input
/// order and no fallback string anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTaskKind {
    /// Every kind-asserting candidate at the winning rank agreed.
    Resolved {
        kind: TaskKind,
        /// The provenance identifier of the winning candidate.
        winner: TaskKindOrigin,
        /// Deterministic rendering of every candidate that took part in the
        /// decision, including agreeing co-candidates.
        evidence: String,
    },
    /// Candidates existed but none asserted a kind under the mapping.
    Unknown { evidence: String },
    /// Candidates at one equal rank normalized to different kinds. No winner
    /// is selected, ever: the conflict persists until the evidence or the
    /// mapping changes.
    Conflict { evidence: String },
}

/// The typed identity states a persisted identity row carries. A bare string
/// never represents a resolved identity: these variants cross module boundaries
/// (the store persists them, the report layer reads them), and the state label
/// is the only serialization they have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskIdentityState {
    /// Every kind-asserting candidate at the winning rank agreed.
    Resolved,
    /// Evidence existed but none of it asserted a kind under the mapping.
    Unknown,
    /// Equal-rank evidence disagreed; no winner was selected.
    Conflict,
}

impl TaskIdentityState {
    /// The state label persisted alongside the identity row.
    pub fn state_label(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unknown => "unknown",
            Self::Conflict => "conflict",
        }
    }

    /// Parses a persisted state label. `None` for a label this code does not
    /// know: the caller refuses rather than guesses.
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "resolved" => Some(Self::Resolved),
            "unknown" => Some(Self::Unknown),
            "conflict" => Some(Self::Conflict),
            _ => None,
        }
    }
}

impl ResolvedTaskKind {
    /// The typed state this resolution carries.
    pub fn state(&self) -> TaskIdentityState {
        match self {
            Self::Resolved { .. } => TaskIdentityState::Resolved,
            Self::Unknown { .. } => TaskIdentityState::Unknown,
            Self::Conflict { .. } => TaskIdentityState::Conflict,
        }
    }
}

/// Resolves one task's candidates into its typed task-kind identity.
///
/// The rules, documented once:
///
/// 1. Every candidate's raw value is normalized through the mapping. A
///    candidate that does not normalize is retained as evidence but asserts
///    no kind.
/// 2. The kind-asserting candidates are grouped by origin rank, and the
///    lowest rank with at least one assertion wins the round.
/// 3. One distinct kind at the winning rank resolves the task; more than one
///    is a conflict; no kind-asserting candidate at all leaves the task
///    unknown.
///
/// The result is independent of candidate order: evidence is rendered in a
/// deterministic total order (rank, provenance identifier, raw value), and no
/// step reads a position in the input.
pub fn resolve_task_kind(
    candidates: &[TaskKindCandidate],
    mapping: &TaskKindMapping,
) -> ResolvedTaskKind {
    let evidence = render_evidence(candidates);
    let mut asserted: Vec<(u8, TaskKind)> = candidates
        .iter()
        .filter_map(|candidate| {
            mapping
                .normalize(&candidate.raw_value)
                .map(|kind| (candidate.origin.rank(), kind))
        })
        .collect();
    if asserted.is_empty() {
        return ResolvedTaskKind::Unknown { evidence };
    }
    asserted.sort_by_key(|(rank, kind)| (*rank, *kind));
    let winning_rank = asserted[0].0;
    let distinct: Vec<TaskKind> = {
        let mut kinds: Vec<TaskKind> = asserted
            .iter()
            .filter(|(rank, _)| *rank == winning_rank)
            .map(|(_, kind)| *kind)
            .collect();
        kinds.dedup();
        kinds
    };
    if distinct.len() > 1 {
        return ResolvedTaskKind::Conflict { evidence };
    }
    let winner = candidates
        .iter()
        .filter(|candidate| {
            candidate.origin.rank() == winning_rank
                && mapping.normalize(&candidate.raw_value) == Some(distinct[0])
        })
        .map(|candidate| candidate.origin.clone())
        .min_by_key(|origin| origin.provenance_id())
        .expect("a winning candidate exists by construction");
    ResolvedTaskKind::Resolved {
        kind: distinct[0],
        winner,
        evidence,
    }
}

/// Renders candidate evidence deterministically: sorted by rank, provenance
/// identifier and raw value, joined with `; `. Order-independence of the
/// resolution depends on this rendering being a pure function of the set.
fn render_evidence(candidates: &[TaskKindCandidate]) -> String {
    let mut rendered: Vec<String> = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{}={}",
                candidate.origin.provenance_id(),
                candidate.raw_value
            )
        })
        .collect();
    rendered.sort();
    rendered.dedup();
    rendered.join(";")
}

/// Normalizes one tracker record without inventing a timestamp.
pub fn normalize_tracker_event(
    tracker_source: SourceNamespace,
    record: TrackerEventRecord,
) -> Result<TaskEvent, TaskEventQuarantine> {
    let occurred_at =
        UtcTimestamp::parse_rfc3339(&record.occurred_at).ok_or_else(|| TaskEventQuarantine {
            tracker_source: tracker_source.clone(),
            upstream_id: record.upstream_id,
            raw_timestamp: record.occurred_at.clone(),
            reason: "unusable timestamp",
        })?;
    let kind = match (
        record.event_type.as_str(),
        record.old_value.as_deref(),
        record.new_value.as_deref(),
    ) {
        ("status_changed", _, Some("in_progress")) => TaskEventKind::Claim,
        ("status_changed", Some("in_progress"), _) => TaskEventKind::Release,
        _ => TaskEventKind::Unknown(record.event_type),
    };
    Ok(TaskEvent {
        tracker_source: tracker_source.clone(),
        upstream_id: record.upstream_id,
        task_id: TaskId::new(tracker_source, NativeTaskId::new(record.task_native)),
        occurred_at,
        kind,
        agent_association: record.actor.filter(|actor| !actor.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn task_kind_candidate(native: &str, origin: TaskKindOrigin, raw: &str) -> TaskKindCandidate {
        TaskKindCandidate {
            task_id: TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new(native)),
            origin,
            raw_value: raw.to_owned(),
        }
    }

    /// Enumerates the resolution variants explicitly: the crate denies
    /// wildcard arms on enum matches, and each unexpected variant is its own
    /// named failure.
    fn expect_resolved(
        resolved: ResolvedTaskKind,
        what: &str,
    ) -> (TaskKind, TaskKindOrigin, String) {
        match resolved {
            ResolvedTaskKind::Resolved {
                kind,
                winner,
                evidence,
            } => (kind, winner, evidence),
            ResolvedTaskKind::Unknown { .. } => {
                panic!("expected a resolution, got unknown: {what}")
            }
            ResolvedTaskKind::Conflict { .. } => {
                panic!("expected a resolution, got a conflict: {what}")
            }
        }
    }

    fn expect_conflict(resolved: ResolvedTaskKind, what: &str) -> String {
        match resolved {
            ResolvedTaskKind::Conflict { evidence } => evidence,
            ResolvedTaskKind::Resolved { .. } => {
                panic!("expected a conflict, got a resolution: {what}")
            }
            ResolvedTaskKind::Unknown { .. } => panic!("expected a conflict, got unknown: {what}"),
        }
    }

    fn expect_unknown(resolved: ResolvedTaskKind, what: &str) -> String {
        match resolved {
            ResolvedTaskKind::Unknown { evidence } => evidence,
            ResolvedTaskKind::Resolved { .. } => {
                panic!("expected unknown, got a resolution: {what}")
            }
            ResolvedTaskKind::Conflict { .. } => panic!("expected unknown, got a conflict: {what}"),
        }
    }

    #[test]
    fn the_identity_field_outranks_a_tag_at_the_same_task() {
        let mapping = TaskKindMapping::default_v1();
        let candidates = vec![
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("testing".to_owned()),
                "testing",
            ),
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerField("issue_type".to_owned()),
                "bug",
            ),
        ];
        let resolved = resolve_task_kind(&candidates, &mapping);
        let (kind, winner, _) = expect_resolved(resolved, "the identity field outranks the tag");
        assert_eq!(kind, TaskKind::Bug);
        assert_eq!(
            winner,
            TaskKindOrigin::TrackerField("issue_type".to_owned())
        );
    }

    #[test]
    fn an_unmapped_identity_field_defers_to_a_mapped_tag() {
        let mapping = TaskKindMapping::new(
            4,
            [
                ("task", TaskKind::Task),
                ("epic", TaskKind::Epic),
                ("bug", TaskKind::Bug),
                ("docs", TaskKind::Docs),
                ("question", TaskKind::Question),
                ("spike-tag", TaskKind::Task),
            ]
            .into_iter()
            .map(|(raw, kind)| (raw.to_owned(), kind)),
        )
        .unwrap();
        let candidates = vec![
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerField("issue_type".to_owned()),
                "spike",
            ),
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("spike-tag".to_owned()),
                "spike-tag",
            ),
        ];
        let (kind, winner, _) = expect_resolved(
            resolve_task_kind(&candidates, &mapping),
            "the tag resolves what the field cannot",
        );
        assert_eq!(kind, TaskKind::Task);
        assert_eq!(winner, TaskKindOrigin::TrackerLabel("spike-tag".to_owned()));
    }

    #[test]
    fn equal_rank_disagreement_is_a_conflict_with_no_winner() {
        let mapping = TaskKindMapping::new(
            1,
            [("alpha", TaskKind::Bug), ("beta", TaskKind::Docs)]
                .into_iter()
                .map(|(raw, kind)| (raw.to_owned(), kind)),
        )
        .unwrap();
        let candidates = vec![
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("alpha".to_owned()),
                "alpha",
            ),
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("beta".to_owned()),
                "beta",
            ),
        ];
        let evidence = expect_conflict(
            resolve_task_kind(&candidates, &mapping),
            "equal-rank disagreement",
        );
        assert!(evidence.contains("tracker_label:alpha=alpha"));
        assert!(evidence.contains("tracker_label:beta=beta"));
    }

    #[test]
    fn missing_evidence_is_unknown_and_every_case_keeps_its_evidence() {
        let mapping = TaskKindMapping::default_v1();
        // No candidates at all: unknown, with no evidence.
        let evidence = expect_unknown(resolve_task_kind(&[], &mapping), "no candidates");
        assert_eq!(evidence, "");
        // Only candidates the mapping does not cover: unknown, with evidence.
        let candidates = vec![task_kind_candidate(
            "aub-1",
            TaskKindOrigin::TrackerField("issue_type".to_owned()),
            "spike",
        )];
        let evidence = expect_unknown(
            resolve_task_kind(&candidates, &mapping),
            "only candidates the mapping does not cover",
        );
        assert_eq!(evidence, "tracker_field:issue_type=spike");
    }

    #[test]
    fn resolution_is_independent_of_candidate_order() {
        let mapping = TaskKindMapping::new(
            1,
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
        let mut candidates = vec![
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerField("issue_type".to_owned()),
                "task",
            ),
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("alpha".to_owned()),
                "alpha",
            ),
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("beta".to_owned()),
                "beta",
            ),
            task_kind_candidate(
                "aub-1",
                TaskKindOrigin::TrackerLabel("docs-tag".to_owned()),
                "docs",
            ),
        ];
        let baseline = resolve_task_kind(&candidates, &mapping);
        // Deterministic rotations and the reversal cover every position of the
        // winning candidate without a random dependency.
        for _ in 1..candidates.len() {
            candidates.rotate_left(1);
            assert_eq!(resolve_task_kind(&candidates, &mapping), baseline);
        }
        candidates.reverse();
        assert_eq!(resolve_task_kind(&candidates, &mapping), baseline);
    }

    proptest::proptest! {
        #[test]
        fn prop_resolution_never_depends_on_input_order(
            specs in proptest::collection::vec((0u8..2, 0u8..3), 0..8),
        ) {
            let mapping = TaskKindMapping::new(
                1,
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
            let build = |spec: (u8, u8)| {
                let (is_field, selector) = spec;
                let (origin, raw) = match (is_field, selector) {
                    (0, 0) => (TaskKindOrigin::TrackerField("issue_type".to_owned()), "task"),
                    (0, 1) => (TaskKindOrigin::TrackerField("issue_type".to_owned()), "bug"),
                    (0, _) => (TaskKindOrigin::TrackerField("issue_type".to_owned()), "spike"),
                    (1, 0) => (TaskKindOrigin::TrackerLabel("label-0".to_owned()), "alpha"),
                    (1, 1) => (TaskKindOrigin::TrackerLabel("label-1".to_owned()), "beta"),
                    (_, _) => (TaskKindOrigin::TrackerLabel("label-2".to_owned()), "spike"),
                };
                task_kind_candidate("aub-1", origin, raw)
            };
            let forward: Vec<TaskKindCandidate> =
                specs.iter().map(|spec| build(*spec)).collect();
            let mut reversed = forward.clone();
            reversed.reverse();
            prop_assert_eq!(
                resolve_task_kind(&forward, &mapping),
                resolve_task_kind(&reversed, &mapping),
            );
            for rotation in 1..forward.len().max(1) {
                let mut rotated = forward.clone();
                rotated.rotate_left(rotation);
                prop_assert_eq!(
                    resolve_task_kind(&forward, &mapping),
                    resolve_task_kind(&rotated, &mapping),
                );
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_a_generated_conflict_never_resolves_by_input_order(
            flags in proptest::collection::vec(proptest::bool::ANY, 1..8),
        ) {
            let mapping = TaskKindMapping::new(
                1,
                [("alpha", TaskKind::Bug), ("beta", TaskKind::Docs)]
                    .into_iter()
                    .map(|(raw, kind)| (raw.to_owned(), kind)),
            )
            .unwrap();
            // Every candidate is an equal-rank tag whose kind is one of two;
            // any multiset containing both is a conflict, and the resolution
            // must not change when the same multiset is reversed.
            let candidates: Vec<TaskKindCandidate> = flags
                .iter()
                .map(|flag| {
                    let (origin, raw) = if *flag {
                        (TaskKindOrigin::TrackerLabel("alpha".to_owned()), "alpha")
                    } else {
                        (TaskKindOrigin::TrackerLabel("beta".to_owned()), "beta")
                    };
                    task_kind_candidate("aub-1", origin, raw)
                })
                .collect();
            let mut reversed = candidates.clone();
            reversed.reverse();
            let forward = resolve_task_kind(&candidates, &mapping);
            prop_assert_eq!(forward, resolve_task_kind(&reversed, &mapping));
        }
    }

    #[test]
    fn mapping_validation_refuses_version_zero_and_padded_raw_values() {
        let zero = TaskKindMapping::new(0, []);
        assert!(zero.is_err());
        let padded = TaskKindMapping::new(1, [(" task".to_owned(), TaskKind::Task)]);
        assert!(padded.is_err());
        let empty = TaskKindMapping::new(1, [(String::new(), TaskKind::Task)]);
        assert!(empty.is_err());
        let valid = TaskKindMapping::new(1, [("task".to_owned(), TaskKind::Task)]);
        assert!(valid.is_ok());
    }

    #[test]
    fn normalization_is_deterministic_and_idempotent() {
        let mapping = TaskKindMapping::default_v1();
        for raw in ["task", "epic", "bug", "docs", "question", "spike", ""] {
            // Idempotence here is that repeated lookups over the same raw
            // value agree, and unmapped values stay unmapped.
            let once = mapping.normalize(raw);
            let twice = mapping.normalize(raw);
            assert_eq!(once, twice);
            if raw == "spike" || raw.is_empty() {
                assert_eq!(once, None);
            }
        }
        // The same raw value normalizes to the same kind from any origin.
        let field = task_kind_candidate(
            "aub-1",
            TaskKindOrigin::TrackerField("issue_type".to_owned()),
            "bug",
        );
        let label = task_kind_candidate(
            "aub-1",
            TaskKindOrigin::TrackerLabel("bug".to_owned()),
            "bug",
        );
        assert_eq!(
            mapping.normalize(&field.raw_value),
            mapping.normalize(&label.raw_value)
        );
    }

    #[test]
    fn provenance_ids_round_trip_losslessly() {
        let field = TaskKindOrigin::TrackerField("issue_type".to_owned());
        let label = TaskKindOrigin::TrackerLabel("testing".to_owned());
        for origin in [field, label] {
            let id = origin.provenance_id();
            assert_eq!(TaskKindOrigin::from_provenance_id(&id), Some(origin));
        }
        assert_eq!(TaskKindOrigin::from_provenance_id("freeform:thing"), None);
    }

    #[test]
    fn unknown_tracker_kinds_are_retained() {
        let event = normalize_tracker_event(
            SourceNamespace::new("beads-a"),
            TrackerEventRecord {
                upstream_id: 1,
                task_native: "aub-1".into(),
                event_type: "commented".into(),
                old_value: None,
                new_value: None,
                occurred_at: "2026-08-31T19:11:34.47746272Z".into(),
                actor: None,
            },
        )
        .unwrap();

        assert_eq!(event.kind, TaskEventKind::Unknown("commented".into()));
    }

    #[test]
    fn an_unusable_timestamp_is_quarantined_not_defaulted() {
        let quarantine = normalize_tracker_event(
            SourceNamespace::new("beads-a"),
            TrackerEventRecord {
                upstream_id: 2,
                task_native: "aub-1".into(),
                event_type: "status_changed".into(),
                old_value: Some("open".into()),
                new_value: Some("in_progress".into()),
                occurred_at: "not a timestamp".into(),
                actor: None,
            },
        )
        .unwrap_err();

        assert_eq!(quarantine.reason, "unusable timestamp");
        assert_eq!(quarantine.raw_timestamp, "not a timestamp");
    }
}
