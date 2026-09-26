//! Assembles can-run's historical task-history samples (`aub-cab.4`) from the
//! completed tasks the store already holds, reusing the task-report machinery
//! rather than a second segmentation implementation.
//!
//! # The three eligibility facts, and where each one comes from
//!
//! [`TaskHistorySample`] needs three facts per completed task, and this module's
//! whole job is producing them without inventing a second source of truth:
//!
//! - `pricing`: [`crate::cost_model::convert`] over the task's own attributed
//!   usage vector, exactly as [`crate::report::task::assemble_task_report`]
//!   prices a single task. [`TaskPricing::UnknownTokenComponents`] whenever
//!   that conversion refuses, for any reason (no active cost model, an unknown
//!   component, or a missing term): all three are "no defensible price",
//!   which is the one distinction this eligibility fact makes.
//! - `account_evidence`: the worst (least confident)
//!   [`AccountEvidenceClass`] `crate::attribution::account_segment::assign`
//!   assigns any of the task's own attributed events, across every session
//!   the task touched. One `Unattributed` event anywhere in the task makes the
//!   whole task `Unattributed`, matching this module's brief: "taking
//!   `Unattributed` when any of the task's sessions is unattributed."
//! - `segmentation_complete`: false when any of the task's own sessions also
//!   carries usage the segmentation engine could not attribute to any task
//!   for [`OverheadReason::AmbiguousBoundary`] specifically, the one overhead
//!   reason that means "a usage window straddled a claim/release boundary
//!   with no principled way to split it" rather than "usage outside any
//!   task's scope" (`BeforeFirstClaim`, `UnmappedSession`, and so on, which do
//!   not cast doubt on a *task's own* boundary and are not checked here).
//!
//! # What "completed" means here
//!
//! The task-event table retains only normalized `claim`/`release` boundaries
//! (`crate::attribution::normalize_tracker_event` discards the tracker's own
//! status string on purpose, so an implementation upgrade to the tracker's
//! vocabulary cannot silently change attribution). Nothing durable records
//! which release was a real close versus a rework or a batch-pending step, so
//! "completed" is defined here as "has recorded a release boundary in the
//! selection period" rather than as a specific terminal status: it is the
//! only completion signal this store retains, stated once so it never reads
//! as an unstated implementation accident.
//!
//! # The selection period
//!
//! No configuration key bounds the historical window this bead reads from
//! (verified: no caller anywhere constructs a
//! [`SelectionPeriod`](crate::advice::historical_distribution::SelectionPeriod)
//! outside that module's own tests). Rather than invent an unconfigured
//! duration constant, [`gather_task_history_group_report`] takes the period
//! from its caller, and `aub can-run`'s own decision is the full known
//! history: from the epoch to the report's `generated_at`. A narrower
//! configured window is a later, separate decision.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::advice::historical_distribution::{
    AttributionCoverage, DistributionVerdict, ExclusionCounts, GroupHistoryReport,
    HistoricalDistributionConfig, SelectionPeriod, TaskHistorySample, TaskPricing,
    build_group_reports,
};
use crate::attribution::TaskEventKind;
use crate::attribution::account_segment::{
    AccountEvidenceClass, AccountSegmentationInputs, AccountUsageEvent, assign,
};
use crate::attribution::segment::{OverheadReason, SegmentTarget};
use crate::attribution::{TaskSize, TaskSpec, TaskVerify};
use crate::config::ModelTable;
use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace, TaskId};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::evidence::Derivation;
use crate::store::spend::CanonicalSpendEvent;
use crate::store::task_identity::TaskIdentityRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskRoutingBreadth {
    SmallMedium,
    Large,
}

impl TaskRoutingBreadth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SmallMedium => "sm",
            Self::Large => "l",
        }
    }
}

/// Dispatcher axes before the critical override collapses them into one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskRoutingCell {
    pub breadth: TaskRoutingBreadth,
    pub verify: TaskVerify,
    pub spec: TaskSpec,
    pub critical: bool,
}

impl Default for TaskRoutingCell {
    fn default() -> Self {
        Self {
            breadth: TaskRoutingBreadth::Large,
            verify: TaskVerify::Gate,
            spec: TaskSpec::Open,
            critical: false,
        }
    }
}

impl TaskRoutingCell {
    pub fn from_identity(identity: &TaskIdentityRow) -> Self {
        let breadth = match identity.size {
            Some(TaskSize::S | TaskSize::M) => TaskRoutingBreadth::SmallMedium,
            Some(TaskSize::L | TaskSize::XL) | None => TaskRoutingBreadth::Large,
        };
        // The dispatcher treats an incomplete verify/spec pair as gate/open.
        let (verify, spec) = match (identity.verify, identity.spec) {
            (Some(verify), Some(spec)) => (verify, spec),
            _ => (TaskVerify::Gate, TaskSpec::Open),
        };
        Self {
            breadth,
            verify,
            spec,
            critical: identity.routing_critical,
        }
    }

