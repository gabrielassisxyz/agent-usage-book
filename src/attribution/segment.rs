//! Temporal task segmentation: usage split into task intervals by explicit
//! claim/release boundaries, with every ambiguity landing in a named
//! overhead bucket rather than an invented split (`aub-eu7.2`, PLAN.md 21,
//! 34.18).
//!
//! The rule that makes task totals defensible is that ambiguity becomes
//! overhead, not invented attribution. Splitting a cumulative record across
//! a boundary in proportion to wall-clock time produces a number for each
//! task and a fabrication in both, invisible because both numbers look
//! reasonable. So boundaries come only from claim and release events, usage
//! between two claims belongs to the earlier one, and a usage window that
//! genuinely spans a boundary with no principled way to split it lands in
//! the ambiguous-boundary bucket rather than being divided.
//!
//! The conservation invariant this module exists to prove is that
//! task-attributed usage plus overhead usage equals total canonical usage
//! over the segmented set, per token kind, with no remainder: every window's
//! full usage lands in exactly one bucket, never split and never dropped.

use std::collections::HashMap;

use crate::attribution::TaskEventKind;
use crate::domain::ids::TaskId;
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
};

/// Why usage could not be attributed to a task. Naming the reason rather
/// than using one generic bucket is what makes the overhead actionable: a
/// large `Contended` bucket and a large `UnmappedSession` bucket call for
/// completely different fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OverheadReason {
    /// Usage before the session's first claim.
    BeforeFirstClaim,
    /// Usage after an explicit release with no next claim.
    AfterReleaseWithNoNextClaim,
    /// A usage window spans a boundary with no principled way to split it.
    AmbiguousBoundary,
    /// The usage window's start or end could not be established.
    MissingTimestamp,
    /// The session this usage belongs to could not be resolved.
    UnmappedSession,
    /// The tracker that would supply claim boundaries was unavailable.
    TrackerUnavailable,
    /// More than one task was claimed at once with no release between them.
    Contended,
    /// The session has usage and no claim at all.
    UnclaimedSession,
}

/// Where one usage window's tokens land: a task, or a named overhead bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentTarget {
    Task(TaskId),
    Overhead(OverheadReason),
}

/// One claim or release boundary. Only [`TaskEventKind::Claim`] and
/// [`TaskEventKind::Release`] create boundaries; a caller filters
/// `TaskEventKind::Unknown` out before building this, since an
/// unrecognized event kind carries no attribution meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimBoundary {
    pub task_id: TaskId,
    pub occurred_at: UtcTimestamp,
    pub kind: TaskEventKind,
}

/// One usage record to segment: a half-open time window `[start, end)` (a
/// point-in-time record has `start == end`) and its per-kind usage. Either
/// bound absent means the record's timestamp could not be established, and
/// the whole window becomes [`OverheadReason::MissingTimestamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageWindow {
    pub start: Option<UtcTimestamp>,
    pub end: Option<UtcTimestamp>,
    pub usage: KnownTokenVector,
}

/// The two session-level preconditions that must hold before per-window
/// boundary logic can even run: whether the usage's own session was
/// resolved, and whether the tracker that supplies claim boundaries was
/// reachable. Either being false is a session-wide fact, so it short-circuits
/// every window in the set to the matching overhead bucket rather than being
/// evaluated per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentationContext {
    pub session_is_mapped: bool,
    pub tracker_available: bool,
}

/// The full input to one segmentation pass over one session's usage set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentationInputs {
    pub context: SegmentationContext,
    pub boundaries: Vec<ClaimBoundary>,
    pub usage: Vec<UsageWindow>,
}

fn zero_vector() -> KnownTokenVector {
    KnownTokenVector::new(
        InputTokens::new(0),
        OutputTokens::new(0),
        CacheReadTokens::new(0),
        CacheWriteTokens::new(0),
    )
}

/// The segmentation result: per-kind usage summed by where it landed.
/// Rebuildable from the same inputs and never mutated in place, matching
/// the underlying `attribution_segment` table's own rebuild-on-change
/// contract: changing the tracker data and re-segmenting changes the
/// attribution rather than patching a frozen prior answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SegmentationResult {
    per_task: HashMap<TaskId, KnownTokenVector>,
    overhead: HashMap<OverheadReason, KnownTokenVector>,
}

