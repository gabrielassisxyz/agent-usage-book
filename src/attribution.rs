//! Account, project, repository, and task attribution.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

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
