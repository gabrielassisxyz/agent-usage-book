//! Assembly of `aub task report` and `aub task overhead` (`aub-eu7.4`).
//!
//! Neither function segments anything itself: both read canonical events and
//! the tracker's durable claim/release history, then hand them to
//! [`crate::attribution::report::attribute_events`], which is the same
//! attribution `aub spend --group-by task` reads back. A report command that
//! reimplemented classification here would be exactly the defect
//! `aub-eu7.4`'s acceptance criteria name: "the commands contain no
//! segmentation logic of their own".
//!
//! May not depend on:
//! - presentation

use std::collections::BTreeMap;

use crate::attribution::report::{AttributableEvent, attribute_events};
use crate::attribution::segment::{OverheadReason, SegmentTarget};
use crate::domain::credits::Credits;
use crate::domain::ids::TaskId;
use crate::domain::provenance::{EvidenceId, QuerySemantics};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::evidence::{Derivation, Provenance, RequiredFact};
use crate::logging::LogicalName;
use crate::report::models::{
    IngestionGeneration, LedgerGeneration, ReportMetadata, SharePpm, TaskOverheadBucket,
    TaskOverheadReport, TaskReport, TaskSessionUsage,
};
use crate::report::provenance::{ProvenanceNode, ValueArithmetic};
use crate::report::spend::{SpendWindow, canonical_usage};
use crate::store::spend::{CanonicalSpendEvent, UNKNOWN_SESSION};

/// Assembles `aub task report TASK-ID`: the task's total usage across every
/// session that contributed to it, its resolved task-kind identity,
/// subscription credits where a complete cost model exists, and the session
/// breakdown.
pub fn assemble_task_report(
    conn: &rusqlite::Connection,
    task_id: &TaskId,
    generated_at: UtcTimestamp,
) -> Result<TaskReport, Error> {
    let events = all_canonical_events(conn)?;
    let diagnostics = crate::store::spend::diagnostics(conn)?;
    let partial = !diagnostics.quarantined_by_class.is_empty();
    let attributed = attribute_all(conn, &events)?;

    let task_label = format!(
        "{}:{}",
        task_id.source().as_str(),
        task_id.native().as_str()
    );
    let task_events: Vec<&CanonicalSpendEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                attributed.get(&event.canonical_id),
                Some(SegmentTarget::Task(id)) if id == task_id
            )
        })
        .collect();

    let usage = canonical_usage(&task_events, partial);
    let sessions = session_breakdown(conn, &task_events, partial)?;
    let task_kind = crate::store::task_identity::read_task_identity(conn, task_id)?;
    let credits = task_credits(conn, generated_at, &usage)?;

    let metadata = report_metadata(conn, generated_at)?;
    let usage_node = evidence_node(
        &task_events,
        QuerySemantics::new("task", task_label.clone()),
        ValueArithmetic::Sum,
    );
    let credits_node = evidence_node(
        &task_events,
        QuerySemantics::new("task_credits", task_label.clone()),
        ValueArithmetic::Converted {
            from: crate::report::provenance::Unit::Tokens,
            to: crate::report::provenance::Unit::Credits,
        },
    );

    Ok(TaskReport::new(
        metadata,
        LogicalName::new(task_label),
        task_kind,
        usage,
        credits,
        sessions,
        usage_node,
        credits_node,
    ))
}

