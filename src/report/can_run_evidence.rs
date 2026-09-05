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

use std::collections::{BTreeSet, HashMap};

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
use crate::attribution::{TaskIdentityState, TaskKind};
use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace, TaskId};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::evidence::Derivation;
use crate::store::spend::CanonicalSpendEvent;

/// Every completed task of `task_kind` in `period`, joined into one
/// [`GroupHistoryReport`]. Never through new SQL outside `src/store`: the
/// canonical event scan, the segmentation join and the account-marker lookup
/// are the same store functions `aub task report` and `aub spend` already
/// call.
pub fn gather_task_history_group_report(
    conn: &rusqlite::Connection,
    task_kind: TaskKind,
    period: SelectionPeriod,
    generated_at: UtcTimestamp,
    config: &HistoricalDistributionConfig,
) -> Result<GroupHistoryReport<TaskKind>, Error> {
    let samples = gather_task_history_samples(conn, task_kind, period, generated_at)?;
    let mut reports = build_group_reports(samples, period, config);
    Ok(reports
        .remove(&task_kind)
        .unwrap_or_else(|| GroupHistoryReport {
            group: task_kind,
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
        }))
}

/// Builds one [`TaskHistorySample`] per completed task of `task_kind` whose
/// release boundary falls in `period`. A task with no attributed usage at all
/// contributes no sample: there is nothing to price or classify.
fn gather_task_history_samples(
    conn: &rusqlite::Connection,
    task_kind: TaskKind,
    period: SelectionPeriod,
    generated_at: UtcTimestamp,
) -> Result<Vec<TaskHistorySample<TaskKind>>, Error> {
    let events = crate::report::task::all_canonical_events(conn)?;
    let diagnostics = crate::store::spend::diagnostics(conn)?;
    let partial = !diagnostics.quarantined_by_class.is_empty();
    let attributed = crate::report::task::attribute_all(conn, &events)?;
    let boundaries = crate::store::task_event::read_boundaries(conn)?;
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
        let Some(identity) = identity else {
            continue;
        };
        if identity.state != TaskIdentityState::Resolved {
            continue;
        }
        let Some(kind) = identity.kind else {
            continue;
        };
        if kind != task_kind {
            continue;
        }

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
            group: task_kind,
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
/// directly: there is no marker timeline to consult.
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
                    let markers = crate::store::session_account_marker::markers_for_session(
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
    use crate::attribution::{TrackerEventReader, TrackerEventRecord};
    use crate::domain::ids::SourceNamespace;
    use crate::domain::time::{FakeClock, MonotonicDuration, UtcDate};
    use crate::sessions::{ProjectKey, RepositoryKey};
    use crate::store::connection::{AccessMode, PragmaPolicy};
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
        let mut conn = crate::store::connection::open(
            &root.join("ledger.db"),
            AccessMode::ReadWrite,
            &PragmaPolicy {
                busy_timeout: MonotonicDuration::from_millis(100),
            },
        )
        .unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        conn
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
                run_id: None,
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

    /// Done-when (`aub-cab.4`): a completed task of the requested kind, with
    /// clean measured usage and explicit account attribution, produces exactly
    /// one eligible sample priced through the active cost model.
    #[test]
    fn a_completed_task_of_the_requested_kind_produces_one_priced_eligible_sample() {
        let mut conn = open_test_ledger("eligible");
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
            TaskKind::Task,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();

        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(matches!(samples[0].pricing, TaskPricing::Priced { .. }));
        assert_eq!(
            samples[0].account_evidence,
            AccountEvidenceClass::Unattributed,
            "no account marker was seeded, so the session resolves unattributed"
        );
        assert!(samples[0].segmentation_complete);
    }

    /// Planted negative: a task of a *different* kind never enters the
    /// requested kind's sample list, even though it is otherwise eligible.
    #[test]
    fn a_task_of_a_different_kind_is_excluded_entirely() {
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
            TaskKind::Task,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();
        assert!(samples.is_empty(), "{samples:?}");
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
            TaskKind::Task,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
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
            TaskKind::Task,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].pricing, TaskPricing::UnknownTokenComponents);
    }

    /// `gather_task_history_group_report` synthesizes an
    /// `InsufficientEvidence` report, never a panic or a fabricated
    /// distribution, for a kind with zero completed tasks in the store.
    #[test]
    fn a_kind_with_zero_completed_tasks_reports_insufficient_evidence_not_a_panic() {
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
            TaskKind::Task,
            period,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
            &config,
        )
        .unwrap();
        assert_eq!(report.sample_count, 0);
        assert!(matches!(
            report.verdict,
            DistributionVerdict::InsufficientEvidence { min_samples: 12 }
        ));
    }
}