    pub fn group(self) -> TaskHistoryGroup {
        if self.critical {
            TaskHistoryGroup::Critical
        } else {
            TaskHistoryGroup::Cell {
                breadth: self.breadth,
                verify: self.verify,
                spec: self.spec,
            }
        }
    }

    fn parent(self) -> TaskHistoryGroup {
        TaskHistoryGroup::BreadthCritical {
            breadth: self.breadth,
            critical: self.critical,
        }
    }
}

/// The selected historical population. Its variant also names the fallback level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskHistoryGroup {
    Cell {
        breadth: TaskRoutingBreadth,
        verify: TaskVerify,
        spec: TaskSpec,
    },
    Critical,
    BreadthCritical {
        breadth: TaskRoutingBreadth,
        critical: bool,
    },
    AllTasks,
}

impl TaskHistoryGroup {
    pub fn level(self) -> &'static str {
        match self {
            Self::Cell { .. } | Self::Critical => "cell",
            Self::BreadthCritical { .. } => "breadth_critical",
            Self::AllTasks => "all_tasks",
        }
    }

    fn contains(self, cell: TaskRoutingCell) -> bool {
        match self {
            Self::Cell { .. } | Self::Critical => cell.group() == self,
            Self::BreadthCritical { breadth, critical } => {
                cell.breadth == breadth && cell.critical == critical
            }
            Self::AllTasks => true,
        }
    }
}

impl std::fmt::Display for TaskHistoryGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cell {
                breadth,
                verify,
                spec,
            } => write!(
                f,
                "{}/{}/{} critical=false",
                breadth.as_str(),
                verify.as_str(),
                spec.as_str()
            ),
            Self::Critical => f.write_str("critical=true"),
            Self::BreadthCritical { breadth, critical } => {
                write!(f, "{} critical={critical}", breadth.as_str())
            }
            Self::AllTasks => f.write_str("all tasks"),
        }
    }
}

/// Selects the first routing population with enough eligible completed tasks.
pub fn gather_task_history_group_report(
    conn: &rusqlite::Connection,
    cell: TaskRoutingCell,
    period: SelectionPeriod,
    generated_at: UtcTimestamp,
    config: &HistoricalDistributionConfig,
    models: &ModelTable,
) -> Result<GroupHistoryReport<TaskHistoryGroup>, Error> {
    let samples = gather_task_history_samples(conn, period, generated_at, models)?;
    Ok(select_task_history_group(&samples, cell, period, config))
}

fn task_cell_reports(
    samples: &[TaskHistorySample<TaskRoutingCell>],
    period: SelectionPeriod,
    config: &HistoricalDistributionConfig,
) -> BTreeMap<TaskHistoryGroup, GroupHistoryReport<TaskHistoryGroup>> {
    build_group_reports(
        samples
            .iter()
            .map(|sample| task_sample_in_group(sample, sample.group.group())),
        period,
        config,
    )
}

fn task_sample_in_group(
    sample: &TaskHistorySample<TaskRoutingCell>,
    group: TaskHistoryGroup,
) -> TaskHistorySample<TaskHistoryGroup> {
    TaskHistorySample {
        group,
        pricing: sample.pricing.clone(),
        account_evidence: sample.account_evidence,
        segmentation_complete: sample.segmentation_complete,
    }
}

fn select_task_history_group(
    samples: &[TaskHistorySample<TaskRoutingCell>],
    cell: TaskRoutingCell,
    period: SelectionPeriod,
    config: &HistoricalDistributionConfig,
) -> GroupHistoryReport<TaskHistoryGroup> {
    let mut reports = task_cell_reports(samples, period, config);
    if let Some(report) = reports.remove(&cell.group())
        && report.sample_count >= config.min_samples
    {
        return report;
    }
    for group in [cell.parent(), TaskHistoryGroup::AllTasks] {
        let matching = samples
            .iter()
            .filter(|sample| group.contains(sample.group))
            .map(|sample| task_sample_in_group(sample, group));
        let report = build_group_reports(matching, period, config)
            .remove(&group)
            .unwrap_or_else(|| GroupHistoryReport {
                group,
                period,
                sample_count: 0,
                exclusions: ExclusionCounts::default(),
                attribution: AttributionCoverage {
                    fraction: crate::attribution::quality::AttributionFraction::new(0, 0),
                    floor: config.attribution_floor,
                },
                verdict: DistributionVerdict::InsufficientEvidence {
                    min_samples: config.min_samples,
                },
            });
        if report.sample_count >= config.min_samples || group == TaskHistoryGroup::AllTasks {
            return report;
        }
    }
    unreachable!("the all-tasks level always returns a report")
}