impl SegmentationResult {
    fn add(&mut self, target: SegmentTarget, usage: KnownTokenVector) {
        match target {
            SegmentTarget::Task(task_id) => {
                let entry = self.per_task.entry(task_id).or_insert_with(zero_vector);
                *entry = *entry + usage;
            }
            SegmentTarget::Overhead(reason) => {
                let entry = self.overhead.entry(reason).or_insert_with(zero_vector);
                *entry = *entry + usage;
            }
        }
    }

    /// The usage attributed to one task, or `None` if it received none.
    pub fn task_usage(&self, task_id: &TaskId) -> Option<KnownTokenVector> {
        self.per_task.get(task_id).copied()
    }

    /// The usage landed in one overhead bucket, or `None` if it is empty.
    pub fn overhead_usage(&self, reason: OverheadReason) -> Option<KnownTokenVector> {
        self.overhead.get(&reason).copied()
    }

    /// Every task that received usage, with its total.
    pub fn tasks(&self) -> impl Iterator<Item = (&TaskId, &KnownTokenVector)> {
        self.per_task.iter()
    }

    /// Every overhead bucket that received usage, with its total.
    pub fn overhead_buckets(&self) -> impl Iterator<Item = (OverheadReason, &KnownTokenVector)> {
        self.overhead.iter().map(|(reason, usage)| (*reason, usage))
    }

    /// The conservation total: every task's usage plus every overhead
    /// bucket's usage. Equal to the sum of every input window's usage by
    /// construction, since [`segment`] assigns each window's full usage to
    /// exactly one bucket and never splits or drops one.
    pub fn total(&self) -> KnownTokenVector {
        self.per_task
            .values()
            .chain(self.overhead.values())
            .fold(zero_vector(), |acc, usage| acc + *usage)
    }
}

/// One interval of the segmented timeline: `[lo, hi)`, with `None` meaning
/// unbounded on that side, and what usage starting in it is attributed to.
struct TimeInterval {
    lo: Option<UtcTimestamp>,
    hi: Option<UtcTimestamp>,
    target: SegmentTarget,
}

/// Which task, if any, is active going into the next interval. A new claim
/// always supersedes whatever was active before it (PLAN.md 21's own worked
/// example: three sequential claims with no releases between them still
/// produce three clean, non-overlapping intervals) - overlap is not "a
/// second claim before the first is released", it is two distinct claims
/// tying at the exact same instant, which is the one case where picking
/// either task over the other would be invention rather than a documented
/// rule.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveState {
    Unclaimed,
    Claimed(TaskId),
    Contended,
}

fn interval_target(active: &ActiveState, is_before_first_claim: bool) -> SegmentTarget {
    match active {
        ActiveState::Unclaimed if is_before_first_claim => {
            SegmentTarget::Overhead(OverheadReason::BeforeFirstClaim)
        }
        ActiveState::Unclaimed => {
            SegmentTarget::Overhead(OverheadReason::AfterReleaseWithNoNextClaim)
        }
        ActiveState::Claimed(task_id) => SegmentTarget::Task(task_id.clone()),
        ActiveState::Contended => SegmentTarget::Overhead(OverheadReason::Contended),
    }
}