/// Assembles `aub task overhead --since`: every overhead bucket usage landed
/// in over the window, alongside the total task-attributed usage in the same
/// window so the two are visible on one report rather than one behind a flag
/// (`aub-eu7.3`'s restored criterion).
///
/// Only buckets that actually received usage in the window are rendered: an
/// empty bucket carries no magnitude to name and no evidence to cite, and a
/// report row with nothing behind it would invite exactly the "why is this
/// here" question `aub-eu7.4`'s honesty invariants exist to avoid. This is a
/// stated design choice, not silent: the eight overhead reasons are a closed,
/// named vocabulary the reader already has (`OverheadReason::ALL`), so a
/// bucket's absence from the report reads as "zero", not as "unknown".
pub fn assemble_task_overhead(
    conn: &rusqlite::Connection,
    window: SpendWindow,
    generated_at: UtcTimestamp,
) -> Result<TaskOverheadReport, Error> {
    let events =
        crate::store::spend::canonical_events(conn, window.since.start(), window.until.start())?;
    let diagnostics = crate::store::spend::diagnostics(conn)?;
    let partial = !diagnostics.quarantined_by_class.is_empty();
    let attributed = attribute_all(conn, &events)?;

    let task_events: Vec<&CanonicalSpendEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                attributed.get(&event.canonical_id),
                Some(SegmentTarget::Task(_))
            )
        })
        .collect();
    let task_usage = canonical_usage(&task_events, partial);
    let task_usage_node = evidence_node(
        &task_events,
        QuerySemantics::new(
            "task_overhead_task_usage",
            format!("{}..{}", window.since.iso(), window.until.iso()),
        ),
        ValueArithmetic::Sum,
    );

    let mut by_reason: BTreeMap<OverheadReason, Vec<&CanonicalSpendEvent>> = BTreeMap::new();
    for event in &events {
        if let Some(SegmentTarget::Overhead(reason)) = attributed.get(&event.canonical_id) {
            by_reason.entry(*reason).or_default().push(event);
        }
    }
    let total_overhead_magnitude: u64 = by_reason
        .values()
        .map(|members| total_known_tokens(&canonical_usage(members, partial)))
        .sum();

    let mut buckets = Vec::new();
    let mut bucket_nodes = Vec::new();
    for reason in OverheadReason::ALL {
        let Some(members) = by_reason.get(&reason) else {
            continue;
        };
        let usage = canonical_usage(members, partial);
        let magnitude = total_known_tokens(&usage);
        let name = LogicalName::new(reason.as_str());
        buckets.push(TaskOverheadBucket {
            reason: name.clone(),
            usage,
            share: SharePpm::of(magnitude, total_overhead_magnitude),
        });
        bucket_nodes.push((
            name,
            evidence_node(
                members,
                QuerySemantics::new(
                    "task_overhead_bucket",
                    format!("{}..{}", window.since.iso(), window.until.iso()),
                ),
                ValueArithmetic::Sum,
            ),
        ));
    }

    let metadata = report_metadata(conn, generated_at)?;
    Ok(TaskOverheadReport::new(
        metadata,
        window.since,
        window.until,
        task_usage,
        task_usage_node,
        buckets,
        bucket_nodes,
    ))
}

/// `pub(crate)`: also the entry point `aub-cab.4`'s can-run task-history
/// gathering uses to enumerate every completed task's usage, rather than
/// opening a second `canonical_events` scan of its own.
pub(crate) fn all_canonical_events(
    conn: &rusqlite::Connection,
) -> Result<Vec<CanonicalSpendEvent>, Error> {
    crate::store::spend::canonical_events(
        conn,
        UtcTimestamp::from_unix_nanos(0),
        UtcTimestamp::from_unix_nanos(i64::MAX),
    )
}

/// Attributes every event to its target, keyed by the event's own
/// `canonical_id`, calling the shared segmentation wiring exactly once per
/// assembly rather than once per task or per bucket.
///
/// `pub(crate)`: shared with `aub-cab.4`'s can-run task-history gathering,
/// for the same reason `all_canonical_events` is: one segmentation pass over
/// the whole ledger, not one per task.
pub(crate) fn attribute_all(
    conn: &rusqlite::Connection,
    events: &[CanonicalSpendEvent],
) -> Result<BTreeMap<String, SegmentTarget>, Error> {
    let boundaries = crate::store::task_event::read_boundaries(conn)?;
    let attributable: Vec<AttributableEvent> = events
        .iter()
        .map(|event| AttributableEvent {
            canonical_id: event.canonical_id.clone(),
            occurred_at: event.occurred_at,
            session_is_mapped: event.session != UNKNOWN_SESSION,
            usage: known_vector(event),
        })
        .collect();
    Ok(attribute_events(boundaries, true, &attributable)
        .into_iter()
        .map(|attribution| (attribution.canonical_id, attribution.target))
        .collect())
}

/// `pub(crate)`: shared with `aub-cab.4`'s can-run task-history gathering,
/// which needs the same per-event known-token extraction to build the account
/// segmentation input for each task's own attributed events.
pub(crate) fn known_vector(event: &CanonicalSpendEvent) -> crate::domain::tokens::KnownTokenVector {
    use crate::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
    };
    KnownTokenVector::new(
        InputTokens::new(event.components.get("input").copied().unwrap_or(0)),
        OutputTokens::new(event.components.get("output").copied().unwrap_or(0)),
        CacheReadTokens::new(event.components.get("cache_read").copied().unwrap_or(0)),
        CacheWriteTokens::new(event.components.get("cache_write").copied().unwrap_or(0)),
    )
}