/// Builds one [`TaskHistorySample`] per completed task whose
/// release boundary falls in `period`. A task with no attributed usage at all
/// contributes no sample: there is nothing to price or classify.
fn gather_task_history_samples(
    conn: &rusqlite::Connection,
    period: SelectionPeriod,
    generated_at: UtcTimestamp,
    models: &ModelTable,
) -> Result<Vec<TaskHistorySample<TaskRoutingCell>>, Error> {
    let events = crate::report::task::all_canonical_events(conn, models)?;
    let diagnostics = crate::store::spend::diagnostics(conn)?;
    let partial = !diagnostics.quarantined_by_class.is_empty();
    let attributed = crate::report::task::attribute_all(conn, &events)?;
    let boundaries = crate::store::task_event::read_boundaries(conn)?.boundaries;
    let cost_model = crate::store::cost_model::load_active_at(conn, generated_at)?;

    let mut completed: BTreeSet<TaskIdWrapper> = BTreeSet::new();
    for boundary in &boundaries {
        if boundary.kind == TaskEventKind::Release
            && boundary.occurred_at.unix_nanos() >= period.start.unix_nanos()
            && boundary.occurred_at.unix_nanos() < period.end.unix_nanos()
        {
            completed.insert(TaskIdWrapper(boundary.task_id.clone()));
        }
    }

    let sessions_with_ambiguous_boundary: BTreeSet<String> = events
        .iter()
        .filter(|event| {
            matches!(
                attributed.get(&event.canonical_id),
                Some(SegmentTarget::Overhead(OverheadReason::AmbiguousBoundary))
            )
        })
        .map(|event| event.session.clone())
        .collect();

    // One markers-for-session lookup per distinct session, not per event: the
    // account-evidence classification below is the same query
    // `crate::report::spend`'s own attribution pass makes, cached here across
    // however many of a task's events land in the same session.
    let mut markers_by_session: HashMap<
        (String, String),
        Vec<crate::attribution::account_segment::AccountMarkerBoundary>,
    > = HashMap::new();

    let mut samples = Vec::new();
    for TaskIdWrapper(task_id) in &completed {
        let identity = crate::store::task_identity::read_task_identity(conn, task_id)?;
        let cell = identity
            .as_ref()
            .map(TaskRoutingCell::from_identity)
            .unwrap_or_default();

        let task_events: Vec<&CanonicalSpendEvent> = events
            .iter()
            .filter(|event| {
                matches!(
                    attributed.get(&event.canonical_id),
                    Some(SegmentTarget::Task(id)) if id == task_id
                )
            })
            .collect();
        if task_events.is_empty() {
            continue;
        }

        let usage = crate::report::spend::canonical_usage(&task_events, partial);
        let pricing = match &cost_model {
            Some(model) => match crate::cost_model::convert(model, &usage) {
                Derivation::Available(qualified) => {
                    let (credits, _coverage, quality, _provenance) = qualified.into_parts();
                    TaskPricing::Priced { credits, quality }
                }
                Derivation::Unavailable { .. } => TaskPricing::UnknownTokenComponents,
            },
            None => TaskPricing::UnknownTokenComponents,
        };

        let task_sessions: BTreeSet<String> = task_events
            .iter()
            .map(|event| event.session.clone())
            .collect();
        let segmentation_complete = task_sessions.is_disjoint(&sessions_with_ambiguous_boundary);

        let account_evidence =
            task_account_evidence_class(conn, &task_events, &mut markers_by_session)?;

        samples.push(TaskHistorySample {
            group: cell,
            pricing,
            account_evidence,
            segmentation_complete,
        });
    }

    Ok(samples)
}

/// The worst (least confident) [`AccountEvidenceClass`] any of this task's own
/// attributed events resolves to, across every session it touched. An event
/// with no session identity at all resolves to [`AccountEvidenceClass::Unattributed`]
/// directly: there is no marker timeline to consult. A Codex subagent session
/// with no markers of its own resolves through its governing marker timeline
/// (`aub-wvrw`), so one subagent event inside a claimed task no longer makes
/// the whole task `Unattributed`.
fn task_account_evidence_class(
    conn: &rusqlite::Connection,
    task_events: &[&CanonicalSpendEvent],
    markers_by_session: &mut HashMap<
        (String, String),
        Vec<crate::attribution::account_segment::AccountMarkerBoundary>,
    >,
) -> Result<AccountEvidenceClass, Error> {
    let mut worst = AccountEvidenceClass::ExplicitLauncherOrHook;
    for event in task_events {
        let class = match (&event.session_source, &event.session_native) {
            (Some(source), Some(native)) => {
                let key = (source.clone(), native.clone());
                if !markers_by_session.contains_key(&key) {
                    let session_id = SessionId::new(
                        SourceNamespace::new(source.clone()),
                        NativeSessionId::new(native.clone()),
                    );
                    let (markers, _) =
                        crate::store::session_account_marker::governing_marker_timeline(
                            conn,
                            &session_id,
                        )?;
                    markers_by_session.insert(
                        key.clone(),
                        markers.iter().map(|marker| marker.boundary()).collect(),
                    );
                }
                let boundaries = markers_by_session.get(&key).expect("just inserted above");
                let assigned = assign(&AccountSegmentationInputs {
                    markers: boundaries.clone(),
                    usage: vec![AccountUsageEvent {
                        occurred_at: event.occurred_at,
                        usage: crate::report::task::known_vector(event),
                    }],
                });
                assigned
                    .first()
                    .map(|(_, class)| *class)
                    .unwrap_or(AccountEvidenceClass::Unattributed)
            }
            _ => AccountEvidenceClass::Unattributed,
        };
        if class > worst {
            worst = class;
        }
    }
    Ok(worst)
}

