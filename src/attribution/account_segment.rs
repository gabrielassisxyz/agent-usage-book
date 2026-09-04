//! Marker-interval account segmentation: usage split into per-account
//! intervals from a session's account-marker timeline (`aub-mgv.1`, PLAN.md
//! 12.6, 19.2, 34.17).
//!
//! The alternative this design rejects is one account column per session,
//! because accounts switch inside a session and a fixed column makes the
//! switch invisible. Instead markers form a timeline: a marker names the
//! account active from its own timestamp forward, until the next marker
//! supersedes it. Usage before any marker exists has no marker to justify an
//! account assignment, so it lands in the explicit unknown-account bucket
//! rather than being guessed from "the currently active profile" (PLAN.md
//! 19.2).
//!
//! Ordering is the subtle part: two markers can share the source's timestamp
//! resolution, and account attribution turns on exactly that ordering. So
//! markers are ordered primarily by timestamp and, only when timestamps tie,
//! by the source-provided ordering key where the source gave one; a marker
//! with no ordering key sorts before one that has a key at the same
//! timestamp, and any tie still remaining after both falls back to input
//! order (the order the caller passed the markers in, which for the store's
//! own callers is insertion order). That final fallback is what makes
//! duplicate and out-of-order input markers produce a deterministic result:
//! the sort is total, so no case is left for an "ambiguous" or "contended"
//! bucket the way two same-instant task claims need one.
//!
//! Usage events in this system carry one timestamp each, not a window, so
//! (unlike task attribution's claim-to-claim segmentation) there is no case
//! of a single usage record spanning two marker intervals: a usage
//! timestamp falls into exactly one interval.

use std::collections::HashMap;

use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
};

/// One account marker as the segmentation algorithm sees it: the account it
/// names, when it was observed, and the source's own ordering key if the
/// source provided one. The store maps its persisted `SessionAccountMarker`
/// rows into this before calling [`segment`]; this type carries no store
/// identity because the algorithm is pure over its inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMarkerBoundary {
    pub logical_account: String,
    pub observed_at: UtcTimestamp,
    pub source_ordering_key: Option<i64>,
}

/// One usage record to segment: a single timestamp and its per-kind usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsageEvent {
    pub occurred_at: UtcTimestamp,
    pub usage: KnownTokenVector,
}

/// Where one usage event's tokens land: a named account, or the explicit
/// unknown-account bucket for usage no marker can justify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountSegmentTarget {
    Account(String),
    UnknownAccount,
}

fn zero_vector() -> KnownTokenVector {
    KnownTokenVector::new(
        InputTokens::new(0),
        OutputTokens::new(0),
        CacheReadTokens::new(0),
        CacheWriteTokens::new(0),
    )
}

/// The full input to one segmentation pass over one session's usage set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSegmentationInputs {
    pub markers: Vec<AccountMarkerBoundary>,
    pub usage: Vec<AccountUsageEvent>,
}

/// The segmentation result: per-kind usage summed by which account it landed
/// on, plus the unknown-account bucket. Rebuildable from the same inputs and
/// never mutated in place: re-running [`segment`] after the marker set
/// changes is expected to change the attribution, matching the
/// `account_attribution_segment` table's rebuild-on-change contract rather
/// than storing attribution as an immutable fact.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountSegmentationResult {
    per_account: HashMap<String, KnownTokenVector>,
    unknown_account: Option<KnownTokenVector>,
}

impl AccountSegmentationResult {
    fn add(&mut self, target: AccountSegmentTarget, usage: KnownTokenVector) {
        match target {
            AccountSegmentTarget::Account(logical_account) => {
                let entry = self
                    .per_account
                    .entry(logical_account)
                    .or_insert_with(zero_vector);
                *entry = *entry + usage;
            }
            AccountSegmentTarget::UnknownAccount => {
                let entry = self.unknown_account.get_or_insert_with(zero_vector);
                *entry = *entry + usage;
            }
        }
    }

    /// The usage attributed to one account, or `None` if it received none.
    pub fn account_usage(&self, logical_account: &str) -> Option<KnownTokenVector> {
        self.per_account.get(logical_account).copied()
    }

    /// The usage landed in the unknown-account bucket, or `None` if it is empty.
    pub fn unknown_account_usage(&self) -> Option<KnownTokenVector> {
        self.unknown_account
    }

    /// Every account that received usage, with its total.
    pub fn accounts(&self) -> impl Iterator<Item = (&str, &KnownTokenVector)> {
        self.per_account
            .iter()
            .map(|(account, usage)| (account.as_str(), usage))
    }