fn total_known_tokens(usage: &crate::domain::tokens::UsageVector) -> u64 {
    let known = usage.known();
    known.input().value()
        + known.output().value()
        + known.cache_read().value()
        + known.cache_write().value()
}

/// The per-session breakdown of a task's usage, each with the session's own
/// run identifier where the session carries one (`aub-eu7.4`'s RunId
/// criterion). Coverage and evidence quality propagate per session exactly
/// as they do for the task total, through the same shared aggregation.
fn session_breakdown(
    conn: &rusqlite::Connection,
    task_events: &[&CanonicalSpendEvent],
    partial: bool,
) -> Result<Vec<TaskSessionUsage>, Error> {
    let sessions = crate::store::session::load_all_sessions(conn)?;
    let run_by_label: BTreeMap<String, Option<crate::domain::ids::NativeRunId>> = sessions
        .iter()
        .map(|session| {
            (
                format!(
                    "{}:{}",
                    session.source().as_str(),
                    session.native_session_id().as_str()
                ),
                session.run_id().cloned(),
            )
        })
        .collect();

    let mut by_session: BTreeMap<String, Vec<&CanonicalSpendEvent>> = BTreeMap::new();
    for event in task_events {
        by_session
            .entry(event.session.clone())
            .or_default()
            .push(event);
    }
    Ok(by_session
        .into_iter()
        .map(|(session_label, members)| TaskSessionUsage {
            session: LogicalName::new(session_label.clone()),
            run: run_by_label.get(&session_label).cloned().flatten(),
            usage: canonical_usage(&members, partial),
        })
        .collect())
}

/// The task's credit derivation under the cost model active at `generated_at`,
/// or a named refusal when no cost model is active: "credits where a complete
/// cost model exists" still produces a document either way, never a silently
/// absent field.
fn task_credits(
    conn: &rusqlite::Connection,
    generated_at: UtcTimestamp,
    usage: &crate::domain::tokens::UsageVector,
) -> Result<Derivation<Credits>, Error> {
    match crate::store::cost_model::load_active_at(conn, generated_at)? {
        Some(model) => Ok(crate::cost_model::convert(&model, usage)),
        None => Ok(Derivation::unavailable(
            [RequiredFact::new("active cost model")],
            Provenance::new(["cost-model:unavailable".to_string()]),
        )
        .expect("the active cost model is a named missing fact")),
    }
}

fn report_metadata(
    conn: &rusqlite::Connection,
    generated_at: UtcTimestamp,
) -> Result<ReportMetadata, Error> {
    Ok(ReportMetadata::new(
        generated_at,
        generated_at,
        LedgerGeneration::new(crate::store::ledger_generation::current(conn)?.value()),
        Some(IngestionGeneration::new(
            crate::store::ingestion_generation::current(conn)?.value(),
        )),
    ))
}