/// Orders [`TaskId`] by its two string components, since `TaskId` itself
/// carries no `Ord`: this module needs a deterministic completed-task
/// enumeration order for reproducible sample lists, never for correctness of
/// the aggregate statistics themselves (`build_group_reports` sorts its own
/// included credits before computing quantiles).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskIdWrapper(TaskId);

impl PartialOrd for TaskIdWrapper {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskIdWrapper {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.0.source().as_str(), self.0.native().as_str())
            .cmp(&(other.0.source().as_str(), other.0.native().as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{TaskKind, TaskKindMapping, TrackerTaskReader, TrackerTaskRecord};
    use crate::attribution::{TrackerEventReader, TrackerEventRecord};
    use crate::domain::ids::SourceNamespace;
    use crate::domain::time::{MonotonicDuration, UtcDate};
    use crate::sessions::{ProjectKey, RepositoryKey};
    use crate::store::connection::PragmaPolicy;
    use crate::store::session::{NewSession, insert_session};
    use crate::store::usage_component::insert_components;
    use crate::store::usage_event::{NewUsageEvent, insert_event};
    use crate::store::usage_occurrence::{NewUsageOccurrence, insert_occurrence};
    use crate::transcripts::ParserVersion;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aub-can-run-evidence-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_test_ledger(tag: &str) -> rusqlite::Connection {
        let root = scratch(tag);

        crate::store::test_schema::open_migrated(
            &root.join("ledger.db"),
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(100),
            },
        )
    }

    fn seed_session(conn: &rusqlite::Connection, name: &str) {
        insert_session(
            conn,
            &NewSession {
                source: SourceNamespace::new("fixture"),
                native_session_id: NativeSessionId::new(name),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: ProjectKey::new("project-a"),
                repository_key: RepositoryKey::new("repository-a"),
                working_directory: None,
                parent_native_session_id: None,
                run_id: None,
            },
        )
        .unwrap();
    }

    fn seed_session_with_parent(conn: &rusqlite::Connection, name: &str, parent: &str) {
        insert_session(
            conn,
            &NewSession {
                source: SourceNamespace::new("fixture"),
                native_session_id: NativeSessionId::new(name),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: ProjectKey::new("project-a"),
                repository_key: RepositoryKey::new("repository-a"),
                working_directory: None,
                parent_native_session_id: Some(NativeSessionId::new(parent)),
                run_id: None,
            },
        )
        .unwrap();
    }

    fn seed_marker(conn: &rusqlite::Connection, native: &str, account: &str, observed_nanos: i64) {
        crate::store::session_account_marker::insert_marker(
            conn,
            &crate::store::session_account_marker::NewSessionAccountMarker {
                session_id: crate::domain::ids::SessionId::new(
                    SourceNamespace::new("fixture"),
                    NativeSessionId::new(native),
                ),
                observed_at: UtcTimestamp::from_unix_nanos(observed_nanos),
                source_ordering_key: None,
                logical_account: account.to_owned(),
                resolved_account_id: None,
                marker_source: crate::store::session_account_marker::MarkerSource::new("hook"),
                run_id: None,
                evidence_designation:
                    crate::store::session_account_marker::EvidenceDesignation::ExplicitLauncherOrHook,
            },
        )
        .unwrap();
    }

    fn seed_canonical(
        conn: &rusqlite::Connection,
        id: &str,
        timestamp: i64,
        session: &str,
        components: &[(&str, u64)],
    ) {
        let event = insert_event(
            conn,
            &NewUsageEvent {
                canonical_event_id: id,
                session_id: Some(session),
                event_timestamp: Some(UtcTimestamp::from_unix_nanos(timestamp)),
                model_id: None,
                evidence_kind: "reported",
                source_provenance: "fixture.jsonl",
                parser_version: "fixture-v1",
                created_at: UtcTimestamp::from_unix_nanos(timestamp),
            },
        )
        .unwrap();
        insert_components(conn, event, components).unwrap();
        let namespace = SourceNamespace::new("fixture");
        let version = ParserVersion::new("fixture-v1");
        insert_occurrence(
            conn,
            &NewUsageOccurrence {
                source_namespace: &namespace,
                native_event_id: Some(id),
                parser_version: &version,
                heuristic_key: None,
                source_file: "fixture.jsonl",
                occurred_at_nanos: Some(timestamp),
                event_id: Some(event),
                transcript_file_id: None,
                source_location: None,
                canonical_fingerprint: None,
                identity_strength: None,
                heuristic_algorithm_version: None,
                canonical_payload_digest: None,
            },
        )
        .unwrap();
    }

    struct FixtureReader(Vec<TrackerEventRecord>);
    impl TrackerEventReader for FixtureReader {
        fn read_events(&self) -> Result<Vec<TrackerEventRecord>, Error> {
            Ok(self.0.clone())
        }
    }

    fn tracker_event(
        upstream_id: i64,
        task_native: &str,
        old: Option<&str>,
        new: Option<&str>,
        at: &str,
    ) -> TrackerEventRecord {
        TrackerEventRecord {
            upstream_id,
            task_native: task_native.to_string(),
            event_type: "status_changed".to_string(),
            old_value: old.map(str::to_string),
            new_value: new.map(str::to_string),
            occurred_at: at.to_string(),
            actor: None,
        }
    }

    fn seed_resolved_kind(conn: &rusqlite::Connection, task_native: &str, kind: &str) {
        crate::store::task_identity::insert_identity(
            conn,
            "beads-a",
            task_native,
            crate::attribution::ResolvedTaskKind::Resolved {
                kind: TaskKind::parse(kind).expect("test-fixture kind is a valid TaskKind"),
                winner: crate::attribution::TaskKindOrigin::TrackerField("kind".to_string()),
                evidence: "{}".to_string(),
            },
            1,
        )
        .unwrap();
    }

    fn day_nanos(date: &str) -> i64 {
        UtcDate::parse(date).unwrap().start().unix_nanos()
    }

    struct RoutingTaskReader(Vec<TrackerTaskRecord>);

    impl TrackerTaskReader for RoutingTaskReader {
        fn read_tasks(&self) -> Result<Vec<TrackerTaskRecord>, Error> {
            Ok(self.0.clone())
        }
    }

    fn seed_routing_identity(
        conn: &rusqlite::Connection,
        native: &str,
        kind: &str,
        labels: &[&str],
    ) -> TaskIdentityRow {
        crate::store::task_identity::ingest_task_kind_candidates(
            conn,
            SourceNamespace::new("beads-a"),
            &RoutingTaskReader(vec![TrackerTaskRecord {
                native: native.to_owned(),
                issue_type: kind.to_owned(),
                labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            }]),
        )
        .unwrap();
        crate::store::task_identity::rebuild_task_identities(conn, &TaskKindMapping::default_v1())
            .unwrap();
        crate::store::task_identity::read_task_identity(
            conn,
            &TaskId::new(
                SourceNamespace::new("beads-a"),
                crate::domain::ids::NativeTaskId::new(native),
            ),
        )
        .unwrap()
        .unwrap()
    }

    fn routing_history_period() -> SelectionPeriod {
        SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        }
    }