    /// The conservation total: every account's usage plus the unknown-account
    /// bucket. Equal to the sum of every input event's usage by construction,
    /// since [`segment`] assigns each event's full usage to exactly one
    /// target and never splits or drops one.
    pub fn total(&self) -> KnownTokenVector {
        self.per_account
            .values()
            .chain(self.unknown_account.iter())
            .fold(zero_vector(), |acc, usage| acc + *usage)
    }
}

/// One interval of the marker timeline: `[lo, hi)`, with `hi = None` meaning
/// unbounded (the last marker applies forward to the end of the session),
/// and the account active during it.
struct MarkerInterval {
    lo: UtcTimestamp,
    hi: Option<UtcTimestamp>,
    logical_account: String,
}

/// Sorts markers into their total, deterministic order: primarily by
/// timestamp; when two markers share a timestamp, a marker with no
/// source-provided ordering key sorts before one that has a key, and two
/// markers that both have keys sort by that key; any tie still remaining
/// (no key on either side, or equal keys, which is what "duplicate markers"
/// looks like) falls back to input order, which [`Vec::sort_by`]'s stability
/// preserves. Documenting this here is the tie-breaking rule the interval
/// timestamp semantics need: usage exactly on a marker's own timestamp
/// belongs to the interval that starts there (see [`build_intervals`]), so
/// which marker starts an interval at a shared timestamp is decided by this
/// order and nothing else.
fn sorted_markers(markers: &[AccountMarkerBoundary]) -> Vec<&AccountMarkerBoundary> {
    let mut sorted: Vec<&AccountMarkerBoundary> = markers.iter().collect();
    sorted.sort_by(|a, b| {
        a.observed_at
            .cmp(&b.observed_at)
            .then_with(|| a.source_ordering_key.cmp(&b.source_ordering_key))
    });
    sorted
}

/// Builds the marker-to-marker interval timeline: marker `i`'s account
/// applies over `[marker_i.observed_at, marker_{i+1}.observed_at)`, and the
/// last marker's account applies unbounded forward. Empty input (no markers
/// at all, covering both the "no marker" and the empty-session cases)
/// produces no intervals, and every usage event then falls through to the
/// unknown-account bucket in [`segment`].
fn build_intervals(markers: &[AccountMarkerBoundary]) -> Vec<MarkerInterval> {
    let sorted = sorted_markers(markers);
    let mut intervals = Vec::with_capacity(sorted.len());
    for (index, marker) in sorted.iter().enumerate() {
        let hi = sorted.get(index + 1).map(|next| next.observed_at);
        intervals.push(MarkerInterval {
            lo: marker.observed_at,
            hi,
            logical_account: marker.logical_account.clone(),
        });
    }
    intervals
}

/// Locates the interval whose `[lo, hi)` contains `instant`, or `None` when
/// `instant` precedes every marker (or no marker exists at all). Intervals
/// are contiguous and gapless from the first marker forward by construction,
/// so at most one interval ever matches.
fn locate(intervals: &[MarkerInterval], instant: UtcTimestamp) -> Option<&MarkerInterval> {
    intervals
        .iter()
        .find(|interval| interval.lo <= instant && interval.hi.is_none_or(|hi| instant < hi))
}

/// Segments one session's usage set against its account-marker timeline.
/// Every usage event is placed by its own timestamp: on or after a marker's
/// timestamp and before the next one, it belongs to that marker's account;
/// strictly before every marker (including when there is no marker at all),
/// it lands in [`AccountSegmentTarget::UnknownAccount`].
///
/// The conservation invariant (per-account usage plus the unknown-account
/// bucket equals total input usage, per token kind) is asserted here in
/// debug builds (`debug_assert!` compiles to nothing under `--release`), so
/// a violation surfaces during development rather than only under `cargo
/// test`.
pub fn segment(inputs: &AccountSegmentationInputs) -> AccountSegmentationResult {
    let mut result = AccountSegmentationResult::default();
    let intervals = build_intervals(&inputs.markers);

    for event in &inputs.usage {
        let target = match locate(&intervals, event.occurred_at) {
            Some(interval) => AccountSegmentTarget::Account(interval.logical_account.clone()),
            None => AccountSegmentTarget::UnknownAccount,
        };
        result.add(target, event.usage);
    }

    debug_assert_conserves(inputs, &result);
    result
}