fn evidence_node(
    events: &[&CanonicalSpendEvent],
    semantics: QuerySemantics,
    arithmetic: ValueArithmetic,
) -> ProvenanceNode {
    let sources = events
        .iter()
        .flat_map(|event| event.sources.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    ProvenanceNode::new(
        events
            .iter()
            .map(|event| EvidenceId::new(event.canonical_id.clone())),
        [],
        semantics,
        sources.len() as u64,
        events.len() as u64,
        arithmetic,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{TrackerEventReader, TrackerEventRecord};
    use crate::domain::ids::{NativeSessionId, NativeTaskId, SourceNamespace};
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
            std::env::temp_dir().join(format!("aub-report-task-{tag}-{}", std::process::id()));
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

    fn seed_session(conn: &rusqlite::Connection, name: &str, run: Option<&str>) {
        insert_session(
            conn,
            &NewSession {
                source: SourceNamespace::new("fixture"),
                native_session_id: NativeSessionId::new(name),
                start: UtcTimestamp::from_unix_nanos(0),
                end: None,
                project_key: ProjectKey::new("project-a"),
                repository_key: RepositoryKey::new("repository-a"),
                run_id: run.map(crate::domain::ids::NativeRunId::new),
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

    fn claim(task_native: &str, at: &str) -> TrackerEventRecord {
        TrackerEventRecord {
            upstream_id: 1,
            task_native: task_native.to_string(),
            event_type: "status_changed".to_string(),
            old_value: Some("open".to_string()),
            new_value: Some("in_progress".to_string()),
            occurred_at: at.to_string(),
            actor: None,
        }
    }

    #[test]
    fn task_report_reconciles_to_the_task_s_attributed_events_and_lists_sessions() {
        let conn = open_test_ledger("task-report");
        seed_session(&conn, "s1", Some("run-1"));
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        let one_hour = 3_600_000_000_000;
        // Before the claim: overhead, must not appear in the task's report.
        seed_canonical(&conn, "e0", day, "s1", &[("input", 100)]);
        // After the claim to T1: attributed to T1.
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 7)]);

        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![claim("T1", "2026-08-25T00:30:00Z")]),
        )
        .unwrap();

        let task_id = TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new("T1"));
        let report = assemble_task_report(
            &conn,
            &task_id,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();

        assert_eq!(
            report.usage.known().input().value(),
            7,
            "must exclude the pre-claim event"
        );
        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].session.as_str(), "fixture:s1");
        assert_eq!(
            report.sessions[0].run.as_ref().map(|r| r.as_str()),
            Some("run-1")
        );
        assert_eq!(report.sessions[0].usage.known().input().value(), 7);
    }

    /// A task touched by a quarantined record reads as partial: coverage and
    /// evidence quality propagate into the task report exactly as they do for
    /// `aub spend`.
    #[test]
    fn a_task_touched_by_a_quarantined_record_is_partial() {
        let conn = open_test_ledger("task-report-partial");
        seed_session(&conn, "s1", None);
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        let one_hour = 3_600_000_000_000;
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 7)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![claim("T1", "2026-08-25T00:30:00Z")]),
        )
        .unwrap();
        // A record with an unusable timestamp is quarantined by the transcript
        // ingest path, not the canonical read path this test exercises
        // directly; the planted negative below is the same report with no
        // quarantine present.
        crate::store::ingest_quarantine::record_quarantine(
            &conn,
            &crate::store::ingest_quarantine::NewQuarantineItem {
                source_file: "fixture.jsonl".to_string(),
                byte_offset: None,
                line_number: None,
                parser: "fixture-v1".to_string(),
                failure_class: "wrong_field_type".to_string(),
                excerpt_hash: "digest".to_string(),
                excerpt: None,
                observed_at: UtcTimestamp::from_unix_nanos(day),
            },
        )
        .unwrap();

        let task_id = TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new("T1"));
        let report = assemble_task_report(
            &conn,
            &task_id,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();
        assert!(
            report.usage.coverage().missing().is_some(),
            "a task touched by a quarantined record must read as partial"
        );

        // Planted negative: no quarantine present, and the same task reads complete.
        let clean = open_test_ledger("task-report-complete");
        seed_session(&clean, "s1", None);
        seed_canonical(&clean, "e1", day + one_hour, "s1", &[("input", 7)]);
        crate::store::task_event::ingest(
            &clean,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![claim("T1", "2026-08-25T00:30:00Z")]),
        )
        .unwrap();
        let clean_report = assemble_task_report(
            &clean,
            &task_id,
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();
        assert!(clean_report.usage.coverage().missing().is_none());
    }

    #[test]
    fn task_overhead_buckets_reconcile_to_the_non_task_attributed_total_and_shares_sum_to_one() {
        let conn = open_test_ledger("task-overhead");
        seed_session(&conn, "s1", None);
        let day = UtcDate::parse("2026-08-25").unwrap().start().unix_nanos();
        let one_hour = 3_600_000_000_000;
        seed_canonical(&conn, "e0", day, "s1", &[("input", 10)]);
        seed_canonical(&conn, "e1", day + one_hour, "s1", &[("input", 7)]);
        crate::store::task_event::ingest(
            &conn,
            SourceNamespace::new("beads-a"),
            &FixtureReader(vec![claim("T1", "2026-08-25T00:30:00Z")]),
        )
        .unwrap();

        let report = assemble_task_overhead(
            &conn,
            SpendWindow::starting(UtcDate::parse("2026-08-25").unwrap(), 1).unwrap(),
            UtcTimestamp::parse_rfc3339("2026-08-26T00:00:00Z").unwrap(),
        )
        .unwrap();

        assert_eq!(report.task_usage.known().input().value(), 7);
        assert_eq!(report.buckets.len(), 1);
        assert_eq!(report.buckets[0].reason.as_str(), "before_first_claim");
        assert_eq!(report.buckets[0].usage.known().input().value(), 10);
        assert_eq!(
            report.buckets[0].share.get(),
            SharePpm::MAX,
            "a single bucket carries the whole overhead total's share"
        );
    }
}