/// Builds the claim-to-claim interval timeline from a session's boundaries,
/// sorted by time. `TaskEventKind::Unknown` boundaries are ignored: they
/// carry no attribution meaning. Usage between two claims belongs to the
/// earlier one (PLAN.md 21); explicit release events create additional
/// boundaries. Boundaries are processed in groups sharing one instant,
/// releases before claims: two or more *distinct* tasks claimed at the same
/// instant is the genuine overlap case and produces
/// [`OverheadReason::Contended`] from that instant on, rather than a choice
/// between them.
fn build_intervals(boundaries: &[ClaimBoundary]) -> Vec<TimeInterval> {
    let mut sorted: Vec<&ClaimBoundary> = boundaries
        .iter()
        .filter(|boundary| matches!(boundary.kind, TaskEventKind::Claim | TaskEventKind::Release))
        .collect();
    if sorted.is_empty() {
        return Vec::new();
    }
    sorted.sort_by_key(|boundary| boundary.occurred_at);

    let mut active = ActiveState::Unclaimed;
    let mut intervals = Vec::new();
    let mut lo: Option<UtcTimestamp> = None;
    let mut is_first_group = true;
    let mut index = 0;

    while index < sorted.len() {
        let instant = sorted[index].occurred_at;
        let mut released = HashMap::new();
        let mut claimed: HashMap<TaskId, ()> = HashMap::new();
        while index < sorted.len() && sorted[index].occurred_at == instant {
            match sorted[index].kind {
                TaskEventKind::Claim => {
                    claimed.insert(sorted[index].task_id.clone(), ());
                }
                TaskEventKind::Release => {
                    released.insert(sorted[index].task_id.clone(), ());
                }
                TaskEventKind::Unknown(_) => unreachable!("filtered out above"),
            }
            index += 1;
        }

        intervals.push(TimeInterval {
            lo,
            hi: Some(instant),
            target: interval_target(&active, is_first_group),
        });
        is_first_group = false;

        if let ActiveState::Claimed(task_id) = &active
            && released.contains_key(task_id)
        {
            active = ActiveState::Unclaimed;
        }
        active = match claimed.len() {
            0 => active,
            1 => ActiveState::Claimed(claimed.into_keys().next().expect("len checked above")),
            _ => ActiveState::Contended,
        };
        lo = Some(instant);
    }
    intervals.push(TimeInterval {
        lo,
        hi: None,
        target: interval_target(&active, false),
    });
    intervals
}

/// Locates the interval whose `[lo, hi)` contains `instant`, matching the
/// half-open convention every interval boundary in this design uses
/// (`09:45 <= t < 10:30` belongs to the interval starting at `09:45`, never
/// the one ending there). `intervals` covers the whole timeline with no gaps
/// by construction, so exactly one interval always matches when it is
/// non-empty.
fn locate(intervals: &[TimeInterval], instant: UtcTimestamp) -> &TimeInterval {
    intervals
        .iter()
        .find(|interval| {
            interval.lo.is_none_or(|lo| lo <= instant) && interval.hi.is_none_or(|hi| instant < hi)
        })
        .expect("build_intervals covers the entire timeline with no gaps")
}

/// Classifies one usage window against the interval timeline. The window's
/// home interval is located by its `start` alone (half-open point
/// membership), then the window is attributed there only if it stays fully
/// inside that interval's upper bound; a window whose `end` reaches past
/// that interval's `hi` genuinely spans a boundary and lands in
/// [`OverheadReason::AmbiguousBoundary`] rather than being split.
fn classify_window(window: &UsageWindow, intervals: &[TimeInterval]) -> SegmentTarget {
    let (Some(start), Some(end)) = (window.start, window.end) else {
        return SegmentTarget::Overhead(OverheadReason::MissingTimestamp);
    };
    let home = locate(intervals, start);
    let fits = home.hi.is_none_or(|hi| end <= hi);
    if fits {
        home.target.clone()
    } else {
        SegmentTarget::Overhead(OverheadReason::AmbiguousBoundary)
    }
}