fn debug_assert_conserves(inputs: &AccountSegmentationInputs, result: &AccountSegmentationResult) {
    debug_assert!(
        {
            let expected = inputs
                .usage
                .iter()
                .fold(zero_vector(), |acc, event| acc + event.usage);
            result.total() == expected
        },
        "account segmentation must conserve every token kind: per-account usage plus the \
         unknown-account bucket must equal total input usage, with no remainder"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    fn event(at: i64, input: u64) -> AccountUsageEvent {
        AccountUsageEvent {
            occurred_at: t(at),
            usage: tokens(input),
        }
    }

    fn marker(account: &str, at: i64, ordering_key: Option<i64>) -> AccountMarkerBoundary {
        AccountMarkerBoundary {
            logical_account: account.to_owned(),
            observed_at: t(at),
            source_ordering_key: ordering_key,
        }
    }

    // --- the six account-segment cases from the design, plus the
    // empty-session case this bead adds ----------------------------------

    #[test]
    fn one_account_for_a_whole_session() {
        let inputs = AccountSegmentationInputs {
            markers: vec![marker("account-a", 0, None)],
            usage: vec![event(10, 5), event(20, 7)],
        };
        let result = segment(&inputs);
        assert_eq!(result.account_usage("account-a"), Some(tokens(12)));
        assert!(result.unknown_account_usage().is_none());
    }

    #[test]
    fn a_mid_session_switch_splits_usage_by_the_marker_that_precedes_it() {
        // PLAN.md 19.2's own worked example: 10:00 account-a, 10:40 account-b.
        let inputs = AccountSegmentationInputs {
            markers: vec![marker("account-a", 0, None), marker("account-b", 40, None)],
            usage: vec![
                event(10, 3),  // before 40 -> account-a
                event(39, 4),  // still before 40 -> account-a
                event(60, 11), // after 40 -> account-b
            ],
        };
        let result = segment(&inputs);
        assert_eq!(result.account_usage("account-a"), Some(tokens(7)));
        assert_eq!(result.account_usage("account-b"), Some(tokens(11)));
    }

    #[test]
    fn usage_exactly_on_a_marker_timestamp_belongs_to_the_marker_starting_there() {
        // Inclusive-start rule, matching the half-open convention used
        // throughout this design: "[lo, hi)" puts the boundary instant with
        // the interval that starts there, never the one ending.
        let inputs = AccountSegmentationInputs {
            markers: vec![marker("account-a", 0, None), marker("account-b", 40, None)],
            usage: vec![event(40, 9)],
        };
        let result = segment(&inputs);
        assert_eq!(result.account_usage("account-a"), None);
        assert_eq!(result.account_usage("account-b"), Some(tokens(9)));
    }

    #[test]
    fn no_marker_at_all_lands_every_event_in_unknown_account() {
        let inputs = AccountSegmentationInputs {
            markers: vec![],
            usage: vec![event(5, 4), event(9, 6)],
        };
        let result = segment(&inputs);
        assert_eq!(result.unknown_account_usage(), Some(tokens(10)));
        assert!(result.accounts().next().is_none());
    }

    #[test]
    fn duplicate_markers_naming_the_same_account_produce_the_same_result_as_one() {
        let single = AccountSegmentationInputs {
            markers: vec![marker("account-a", 0, None)],
            usage: vec![event(10, 5)],
        };
        let duplicated = AccountSegmentationInputs {
            markers: vec![marker("account-a", 0, None), marker("account-a", 0, None)],
            usage: vec![event(10, 5)],
        };
        assert_eq!(
            segment(&single).account_usage("account-a"),
            segment(&duplicated).account_usage("account-a")
        );
        assert_eq!(
            segment(&duplicated).account_usage("account-a"),
            Some(tokens(5))
        );
    }

    #[test]
    fn out_of_order_input_markers_produce_the_same_result_as_sorted_input() {
        let sorted_input = AccountSegmentationInputs {
            markers: vec![
                marker("account-a", 0, None),
                marker("account-b", 40, None),
                marker("account-c", 80, None),
            ],
            usage: vec![event(10, 1), event(50, 2), event(90, 3)],
        };
        let reversed_input = AccountSegmentationInputs {
            markers: vec![
                marker("account-c", 80, None),
                marker("account-b", 40, None),
                marker("account-a", 0, None),
            ],
            usage: vec![event(10, 1), event(50, 2), event(90, 3)],
        };
        assert_eq!(segment(&sorted_input), segment(&reversed_input));
        let result = segment(&reversed_input);
        assert_eq!(result.account_usage("account-a"), Some(tokens(1)));
        assert_eq!(result.account_usage("account-b"), Some(tokens(2)));
        assert_eq!(result.account_usage("account-c"), Some(tokens(3)));
    }

    #[test]
    fn an_empty_session_produces_an_empty_result() {
        let inputs = AccountSegmentationInputs {
            markers: vec![],
            usage: vec![],
        };
        let result = segment(&inputs);
        assert_eq!(result.total(), tokens(0));
        assert!(result.unknown_account_usage().is_none());
        assert!(result.accounts().next().is_none());
    }

    // --- ordering: source sequence where available, timestamp otherwise --

    #[test]
    fn source_ordering_key_decides_which_marker_wins_at_a_shared_timestamp() {
        // Two markers tie on timestamp: sorted order places one immediately
        // ahead of the other, so it gets a zero-width interval and is
        // instantly superseded. Swapping which marker carries the lower key
        // swaps which account usage at and after the shared instant lands
        // on, proving the ordering key (and not e.g. insertion order) is
        // what decided it.
        let key_one_is_account_a = AccountSegmentationInputs {
            markers: vec![
                marker("account-a", 50, Some(1)),
                marker("account-b", 50, Some(2)),
            ],
            usage: vec![event(50, 9)],
        };
        let key_one_is_account_b = AccountSegmentationInputs {
            markers: vec![
                marker("account-a", 50, Some(2)),
                marker("account-b", 50, Some(1)),
            ],
            usage: vec![event(50, 9)],
        };
        assert_eq!(
            segment(&key_one_is_account_a).account_usage("account-b"),
            Some(tokens(9))
        );
        assert!(
            segment(&key_one_is_account_a)
                .account_usage("account-a")
                .is_none()
        );
        assert_eq!(
            segment(&key_one_is_account_b).account_usage("account-a"),
            Some(tokens(9))
        );
        assert!(
            segment(&key_one_is_account_b)
                .account_usage("account-b")
                .is_none()
        );
    }

    #[test]
    fn a_marker_with_no_ordering_key_sorts_before_one_with_a_key_at_the_same_timestamp() {
        // `None` sorts before `Some(_)`, so "unkeyed" is superseded
        // immediately by "keyed" and never receives usage.
        let inputs = AccountSegmentationInputs {
            markers: vec![marker("keyed", 50, Some(0)), marker("unkeyed", 50, None)],
            usage: vec![event(50, 1), event(51, 2)],
        };
        let result = segment(&inputs);
        assert!(result.account_usage("unkeyed").is_none());
        assert_eq!(result.account_usage("keyed"), Some(tokens(3)));
    }

    // --- rebuild: changing the marker set changes the attribution --------

    #[test]
    fn rebuilding_with_a_different_marker_set_changes_the_attribution() {
        let usage = vec![event(15, 9)];
        let under_account_a = AccountSegmentationInputs {
            markers: vec![marker("account-a", 0, None)],
            usage: usage.clone(),
        };
        let under_account_b = AccountSegmentationInputs {
            markers: vec![marker("account-b", 0, None)],
            usage,
        };
        assert_eq!(
            segment(&under_account_a).account_usage("account-a"),
            Some(tokens(9))
        );
        assert_eq!(
            segment(&under_account_b).account_usage("account-b"),
            Some(tokens(9))
        );
        assert!(
            segment(&under_account_b)
                .account_usage("account-a")
                .is_none()
        );
    }

    // --- conservation invariant -------------------------------------------

    #[test]
    fn per_account_usage_plus_unknown_account_equals_total_input_usage() {
        let inputs = AccountSegmentationInputs {
            markers: vec![marker("account-a", 10, None)],
            usage: vec![
                event(0, 1),  // before the marker -> unknown
                event(15, 2), // account-a
            ],
        };
        let result = segment(&inputs);
        assert_eq!(result.total(), tokens(3));
    }

    #[test]
    fn a_deliberately_dropped_event_fails_the_conservation_assertion() {
        let inputs = AccountSegmentationInputs {
            markers: vec![],
            usage: vec![event(0, 42)],
        };
        let empty_result = AccountSegmentationResult::default();
        let caught = std::panic::catch_unwind(|| debug_assert_conserves(&inputs, &empty_result));
        assert!(
            caught.is_err(),
            "a result missing an input event's usage must fail the conservation assertion"
        );
    }

    // --- property test: every event lands in exactly one target ----------

    proptest! {
        /// No usage event is assigned to more than one account or bucket:
        /// the per-event contribution always lands in exactly the one
        /// target [`segment`] names, over generated marker and usage sets.
        #[test]
        fn every_event_is_assigned_to_exactly_one_target(
            marker_count in 0usize..5,
            marker_times in proptest::collection::vec(0i64..1000, 0..5),
            usage_times in proptest::collection::vec(0i64..1000, 0..8),
            usage_amounts in proptest::collection::vec(1u64..1000, 0..8),
        ) {
            let markers: Vec<AccountMarkerBoundary> = marker_times
                .iter()
                .take(marker_count.min(marker_times.len()))
                .enumerate()
                .map(|(i, &at)| marker(&format!("account-{i}"), at, None))
                .collect();
            let usage: Vec<AccountUsageEvent> = usage_times
                .iter()
                .zip(usage_amounts.iter())
                .map(|(&at, &amount)| event(at, amount))
                .collect();
            let inputs = AccountSegmentationInputs { markers, usage: usage.clone() };
            let result = segment(&inputs);

            let expected_total = usage.iter().fold(zero_vector(), |acc, e| acc + e.usage);
            prop_assert_eq!(result.total(), expected_total);
        }
    }
}