    fn routing_history_config() -> HistoricalDistributionConfig {
        HistoricalDistributionConfig {
            central_low: crate::advice::historical_distribution::Percentile::new(25).unwrap(),
            central_high: crate::advice::historical_distribution::Percentile::new(75).unwrap(),
            upper: crate::advice::historical_distribution::Percentile::new(90).unwrap(),
            min_samples: 12,
            quantile_method: crate::advice::historical_distribution::QuantileMethod::NearestRank,
            attribution_floor: crate::attribution::quality::AttributionQualityFloor::new(0.80)
                .unwrap(),
        }
    }

    fn routing_history_sample(cell: TaskRoutingCell) -> TaskHistorySample<TaskRoutingCell> {
        TaskHistorySample {
            group: cell,
            pricing: TaskPricing::Priced {
                credits: crate::domain::credits::Credits::from_micros(1000),
                quality: crate::evidence::EvidenceQuality::Measured,
            },
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        }
    }

    #[test]
    fn routing_cells_preserve_four_populations_and_exact_counts() {
        let mut conn = open_test_ledger("four-routing-cells");
        let populations: [(&[&str], usize, TaskHistoryGroup); 4] = [
            (
                &["size:S", "verify:local", "spec:closed"],
                1,
                TaskHistoryGroup::Cell {
                    breadth: TaskRoutingBreadth::SmallMedium,
                    verify: TaskVerify::Local,
                    spec: TaskSpec::Closed,
                },
            ),
            (
                &["size:M", "verify:external", "spec:open"],
                2,
                TaskHistoryGroup::Cell {
                    breadth: TaskRoutingBreadth::SmallMedium,
                    verify: TaskVerify::External,
                    spec: TaskSpec::Open,
                },
            ),
            (
                &["size:XL", "verify:local", "spec:open"],
                3,
                TaskHistoryGroup::Cell {
                    breadth: TaskRoutingBreadth::Large,
                    verify: TaskVerify::Local,
                    spec: TaskSpec::Open,
                },
            ),
            (
                &[],
                4,
                TaskHistoryGroup::Cell {
                    breadth: TaskRoutingBreadth::Large,
                    verify: TaskVerify::Gate,
                    spec: TaskSpec::Open,
                },
            ),
        ];
        let mut index = 0;
        for (labels, count, _) in &populations {
            for _ in 0..*count {
                let native = format!("routing-{index}");
                seed_session(&conn, &native);
                seed_marker(&conn, &native, "work", 0);
                let at =
                    UtcTimestamp::parse_rfc3339(&format!("2026-08-25T{index:02}:30:00Z")).unwrap();
                seed_canonical(&conn, &native, at.unix_nanos(), &native, &[("input", 1000)]);
                crate::store::task_event::ingest(
                    &conn,
                    SourceNamespace::new("beads-a"),
                    &FixtureReader(vec![
                        tracker_event(
                            index * 2 + 1,
                            &native,
                            Some("open"),
                            Some("in_progress"),
                            &format!("2026-08-25T{index:02}:00:00Z"),
                        ),
                        tracker_event(
                            index * 2 + 2,
                            &native,
                            Some("in_progress"),
                            Some("closed"),
                            &format!("2026-08-25T{index:02}:50:00Z"),
                        ),
                    ]),
                )
                .unwrap();
                seed_routing_identity(
                    &conn,
                    &native,
                    if index % 2 == 0 { "task" } else { "bug" },
                    labels,
                );
                index += 1;
            }
        }
        crate::store::cost_model::seed_initial_cost_model(
            &mut conn,
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();
        let period = routing_history_period();
        let samples =
            gather_task_history_samples(&conn, period, period.end, &ModelTable::default()).unwrap();
        let reports = task_cell_reports(&samples, period, &routing_history_config());
        assert_eq!(reports.len(), 4, "one group per routing cell");
        for (_, count, key) in populations {
            assert_eq!(reports[&key].sample_count, count, "{key}");
            assert_eq!(reports[&key].exclusions.total(), 0);
        }
    }

    #[test]
    fn routing_cell_derivation_defaults_missing_and_invalid_axes() {
        let conn = open_test_ledger("routing-defaults");
        let cases: &[(&[&str], TaskRoutingBreadth, TaskVerify, TaskSpec)] = &[
            (
                &[],
                TaskRoutingBreadth::Large,
                TaskVerify::Gate,
                TaskSpec::Open,
            ),
            (
                &["verify:local", "spec:closed"],
                TaskRoutingBreadth::Large,
                TaskVerify::Local,
                TaskSpec::Closed,
            ),
            (
                &["size:small", "verify:local", "spec:closed"],
                TaskRoutingBreadth::Large,
                TaskVerify::Local,
                TaskSpec::Closed,
            ),
            (
                &["size:S", "spec:closed"],
                TaskRoutingBreadth::SmallMedium,
                TaskVerify::Gate,
                TaskSpec::Open,
            ),
            (
                &["size:M", "verify:bad", "spec:closed"],
                TaskRoutingBreadth::SmallMedium,
                TaskVerify::Gate,
                TaskSpec::Open,
            ),
            (
                &["size:L", "verify:local"],
                TaskRoutingBreadth::Large,
                TaskVerify::Gate,
                TaskSpec::Open,
            ),
            (
                &["size:XL", "verify:local", "spec:bad"],
                TaskRoutingBreadth::Large,
                TaskVerify::Gate,
                TaskSpec::Open,
            ),
        ];
        for (index, (labels, breadth, verify, spec)) in cases.iter().enumerate() {
            let identity =
                seed_routing_identity(&conn, &format!("defaults-{index}"), "task", labels);
            assert_eq!(
                TaskRoutingCell::from_identity(&identity),
                TaskRoutingCell {
                    breadth: *breadth,
                    verify: *verify,
                    spec: *spec,
                    critical: false,
                },
                "{labels:?}"
            );
        }
    }

    #[test]
    fn both_critical_labels_override_every_other_routing_axis() {
        let conn = open_test_ledger("critical-routing");
        let labels: &[&[&str]] = &[
            &[
                "size:S",
                "verify:local",
                "spec:closed",
                "difficulty:critical",
            ],
            &["size:XL", "verify:external", "spec:open", "critical"],
            &["size:M", "difficulty:mechanical", "difficulty:critical"],
        ];
        let mut samples = Vec::new();
        for (index, labels) in labels.iter().enumerate() {
            let identity =
                seed_routing_identity(&conn, &format!("critical-{index}"), "bug", labels);
            let cell = TaskRoutingCell::from_identity(&identity);
            assert_eq!(cell.group(), TaskHistoryGroup::Critical, "{labels:?}");
            samples.push(routing_history_sample(cell));
        }
        let reports = task_cell_reports(
            &samples,
            routing_history_period(),
            &routing_history_config(),
        );
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[&TaskHistoryGroup::Critical].sample_count, 3);
        let noncritical = seed_routing_identity(
            &conn,
            "not-critical",
            "task",
            &["Critical", "difficulty:Critical"],
        );
        assert!(!TaskRoutingCell::from_identity(&noncritical).critical);
    }

