//! Wires the segmentation engine ([`crate::attribution::segment`]) into a
//! shape a report command can call without holding any classification rule
//! of its own (`aub-eu7.4`).
//!
//! [`segment::classify`] takes one [`SegmentationContext`] and one boundary
//! list for a whole call, so one call can never classify a batch that mixes
//! sessions: stamping every window with one `session_is_mapped` value would
//! misclassify whichever half disagrees with it, and running every session's
//! usage against one tracker-wide timeline hands each lane the claims the
//! other lanes made. [`attribute_events`] therefore partitions the batch by
//! session before calling `classify`, and reassembles the results in the
//! caller's original order.
//!
//! **The two timelines, and which one wins.** A boundary whose actor resolved
//! to exactly one session ([`ClaimBoundary::session`]) belongs to that
//! session's own timeline and is invisible to every other session. A boundary
//! that resolved to none - a human actor, a script's hardcoded name, an actor
//! whose fragment fitted several sessions - joins one fallback timeline that
//! applies to a session only where that session has no claim of its own
//! covering the instant. A session-scoped claim always beats the fallback for
//! the same session and instant, which is what stops a claim made by one lane
//! from taking the spend of another lane running beside it.

use std::collections::BTreeMap;

use crate::attribution::segment::{
    ClaimBoundary, OverheadReason, SegmentTarget, SegmentationContext, SegmentationInputs,
    UsageWindow, classify,
};
use crate::domain::ids::SessionId;
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
    /// The event's own session, or `None` when the event carried no session
    /// identity at all. `None` is a session-wide fact the segmentation engine
    /// short-circuits to
    /// [`crate::attribution::segment::OverheadReason::UnmappedSession`], and a
    /// `Some` is also what selects which claim timeline governs the event,
    /// which is why the session travels with every event rather than with the
    /// batch.
    pub session: Option<SessionId>,
    pub usage: KnownTokenVector,
}

/// One event's attribution outcome, indexed by the same `canonical_id` the
/// caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAttribution {
    pub canonical_id: String,
    pub target: SegmentTarget,
}

/// Whether a classification against a session's own timeline means that
/// session had a claim covering the instant. The three reasons below are the
/// timeline saying "no claim of mine is open here", which is exactly when the
/// fallback timeline is allowed to speak; every other outcome, including
/// `Contended`, is the session's own claims deciding the instant and beats the
/// fallback.
fn session_scoped_claim_covers(target: &SegmentTarget) -> bool {
    !matches!(
        target,
        SegmentTarget::Overhead(
            OverheadReason::BeforeFirstClaim
                | OverheadReason::AfterReleaseWithNoNextClaim
                | OverheadReason::UnclaimedSession
        )
    )
}

/// Attributes every event in `events` against the tracker's claim/release
/// timeline in `boundaries`. `tracker_available` names whether the tracker
/// history behind `boundaries` was read successfully; a command that reads
/// already-durably-ingested `task_event` rows (every caller of this function
/// today) always passes `true`, since a read failure at ingest time was
/// already reported by `task ingest` and does not recur at report time.
///
/// Output order matches input order exactly: every event is written back to
/// its own index, so a caller may correlate by position as well as by
/// `canonical_id`.
pub fn attribute_events(
    boundaries: Vec<ClaimBoundary>,
    tracker_available: bool,
    events: &[AttributableEvent],
) -> Vec<EventAttribution> {
    let fallback: Vec<ClaimBoundary> = boundaries
        .iter()
        .filter(|boundary| boundary.session.is_none())
        .cloned()
        .collect();
    let mut scoped: BTreeMap<String, Vec<ClaimBoundary>> = BTreeMap::new();
    for boundary in &boundaries {
        if let Some(session) = &boundary.session {
            scoped
                .entry(session_key(session))
                .or_default()
                .push(boundary.clone());
        }
    }

    let mut by_session: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut unmapped: Vec<usize> = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match &event.session {
            Some(session) => by_session
                .entry(session_key(session))
                .or_default()
                .push(index),
            None => unmapped.push(index),
        }
    }

    let mut targets: Vec<Option<SegmentTarget>> = vec![None; events.len()];

    for (key, indices) in by_session {
        let windows = windows(events, &indices);
        let own = classify(&SegmentationInputs {
            context: SegmentationContext {
                session_is_mapped: true,
                tracker_available,
            },
            boundaries: scoped.get(&key).cloned().unwrap_or_default(),
            usage: windows.clone(),
        });
        let shared = classify(&SegmentationInputs {
            context: SegmentationContext {
                session_is_mapped: true,
                tracker_available,
            },
            boundaries: fallback.clone(),
            usage: windows,
        });
        for ((index, own), shared) in indices.into_iter().zip(own).zip(shared) {
            let target = if session_scoped_claim_covers(&own.target) || fallback.is_empty() {
                own.target
            } else {
                shared.target
            };
            targets[index] = Some(target);
        }
    }

    let unmapped_targets = classify(&SegmentationInputs {
        context: SegmentationContext {
            session_is_mapped: false,
            tracker_available,
        },
        boundaries,
        usage: windows(events, &unmapped),
    });
    for (index, classification) in unmapped.into_iter().zip(unmapped_targets) {
        targets[index] = Some(classification.target);
    }

    events
        .iter()
        .zip(targets)
        .map(|(event, target)| EventAttribution {
            canonical_id: event.canonical_id.clone(),
            target: target.expect("every event index is classified in exactly one partition"),
        })
        .collect()
}