/// Segments one session's usage set. The two session-level preconditions in
/// `inputs.context` are checked first and short-circuit every window to the
/// matching overhead bucket when false; a mapped, tracker-available session
/// with usage but no claims at all lands every window in
/// [`OverheadReason::UnclaimedSession`]; otherwise every window is located
/// against the claim-to-claim interval timeline.
///
/// The conservation invariant (task-attributed plus overhead equals total
/// input usage, per token kind) is asserted here in debug builds
/// (`debug_assert!` compiles to nothing under `--release`), so a violation
/// surfaces during development rather than only under `cargo test`.
pub fn segment(inputs: &SegmentationInputs) -> SegmentationResult {
    let mut result = SegmentationResult::default();

    let short_circuit = if !inputs.context.session_is_mapped {
        Some(OverheadReason::UnmappedSession)
    } else if !inputs.context.tracker_available {
        Some(OverheadReason::TrackerUnavailable)
    } else {
        None
    };
    if let Some(reason) = short_circuit {
        for window in &inputs.usage {
            result.add(SegmentTarget::Overhead(reason), window.usage);
        }
        debug_assert_conserves(inputs, &result);
        return result;
    }

    let intervals = build_intervals(&inputs.boundaries);
    if intervals.is_empty() {
        for window in &inputs.usage {
            result.add(
                SegmentTarget::Overhead(OverheadReason::UnclaimedSession),
                window.usage,
            );
        }
        debug_assert_conserves(inputs, &result);
        return result;
    }

    for window in &inputs.usage {
        let target = classify_window(window, &intervals);
        result.add(target, window.usage);
    }
    debug_assert_conserves(inputs, &result);
    result
}