    #[test]
    fn routing_history_ladder_uses_eligible_counts_at_eleven_and_twelve() {
        let cell = TaskRoutingCell {
            breadth: TaskRoutingBreadth::SmallMedium,
            verify: TaskVerify::Local,
            spec: TaskSpec::Closed,
            critical: false,
        };
        let sibling = TaskRoutingCell {
            verify: TaskVerify::External,
            ..cell
        };
        let period = routing_history_period();
        let config = routing_history_config();
        assert_eq!(config.min_samples, 12);
        let mut samples = vec![routing_history_sample(cell); 11];
        samples.push(routing_history_sample(sibling));
        let parent = select_task_history_group(&samples, cell, period, &config);
        assert_eq!(
            parent.group,
            TaskHistoryGroup::BreadthCritical {
                breadth: TaskRoutingBreadth::SmallMedium,
                critical: false
            }
        );
        assert_eq!(parent.group.level(), "breadth_critical");
        assert_eq!(parent.sample_count, 12);
        assert!(matches!(
            parent.verdict,
            DistributionVerdict::Distribution { .. }
        ));

        let mut excluded = routing_history_sample(cell);
        excluded.account_evidence = AccountEvidenceClass::Unattributed;
        samples.push(excluded);
        let still_parent = select_task_history_group(&samples, cell, period, &config);
        assert_eq!(
            still_parent.group, parent.group,
            "an ineligible twelfth cell sample cannot stop fallback"
        );
        assert_eq!(still_parent.exclusions.unknown_account_attribution, 1);
        assert!(
            still_parent.attribution.fraction.ppm() < parent.attribution.fraction.ppm(),
            "an excluded sample lowers the group's attributed fraction"
        );

        samples.push(routing_history_sample(cell));
        let full = select_task_history_group(&samples, cell, period, &config);
        assert_eq!(full.group, cell.group());
        assert_eq!(full.group.level(), "cell");
        assert_eq!(full.sample_count, 12);

        let mut all_samples = vec![routing_history_sample(cell); 11];
        all_samples.push(routing_history_sample(TaskRoutingCell {
            breadth: TaskRoutingBreadth::Large,
            ..cell
        }));
        let all = select_task_history_group(&all_samples, cell, period, &config);
        assert_eq!(all.group, TaskHistoryGroup::AllTasks);
        assert_eq!(all.group.level(), "all_tasks");
        assert_eq!(all.sample_count, 12);
        assert!(matches!(
            all.verdict,
            DistributionVerdict::Distribution { .. }
        ));
        all_samples.pop();
        let insufficient = select_task_history_group(&all_samples, cell, period, &config);
        assert_eq!(insufficient.group, TaskHistoryGroup::AllTasks);
        assert!(matches!(
            insufficient.verdict,
            DistributionVerdict::InsufficientEvidence { min_samples: 12 }
        ));
    }