/// A session's stable partition key. `SessionId` is not `Ord`, and the
/// partition has to iterate in a fixed order for the output to be reproducible
/// across runs, so the namespaced label it renders under everywhere else in
/// the crate is the key.
fn session_key(session: &SessionId) -> String {
    format!(
        "{}:{}",
        session.source().as_str(),
        session.native().as_str()
    )
}

fn windows(events: &[AttributableEvent], indices: &[usize]) -> Vec<UsageWindow> {
    indices
        .iter()
        .map(|index| UsageWindow {
            start: Some(events[*index].occurred_at),
            end: Some(events[*index].occurred_at),
            usage: events[*index].usage,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::TaskEventKind;
    use crate::attribution::segment::OverheadReason;
    use crate::domain::ids::{NativeSessionId, NativeTaskId, SourceNamespace, TaskId};
    use crate::domain::tokens::{CacheReadTokens, CacheWriteTokens, InputTokens, OutputTokens};

    fn task(name: &str) -> TaskId {
        TaskId::new(SourceNamespace::new("beads-a"), NativeTaskId::new(name))
    }

    fn session(native: &str) -> SessionId {
        SessionId::new(
            SourceNamespace::new("claude-code"),
            NativeSessionId::new(native),
        )
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

    fn event(id: &str, at: i64, session: Option<SessionId>) -> AttributableEvent {
        AttributableEvent {
            canonical_id: id.to_string(),
            occurred_at: t(at),
            session,
            usage: tokens(1),
        }
    }

    fn claim(task_id: TaskId, at: i64, actor: &str, bound: Option<SessionId>) -> ClaimBoundary {
        ClaimBoundary {
            task_id,
            occurred_at: t(at),
            kind: TaskEventKind::Claim,
            agent_association: Some(actor.to_string()),
            session: bound,
        }
    }

    fn release(task_id: TaskId, at: i64, actor: &str, bound: Option<SessionId>) -> ClaimBoundary {
        ClaimBoundary {
            task_id,
            occurred_at: t(at),
            kind: TaskEventKind::Release,
            agent_association: Some(actor.to_string()),
            session: bound,
        }
    }

    fn target_of<'a>(attributed: &'a [EventAttribution], id: &str) -> &'a SegmentTarget {
        &attributed
            .iter()
            .find(|attribution| attribution.canonical_id == id)
            .expect("every input event must produce one attribution")
            .target
    }

    #[test]
    fn mapped_events_attribute_against_their_own_session_s_boundary_timeline() {
        let a = session("aaaa1111-full");
        let boundaries = vec![claim(task("T1"), 10, "lane-aaaa1111", Some(a.clone()))];
        let events = vec![
            event("e1", 5, Some(a.clone())),
            event("e2", 15, Some(a.clone())),
        ];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(
            *target_of(&attributed, "e1"),
            SegmentTarget::Overhead(OverheadReason::BeforeFirstClaim)
        );
        assert_eq!(
            *target_of(&attributed, "e2"),
            SegmentTarget::Task(task("T1"))
        );
    }

    /// The planted negative for the whole bead: two lanes claiming two
    /// different beads, interleaved in time. Under one shared timeline B's
    /// claim at t=20 would take A's t=25 usage for T2; with each claim bound
    /// to its own session, A stays on T1 throughout and B stays on T2.
    #[test]
    fn a_claim_by_one_session_never_governs_another_session_s_usage() {
        let a = session("aaaa1111-a");
        let b = session("bbbb2222-b");
        let boundaries = vec![
            claim(task("T1"), 10, "lane-aaaa1111", Some(a.clone())),
            claim(task("T2"), 20, "lane-bbbb2222", Some(b.clone())),
        ];
        let events = vec![
            event("a-late", 25, Some(a.clone())),
            event("b-late", 25, Some(b.clone())),
        ];

        let attributed = attribute_events(boundaries.clone(), true, &events);
        assert_eq!(
            *target_of(&attributed, "a-late"),
            SegmentTarget::Task(task("T1")),
            "session A's usage after B's claim still belongs to A's own claim"
        );
        assert_eq!(
            *target_of(&attributed, "b-late"),
            SegmentTarget::Task(task("T2"))
        );

        // The shared-timeline result the old code produced, reconstructed by
        // stripping every binding: both events land on T2, which is the
        // misattribution this bead exists to remove. Asserting the two
        // disagree is what stops the test above passing for a weaker reason.
        let unbound: Vec<ClaimBoundary> = boundaries
            .into_iter()
            .map(|boundary| ClaimBoundary {
                session: None,
                ..boundary
            })
            .collect();
        let shared = attribute_events(unbound, true, &events);
        assert_eq!(
            *target_of(&shared, "a-late"),
            SegmentTarget::Task(task("T2")),
            "the shared timeline is what misattributes A's usage; if it does not, \
             this test proves nothing"
        );
        assert_ne!(
            target_of(&attributed, "a-late"),
            target_of(&shared, "a-late")
        );
    }

    /// A session-less actor governs a session that has no claim of its own at
    /// that instant, and is overruled where the session does have one.
    #[test]
    fn the_fallback_timeline_applies_only_where_no_session_scoped_claim_covers_the_instant() {
        let a = session("aaaa1111-a");
        let b = session("bbbb2222-b");
        let boundaries = vec![
            claim(task("T-fallback"), 10, "gabriel", None),
            claim(task("T-own"), 20, "lane-aaaa1111", Some(a.clone())),
        ];
        let events = vec![
            event("a-after-own-claim", 30, Some(a.clone())),
            event("b-no-claim-of-its-own", 30, Some(b.clone())),
        ];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(
            *target_of(&attributed, "a-after-own-claim"),
            SegmentTarget::Task(task("T-own")),
            "a session-scoped claim beats the fallback for the same instant"
        );
        assert_eq!(
            *target_of(&attributed, "b-no-claim-of-its-own"),
            SegmentTarget::Task(task("T-fallback")),
            "a session with no claim of its own is governed by the fallback"
        );
    }

    /// Precedence is per instant, not per session: the same session is
    /// governed by the fallback before its own claim opens and after its own
    /// release closes.
    #[test]
    fn the_fallback_governs_a_session_before_its_own_claim_and_after_its_own_release() {
        let a = session("aaaa1111-a");
        let boundaries = vec![
            claim(task("T-fallback"), 5, "gabriel", None),
            claim(task("T-own"), 20, "lane-aaaa1111", Some(a.clone())),
            release(task("T-own"), 30, "lane-aaaa1111", Some(a.clone())),
        ];
        let events = vec![
            event("before", 10, Some(a.clone())),
            event("during", 25, Some(a.clone())),
            event("after", 40, Some(a.clone())),
        ];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(
            *target_of(&attributed, "before"),
            SegmentTarget::Task(task("T-fallback"))
        );
        assert_eq!(
            *target_of(&attributed, "during"),
            SegmentTarget::Task(task("T-own"))
        );
        assert_eq!(
            *target_of(&attributed, "after"),
            SegmentTarget::Task(task("T-fallback"))
        );
    }

    /// The planted negative: an unresolved-session event lands in
    /// `UnmappedSession` even when its timestamp falls squarely inside a
    /// claimed interval that would otherwise attribute it to a task. A
    /// caller that dropped the mapped/unmapped partition (stamping every
    /// event with one context) would instead attribute this event to `T1`.
    #[test]
    fn an_unmapped_session_event_never_attributes_to_a_task_even_inside_a_claimed_interval() {
        let boundaries = vec![claim(task("T1"), 0, "gabriel", None)];
        let events = vec![event("e1", 50, None)];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(attributed.len(), 1);
        assert_eq!(
            attributed[0].target,
            SegmentTarget::Overhead(OverheadReason::UnmappedSession)
        );
    }

    #[test]
    fn output_order_matches_input_order_across_sessions() {
        let a = session("aaaa1111-a");
        let b = session("bbbb2222-b");
        let boundaries = vec![
            claim(task("T1"), 0, "lane-aaaa1111", Some(a.clone())),
            claim(task("T2"), 0, "lane-bbbb2222", Some(b.clone())),
        ];
        let events = vec![
            event("b-first", 5, Some(b.clone())),
            event("unmapped", 5, None),
            event("a-second", 25, Some(a.clone())),
        ];

        let attributed = attribute_events(boundaries, true, &events);

        assert_eq!(attributed[0].canonical_id, "b-first");
        assert_eq!(attributed[0].target, SegmentTarget::Task(task("T2")));
        assert_eq!(attributed[1].canonical_id, "unmapped");
        assert_eq!(attributed[2].canonical_id, "a-second");
        assert_eq!(attributed[2].target, SegmentTarget::Task(task("T1")));
    }
}