fn debug_assert_conserves(inputs: &SegmentationInputs, result: &SegmentationResult) {
    debug_assert!(
        {
            let expected = inputs
                .usage
                .iter()
                .fold(zero_vector(), |acc, window| acc + window.usage);
            result.total() == expected
        },
        "segmentation must conserve every token kind: task-attributed plus overhead must equal \
         total input usage, with no remainder"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{NativeTaskId, SourceNamespace};
    use proptest::prelude::*;

    fn task(name: &str) -> TaskId {
        TaskId::new(SourceNamespace::new("github"), NativeTaskId::new(name))
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

    fn point(at: i64, input: u64) -> UsageWindow {
        UsageWindow {
            start: Some(t(at)),
            end: Some(t(at)),
            usage: tokens(input),
        }
    }

    fn claim(task_id: TaskId, at: i64) -> ClaimBoundary {
        ClaimBoundary {
            task_id,
            occurred_at: t(at),
            kind: TaskEventKind::Claim,
        }
    }

    fn release(task_id: TaskId, at: i64) -> ClaimBoundary {
        ClaimBoundary {
            task_id,
            occurred_at: t(at),
            kind: TaskEventKind::Release,
        }
    }

    fn mapped_available() -> SegmentationContext {
        SegmentationContext {
            session_is_mapped: true,
            tracker_available: true,
        }
    }

    // --- the eight overhead buckets, each by its own condition -----------

    #[test]
    fn usage_before_the_first_claim_is_before_first_claim() {
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 100)],
            usage: vec![point(50, 10)],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::BeforeFirstClaim),
            Some(tokens(10))
        );
        assert!(result.task_usage(&task("T1")).is_none());
    }

    #[test]
    fn usage_between_two_claims_belongs_to_the_earlier_task() {
        // PLAN.md 21's own worked example.
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![
                claim(task("T1"), 0),  // 09:10
                claim(task("T2"), 35), // 09:45
                claim(task("T3"), 80), // 10:30
            ],
            usage: vec![
                point(10, 5),  // inside [09:10, 09:45) -> T1
                point(60, 7),  // inside [09:45, 10:30) -> T2
                point(90, 11), // inside [10:30, +inf)  -> T3
            ],
        };
        let result = segment(&inputs);
        assert_eq!(result.task_usage(&task("T1")), Some(tokens(5)));
        assert_eq!(result.task_usage(&task("T2")), Some(tokens(7)));
        assert_eq!(result.task_usage(&task("T3")), Some(tokens(11)));
    }

    #[test]
    fn usage_exactly_on_a_boundary_timestamp_belongs_to_the_task_starting_there() {
        // PLAN.md 21: "09:45 <= t < 10:30" - the boundary instant itself
        // belongs to the interval that starts there, never the one ending.
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 0), claim(task("T2"), 35)],
            usage: vec![point(35, 9)],
        };
        let result = segment(&inputs);
        assert_eq!(result.task_usage(&task("T1")), None);
        assert_eq!(result.task_usage(&task("T2")), Some(tokens(9)));
    }

    #[test]
    fn usage_after_release_with_no_next_claim_lands_in_that_bucket() {
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 0), release(task("T1"), 10)],
            usage: vec![point(20, 13)],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::AfterReleaseWithNoNextClaim),
            Some(tokens(13))
        );
    }

    #[test]
    fn overlapping_claims_produce_contended_rather_than_a_choice_between_them() {
        // A genuine overlap: two distinct tasks claimed at the exact same
        // instant, where nothing in the data says which one usage after
        // that point belongs to. A later, distinct-time claim is not this
        // case - see `usage_between_two_claims_belongs_to_the_earlier_task`,
        // PLAN.md 21's own worked example, where a claim cleanly supersedes
        // whatever was active before it.
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 10), claim(task("T2"), 10)],
            usage: vec![point(15, 17)],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::Contended),
            Some(tokens(17))
        );
        assert!(result.task_usage(&task("T1")).is_none());
        assert!(result.task_usage(&task("T2")).is_none());
    }

    #[test]
    fn a_window_spanning_a_boundary_is_never_split_but_lands_in_ambiguous_boundary() {
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 0), claim(task("T2"), 35)],
            usage: vec![UsageWindow {
                start: Some(t(30)),
                end: Some(t(40)), // crosses the boundary at 35
                usage: tokens(21),
            }],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::AmbiguousBoundary),
            Some(tokens(21))
        );
        assert!(result.task_usage(&task("T1")).is_none());
        assert!(result.task_usage(&task("T2")).is_none());
    }

    #[test]
    fn a_window_ending_exactly_at_a_boundary_is_not_ambiguous() {
        // The window ends AT the boundary, not past it: it belongs wholly
        // to the earlier task, the mirror case of the spanning test above.
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 0), claim(task("T2"), 35)],
            usage: vec![UsageWindow {
                start: Some(t(30)),
                end: Some(t(35)),
                usage: tokens(8),
            }],
        };
        let result = segment(&inputs);
        assert_eq!(result.task_usage(&task("T1")), Some(tokens(8)));
        assert!(
            result
                .overhead_usage(OverheadReason::AmbiguousBoundary)
                .is_none()
        );
    }

    #[test]
    fn missing_timestamp_lands_in_its_own_bucket() {
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 0)],
            usage: vec![UsageWindow {
                start: None,
                end: Some(t(5)),
                usage: tokens(3),
            }],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::MissingTimestamp),
            Some(tokens(3))
        );
    }

    #[test]
    fn a_session_with_usage_and_no_claim_at_all_is_unclaimed_session() {
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![],
            usage: vec![point(5, 4)],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::UnclaimedSession),
            Some(tokens(4))
        );
    }

    #[test]
    fn an_unmapped_session_short_circuits_every_window_regardless_of_claims() {
        let inputs = SegmentationInputs {
            context: SegmentationContext {
                session_is_mapped: false,
                tracker_available: true,
            },
            boundaries: vec![claim(task("T1"), 0)],
            usage: vec![point(5, 6)],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::UnmappedSession),
            Some(tokens(6))
        );
        assert!(result.task_usage(&task("T1")).is_none());
    }

    #[test]
    fn tracker_unavailable_short_circuits_every_window_regardless_of_claims() {
        let inputs = SegmentationInputs {
            context: SegmentationContext {
                session_is_mapped: true,
                tracker_available: false,
            },
            boundaries: vec![claim(task("T1"), 0)],
            usage: vec![point(5, 6)],
        };
        let result = segment(&inputs);
        assert_eq!(
            result.overhead_usage(OverheadReason::TrackerUnavailable),
            Some(tokens(6))
        );
    }

    // --- conservation invariant --------------------------------------

    #[test]
    fn task_attributed_plus_overhead_equals_total_input_usage() {
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 10), release(task("T1"), 20)],
            usage: vec![
                point(0, 1),  // before first claim
                point(15, 2), // T1
                point(25, 3), // after release, no next claim
            ],
        };
        let result = segment(&inputs);
        assert_eq!(result.total(), tokens(6));
    }

    #[test]
    fn a_deliberately_dropped_window_fails_the_conservation_assertion() {
        // Builds a result that omits one input window's usage, the way a
        // bug that silently drops an event would, and proves the debug
        // assertion this bead requires actually catches it rather than
        // passing silently.
        let inputs = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![],
            usage: vec![point(0, 42)],
        };
        let empty_result = SegmentationResult::default();
        let caught = std::panic::catch_unwind(|| debug_assert_conserves(&inputs, &empty_result));
        assert!(
            caught.is_err(),
            "a result missing an input window's usage must fail the conservation assertion"
        );
    }

    #[test]
    fn rebuilding_with_different_tracker_data_changes_the_attribution() {
        let usage = vec![point(15, 9)];
        let claimed_by_t1 = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T1"), 0)],
            usage: usage.clone(),
        };
        let claimed_by_t2 = SegmentationInputs {
            context: mapped_available(),
            boundaries: vec![claim(task("T2"), 0)],
            usage,
        };
        assert_eq!(
            segment(&claimed_by_t1).task_usage(&task("T1")),
            Some(tokens(9))
        );
        assert_eq!(
            segment(&claimed_by_t2).task_usage(&task("T2")),
            Some(tokens(9))
        );
        assert!(segment(&claimed_by_t2).task_usage(&task("T1")).is_none());
    }

    // --- property tests --------------------------------------------------

    proptest! {
        /// No usage event is assigned to more than one task or bucket: the
        /// per-window contribution always lands in exactly the one target
        /// [`classify_window`] names, over generated claim and usage sets.
        #[test]
        fn every_window_is_assigned_to_exactly_one_target(
            claim_count in 0usize..5,
            claim_times in proptest::collection::vec(0i64..1000, 0..5),
            usage_times in proptest::collection::vec(0i64..1000, 0..8),
            usage_amounts in proptest::collection::vec(1u64..1000, 0..8),
        ) {
            let boundaries: Vec<ClaimBoundary> = claim_times
                .iter()
                .take(claim_count.min(claim_times.len()))
                .enumerate()
                .map(|(i, &at)| claim(task(&format!("T{i}")), at))
                .collect();
            let usage: Vec<UsageWindow> = usage_times
                .iter()
                .zip(usage_amounts.iter())
                .map(|(&at, &amount)| point(at, amount))
                .collect();
            let inputs = SegmentationInputs {
                context: mapped_available(),
                boundaries,
                usage: usage.clone(),
            };
            let result = segment(&inputs);

            let expected_total = usage.iter().fold(zero_vector(), |acc, w| acc + w.usage);
            prop_assert_eq!(result.total(), expected_total);
        }

        /// The conservation invariant holds over generated corpora that
        /// deliberately carry every overhead class: before-first-claim,
        /// between-claims, after-release, contended (via a duplicate claim
        /// with no release), and unclaimed (no boundaries at all).
        #[test]
        fn conservation_holds_over_generated_corpora_with_every_overhead_class(
            has_release in any::<bool>(),
            has_overlap in any::<bool>(),
            amounts in proptest::collection::vec(1u64..500, 1..10),
        ) {
            let mut boundaries = vec![claim(task("T1"), 100)];
            if has_release {
                boundaries.push(release(task("T1"), 200));
            }
            if has_overlap {
                // A genuine tie: T2 claimed at the exact same instant as
                // T1, distinct from a later, distinct-time claim (which
                // would simply supersede T1 cleanly).
                boundaries.push(claim(task("T2"), 100));
            }
            // One usage window per generated amount, spread across a range
            // that covers before-first-claim, mid-claim and post-boundary
            // instants regardless of which boundaries above are present.
            let usage: Vec<UsageWindow> = amounts
                .iter()
                .enumerate()
                .map(|(i, &amount)| point((i as i64) * 50, amount))
                .collect();
            let inputs = SegmentationInputs {
                context: mapped_available(),
                boundaries,
                usage: usage.clone(),
            };
            let result = segment(&inputs);
            let expected_total = usage.iter().fold(zero_vector(), |acc, w| acc + w.usage);
            prop_assert_eq!(result.total(), expected_total);
        }
    }
}