    #[test]
    fn a_completed_task_produces_one_priced_eligible_sample() {
        let mut conn = open_test_ledger("eligible");
        seed_session(&conn, "s1");
        let day = day_nanos("2026-08-25");
        seed_marker(&conn, "s1", "work", day + 1);
        let one_hour = 3_600_000_000_000;
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 1000)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![
                tracker_event(
                    1,
                    "T1",
                    Some("open"),
                    Some("in_progress"),
                    "2026-08-25T00:30:00Z",
                ),
                tracker_event(
                    2,
                    "T1",
                    Some("in_progress"),
                    Some("closed"),
                    "2026-08-25T02:00:00Z",
                ),
            ]),
        )
        .unwrap();
        seed_resolved_kind(&conn, "T1", "task");
        crate::store::cost_model::seed_initial_cost_model(
            &mut conn,
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();

        let period = SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        };
        let samples = gather_task_history_samples(
            &conn,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &crate::config::ModelTable::default(),
        )
        .unwrap();

        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(matches!(samples[0].pricing, TaskPricing::Priced { .. }));
        assert_eq!(
            samples[0].account_evidence,
            AccountEvidenceClass::ExplicitLauncherOrHook,
            "the seeded marker makes this task eligible"
        );
        assert_eq!(
            crate::advice::historical_distribution::ineligibility_reason(&samples[0]),
            None
        );
        assert!(samples[0].segmentation_complete);
    }

    #[test]
    fn a_bug_enters_the_same_unlabelled_cell_as_a_task() {
        let mut conn = open_test_ledger("different-kind");
        seed_session(&conn, "s1");
        let day = day_nanos("2026-08-25");
        let one_hour = 3_600_000_000_000;
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 1000)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![
                tracker_event(
                    1,
                    "T1",
                    Some("open"),
                    Some("in_progress"),
                    "2026-08-25T00:30:00Z",
                ),
                tracker_event(
                    2,
                    "T1",
                    Some("in_progress"),
                    Some("closed"),
                    "2026-08-25T02:00:00Z",
                ),
            ]),
        )
        .unwrap();
        seed_resolved_kind(&conn, "T1", "bug");
        crate::store::cost_model::seed_initial_cost_model(
            &mut conn,
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();

        let period = SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        };
        let samples = gather_task_history_samples(
            &conn,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &crate::config::ModelTable::default(),
        )
        .unwrap();
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert_eq!(samples[0].group, TaskRoutingCell::default());
    }

    /// A task never claimed and released (no release boundary at all) is not
    /// "completed" under this module's own definition, and produces no sample
    /// even though it has attributed usage.
    #[test]
    fn a_task_with_no_release_boundary_is_not_completed_and_produces_no_sample() {
        let mut conn = open_test_ledger("no-release");
        seed_session(&conn, "s1");
        let day = day_nanos("2026-08-25");
        let one_hour = 3_600_000_000_000;
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 1000)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![tracker_event(
                1,
                "T1",
                Some("open"),
                Some("in_progress"),
                "2026-08-25T00:30:00Z",
            )]),
        )
        .unwrap();
        seed_resolved_kind(&conn, "T1", "task");
        crate::store::cost_model::seed_initial_cost_model(
            &mut conn,
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();

        let period = SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        };
        let samples = gather_task_history_samples(
            &conn,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &crate::config::ModelTable::default(),
        )
        .unwrap();
        assert!(samples.is_empty(), "{samples:?}");
    }

    /// A task priced with no active cost model refuses to a priceable fact:
    /// `UnknownTokenComponents`, never a silently zero or estimated price.
    #[test]
    fn a_task_with_no_active_cost_model_prices_as_unknown_token_components() {
        let conn = open_test_ledger("no-cost-model");
        seed_session(&conn, "s1");
        let day = day_nanos("2026-08-25");
        let one_hour = 3_600_000_000_000;
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 1000)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![
                tracker_event(
                    1,
                    "T1",
                    Some("open"),
                    Some("in_progress"),
                    "2026-08-25T00:30:00Z",
                ),
                tracker_event(
                    2,
                    "T1",
                    Some("in_progress"),
                    Some("closed"),
                    "2026-08-25T02:00:00Z",
                ),
            ]),
        )
        .unwrap();
        seed_resolved_kind(&conn, "T1", "task");
        // No cost model seeded at all.

        let period = SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        };
        let samples = gather_task_history_samples(
            &conn,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &crate::config::ModelTable::default(),
        )
        .unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].pricing, TaskPricing::UnknownTokenComponents);
    }

    /// `gather_task_history_group_report` synthesizes an
    /// `InsufficientEvidence` report, never a panic or a fabricated
    /// distribution, with zero completed tasks in the store.
    #[test]
    fn zero_completed_tasks_reports_insufficient_evidence_not_a_panic() {
        let conn = open_test_ledger("zero-tasks");
        let period = SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        };
        let config = crate::advice::historical_distribution::HistoricalDistributionConfig {
            central_low: crate::advice::historical_distribution::Percentile::new(25).unwrap(),
            central_high: crate::advice::historical_distribution::Percentile::new(75).unwrap(),
            upper: crate::advice::historical_distribution::Percentile::new(90).unwrap(),
            min_samples: 12,
            quantile_method: crate::advice::historical_distribution::QuantileMethod::NearestRank,
            attribution_floor: crate::attribution::quality::AttributionQualityFloor::new(0.80)
                .unwrap(),
        };
        let report = gather_task_history_group_report(
            &conn,
            TaskRoutingCell::default(),
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &config,
            &crate::config::ModelTable::default(),
        )
        .unwrap();
        assert_eq!(report.sample_count, 0);
        assert_eq!(report.group, TaskHistoryGroup::AllTasks);
        assert_eq!(report.group.level(), "all_tasks");
        assert!(matches!(
            report.verdict,
            DistributionVerdict::InsufficientEvidence { min_samples: 12 }
        ));
    }

    /// A Codex subagent event inside a claimed task inherits its parent's
    /// marker timeline (`aub-wvrw`): the task keeps an explicit evidence
    /// class instead of falling to `Unattributed` and out of the reference
    /// distribution as `unknown_account_attribution`.
    #[test]
    fn a_subagent_event_inside_a_task_inherits_the_parent_account() {
        let mut conn = open_test_ledger("subagent-inherits");
        seed_session(&conn, "parent-1");
        seed_session_with_parent(&conn, "child-1", "parent-1");
        let day = day_nanos("2026-08-25");
        let one_hour = 3_600_000_000_000;
        seed_marker(&conn, "parent-1", "work", day + 1);
        seed_canonical(&conn, "e1", day + one_hour, "child-1", &[("input", 1000)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![
                tracker_event(
                    1,
                    "T1",
                    Some("open"),
                    Some("in_progress"),
                    "2026-08-25T00:30:00Z",
                ),
                tracker_event(
                    2,
                    "T1",
                    Some("in_progress"),
                    Some("closed"),
                    "2026-08-25T02:00:00Z",
                ),
            ]),
        )
        .unwrap();
        seed_resolved_kind(&conn, "T1", "task");
        crate::store::cost_model::seed_initial_cost_model(
            &mut conn,
            UtcTimestamp::from_unix_nanos(0),
        )
        .unwrap();

        let period = SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        };
        let samples = gather_task_history_samples(
            &conn,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &crate::config::ModelTable::default(),
        )
        .unwrap();

        assert_eq!(samples.len(), 1, "{samples:?}");
        assert_eq!(
            samples[0].account_evidence,
            AccountEvidenceClass::ExplicitLauncherOrHook,
            "a subagent event inherits its parent marker instead of going unattributed"
        );
    }
}
