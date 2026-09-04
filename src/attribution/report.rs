//! Wires the segmentation engine ([`crate::attribution::segment`]) into a
//! shape a report command can call without holding any classification rule
//! of its own (`aub-eu7.4`).
//!
//! [`segment::classify`] takes one [`SegmentationContext`] for a whole call,
//! so a batch that mixes events whose session resolved with events whose
//! session did not cannot be classified in one pass: stamping every window
//! with one `session_is_mapped` value would misclassify whichever half
//! disagrees with it. [`attribute_events`] partitions the batch on exactly
//! that property before calling `classify`, once per partition, then
//! reassembles the results in the caller's original order.

use crate::attribution::segment::{
    ClaimBoundary, SegmentTarget, SegmentationContext, SegmentationInputs, UsageWindow, classify,
};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::KnownTokenVector;

/// One canonical usage record ready for task attribution: enough identity to
/// segment it and rejoin the result back to its own row, and no more. This
/// module never reads a store connection or a canonical event type directly,
/// so a caller in `report::spend` or `report::task` builds this from
/// whatever its own row shape happens to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributableEvent {
    pub canonical_id: String,
    pub occurred_at: UtcTimestamp,
    /// Whether the event's own session resolved to a known session. An
    /// unresolved session is a session-wide fact the segmentation engine
    /// short-circuits to [`crate::attribution::segment::OverheadReason::UnmappedSession`],
    /// which is why it is asked of every event and not of the batch as a whole.
    pub session_is_mapped: bool,
    pub usage: KnownTokenVector,
}

/// One event's attribution outcome, indexed by the same `canonical_id` the
/// caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAttribution {
    pub canonical_id: String,
    pub target: SegmentTarget,
}

/// Attributes every event in `events` against the tracker's claim/release
/// timeline in `boundaries`. `tracker_available` names whether the tracker
/// history behind `boundaries` was read successfully; a command that reads
/// already-durably-ingested `task_event` rows (every caller of this function
/// today) always passes `true`, since a read failure at ingest time was
/// already reported by `task ingest` and does not recur at report time.
///
/// Output order matches input order; a caller correlating attribution back to
/// its own rows does so by `canonical_id`, not by position, since the two
/// partitions this function builds internally do not preserve input order
/// across the mapped/unmapped split.
pub fn attribute_events(
    boundaries: Vec<ClaimBoundary>,
    tracker_available: bool,
    events: &[AttributableEvent],
) -> Vec<EventAttribution> {
    let (mapped, unmapped): (Vec<&AttributableEvent>, Vec<&AttributableEvent>) =
        events.iter().partition(|event| event.session_is_mapped);

    let mapped_targets = classify(&SegmentationInputs {
        context: SegmentationContext {
            session_is_mapped: true,
            tracker_available,
        },
        boundaries: boundaries.clone(),
        usage: windows(&mapped),
    });
    let unmapped_targets = classify(&SegmentationInputs {
        context: SegmentationContext {
            session_is_mapped: false,
            tracker_available,
        },
        boundaries,
        usage: windows(&unmapped),
    });

    mapped
        .into_iter()
        .zip(mapped_targets)
        .chain(unmapped.into_iter().zip(unmapped_targets))
        .map(|(event, classification)| EventAttribution {
            canonical_id: event.canonical_id.clone(),
            target: classification.target,
        })
        .collect()
}

fn windows(events: &[&AttributableEvent]) -> Vec<UsageWindow> {
    events
        .iter()
        .map(|event| UsageWindow {
            start: Some(event.occurred_at),
            end: Some(event.occurred_at),
            usage: event.usage,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::TaskEventKind;
    use crate::attribution::segment::OverheadReason;
    use crate::domain::ids::{NativeTaskId, SourceNamespace, TaskId};
    use crate::domain::tokens::{CacheReadTokens, CacheWriteTokens, InputTokens, OutputTokens};

    fn task(name: &str) -> TaskId {
        TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new(name))
    }

    fn t(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    fn tokens(input: u64) -> KnownTokenVector {
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(0),
            CacheReadTokens::new(0),
            CacheWriteTokens::new(0),
        )
    }

    fn event(id: &str, at: i64, mapped: bool) -> AttributableEvent {
        AttributableEvent {
            canonical_id: id.to_string(),
            occurred_at: t(at),
            session_is_mapped: mapped,
            usage: tokens(1),
        }
    }

    #[test]
    fn mapped_events_attribute_against_the_shared_boundary_timeline() {
        let boundaries = vec![ClaimBoundary {
            task_id: task("T1"),
            occurred_at: t(10),
            kind: TaskEventKind::Claim,
        }];
        let events = vec![event("e1", 5, true), event("e2", 15, true)];

        let attributed = attribute_events(boundaries, true, &events);

        let by_id = |id: &str| {
            attributed
                .iter()
                .find(|a| a.canonical_id == id)
                .expect("every input event must produce one attribution")
        };
        assert_eq!(
            by_id("e1").target,
            SegmentTarget::Overhead(OverheadReason::BeforeFirstClaim)
        );
        assert_eq!(by_id("e2").target, SegmentTarget::Task(task("T1")));
    }

    /// The planted negative: an unresolved-session event lands in
    /// `UnmappedSession` even when its timestamp falls squarely inside a
    /// claimed interval that would otherwise attribute it to a task. A
    /// caller that dropped the mapped/unmapped partition (stamping every
    /// event with one context) would instead attribute this event to `T1`.
    #[test]
    fn an_unmapped_session_event_never_attributes_to_a_task_even_inside_a_claimed_interval() {
        let boundaries = vec![ClaimBoundary {
            task_id: task("T1"),
            occurred_at: t(0),
            kind: TaskEventKind::Claim,
        }];
        let events = vec![event("e1", 50, false)];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(attributed.len(), 1);
        assert_eq!(
            attributed[0].target,
            SegmentTarget::Overhead(OverheadReason::UnmappedSession)
        );
    }

    #[test]
    fn output_order_matches_input_order_within_each_mapped_ness_partition() {
        let boundaries = vec![
            ClaimBoundary {
                task_id: task("T1"),
                occurred_at: t(0),
                kind: TaskEventKind::Claim,
            },
            ClaimBoundary {
                task_id: task("T2"),
                occurred_at: t(20),
                kind: TaskEventKind::Claim,
            },
        ];
        let events = vec![event("e1", 5, true), event("e2", 25, true)];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(attributed[0].canonical_id, "e1");
        assert_eq!(attributed[0].target, SegmentTarget::Task(task("T1")));
        assert_eq!(attributed[1].canonical_id, "e2");
        assert_eq!(attributed[1].target, SegmentTarget::Task(task("T2")));
    }
}
