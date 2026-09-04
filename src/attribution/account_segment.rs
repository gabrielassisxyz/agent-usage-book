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
use std::fmt;
use std::str::FromStr;

use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
};
use crate::error::Error;

/// The five account attribution evidence classes in descending confidence order
/// (PLAN.md 19.2, aub-mgv.2):
/// 1. explicit session and account marker from launcher or hook;
/// 2. explicit provider or account identity returned during that session;
/// 3. configured credential-source identity with validated mapping;
/// 4. conservative temporal inference;
/// 5. unattributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccountEvidenceClass {
    /// Explicit session and account marker from launcher or hook (Rank 1).
    ExplicitLauncherOrHook = 1,
    /// Explicit provider or account identity returned during that session (Rank 2).
    ExplicitProviderIdentity = 2,
    /// Configured credential-source identity with validated mapping (Rank 3).
    ConfiguredCredentialMapping = 3,
    /// Conservative temporal inference (Rank 4).
    ConservativeTemporalInference = 4,
    /// Unattributed (Rank 5).
    Unattributed = 5,
}

impl AccountEvidenceClass {
    /// The five evidence classes in precedence order (highest confidence first).
    pub const ALL: [Self; 5] = [
        Self::ExplicitLauncherOrHook,
        Self::ExplicitProviderIdentity,
        Self::ConfiguredCredentialMapping,
        Self::ConservativeTemporalInference,
        Self::Unattributed,
    ];

    /// Precedence rank from 1 (highest confidence) to 5 (unattributed).
    pub const fn rank(self) -> u8 {
        match self {
            Self::ExplicitLauncherOrHook => 1,
            Self::ExplicitProviderIdentity => 2,
            Self::ConfiguredCredentialMapping => 3,
            Self::ConservativeTemporalInference => 4,
            Self::Unattributed => 5,
        }
    }

    /// Returns true if `self` has higher precedence than `other`.
    pub const fn takes_precedence_over(self, other: Self) -> bool {
        self.rank() < other.rank()
    }

    /// True when this evidence class represents conservative temporal inference.
    /// Sufficient for `aub-c0b.7` to reject inferred attribution without reconstructing provenance.
    pub const fn is_inferred(self) -> bool {
        matches!(self, Self::ConservativeTemporalInference)
    }

    /// True when this class represents explicit evidence (ranks 1 and 2).
    pub const fn is_explicit(self) -> bool {
        matches!(
            self,
            Self::ExplicitLauncherOrHook | Self::ExplicitProviderIdentity
        )
    }

    /// True when this class is eligible for passive calibration (`aub-c0b.7`).
    /// Inferred attribution and unattributed usage are ineligible.
    pub const fn is_eligible_for_passive_calibration(self) -> bool {
        !self.is_inferred() && !matches!(self, Self::Unattributed)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitLauncherOrHook => "explicit_launcher_or_hook",
            Self::ExplicitProviderIdentity => "explicit_provider_identity",
            Self::ConfiguredCredentialMapping => "configured_credential_mapping",
            Self::ConservativeTemporalInference => "conservative_temporal_inference",
            Self::Unattributed => "unattributed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "explicit_launcher_or_hook" | "launcher_or_hook" => Some(Self::ExplicitLauncherOrHook),
            "explicit_provider_identity" | "provider_identity" => {
                Some(Self::ExplicitProviderIdentity)
            }
            "configured_credential_mapping" | "credential_mapping" => {
                Some(Self::ConfiguredCredentialMapping)
            }
            "conservative_temporal_inference" | "temporal_inference" | "inferred" => {
                Some(Self::ConservativeTemporalInference)
            }
            "unattributed" => Some(Self::Unattributed),
            _ => None,
        }
    }
}

impl fmt::Display for AccountEvidenceClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AccountEvidenceClass {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| Error::Store(format!("unknown account evidence class: '{s}'")))
    }
}

/// One account marker as the segmentation algorithm sees it: the account it
/// names, when it was observed, the source's own ordering key if the source
/// provided one, its evidence class, and whether its credential mapping has
/// been validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMarkerBoundary {
    pub logical_account: String,
    pub observed_at: UtcTimestamp,
    pub source_ordering_key: Option<i64>,
    pub evidence_class: AccountEvidenceClass,
    pub mapping_validated: bool,
}

impl AccountMarkerBoundary {
    pub fn new(
        logical_account: impl Into<String>,
        observed_at: UtcTimestamp,
        source_ordering_key: Option<i64>,
        evidence_class: AccountEvidenceClass,
        mapping_validated: bool,
    ) -> Self {
        Self {
            logical_account: logical_account.into(),
            observed_at,
            source_ordering_key,
            evidence_class,
            mapping_validated,
        }
    }

    /// Creates an explicit launcher or hook marker boundary.
    pub fn explicit(
        logical_account: impl Into<String>,
        observed_at: UtcTimestamp,
        source_ordering_key: Option<i64>,
    ) -> Self {
        Self::new(
            logical_account,
            observed_at,
            source_ordering_key,
            AccountEvidenceClass::ExplicitLauncherOrHook,
            true,
        )
    }

    /// Creates a conservative temporal inference marker boundary.
    pub fn inferred(
        logical_account: impl Into<String>,
        observed_at: UtcTimestamp,
        source_ordering_key: Option<i64>,
    ) -> Self {
        Self::new(
            logical_account,
            observed_at,
            source_ordering_key,
            AccountEvidenceClass::ConservativeTemporalInference,
            true,
        )
    }

    /// The effective evidence class of this marker.
    ///
    /// Where a credential-source identity is used, the mapping must be
    /// validated, and an unvalidated mapping falls through to the next
    /// class (`ConservativeTemporalInference`).
    pub fn effective_evidence_class(&self) -> AccountEvidenceClass {
        match self.evidence_class {
            AccountEvidenceClass::ConfiguredCredentialMapping if !self.mapping_validated => {
                AccountEvidenceClass::ConservativeTemporalInference
            }
            other => other,
        }
    }
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
    per_account: HashMap<String, (KnownTokenVector, AccountEvidenceClass)>,
    unknown_account: Option<KnownTokenVector>,
}

impl AccountSegmentationResult {
    fn add(
        &mut self,
        target: AccountSegmentTarget,
        usage: KnownTokenVector,
        evidence_class: AccountEvidenceClass,
    ) {
        match target {
            AccountSegmentTarget::Account(logical_account) => {
                let entry = self
                    .per_account
                    .entry(logical_account)
                    .or_insert_with(|| (zero_vector(), evidence_class));
                entry.0 = entry.0 + usage;
                if evidence_class.takes_precedence_over(entry.1) {
                    entry.1 = evidence_class;
                }
            }
            AccountSegmentTarget::UnknownAccount => {
                let entry = self.unknown_account.get_or_insert_with(zero_vector);
                *entry = *entry + usage;
            }
        }
    }

    /// The usage attributed to one account, or `None` if it received none.
    pub fn account_usage(&self, logical_account: &str) -> Option<KnownTokenVector> {
        self.per_account
            .get(logical_account)
            .map(|(usage, _)| *usage)
    }

    /// The evidence class justifying the usage attributed to one account,
    /// or `None` if the account received no attribution.
    pub fn account_evidence_class(&self, logical_account: &str) -> Option<AccountEvidenceClass> {
        self.per_account
            .get(logical_account)
            .map(|(_, evidence)| *evidence)
    }

    /// The usage landed in the unknown-account bucket, or `None` if it is empty.
    pub fn unknown_account_usage(&self) -> Option<KnownTokenVector> {
        self.unknown_account
    }

    /// The evidence class for the unknown-account bucket: always `Unattributed`.
    pub fn unknown_account_evidence_class(&self) -> AccountEvidenceClass {
        AccountEvidenceClass::Unattributed
    }

    /// Every account that received usage, with its total.
    pub fn accounts(&self) -> impl Iterator<Item = (&str, &KnownTokenVector)> {
        self.per_account
            .iter()
            .map(|(account, (usage, _))| (account.as_str(), usage))
    }

    /// Every account that received usage, with its total and justifying evidence class.
    pub fn accounts_with_evidence(
        &self,
    ) -> impl Iterator<Item = (&str, &KnownTokenVector, AccountEvidenceClass)> {
        self.per_account
            .iter()
            .map(|(account, (usage, evidence))| (account.as_str(), usage, *evidence))
    }

    /// The conservation total: every account's usage plus the unknown-account
    /// bucket. Equal to the sum of every input event's usage by construction,
    /// since [`segment`] assigns each event's full usage to exactly one
    /// target and never splits or drops one.
    pub fn total(&self) -> KnownTokenVector {
        self.per_account
            .values()
            .map(|(usage, _)| usage)
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
    evidence_class: AccountEvidenceClass,
}

/// Builds the marker-to-marker interval timeline.
///
/// Precedence rule: markers are grouped by effective evidence rank. Lower-confidence
/// markers (such as conservative temporal inference when explicit markers exist) cannot
/// overwrite higher-confidence markers, regardless of arrival order. Only markers at the
/// winning (highest confidence) rank form the active timeline.
///
/// Within the winning rank, markers are ordered by timestamp, source ordering key,
/// and input stability fallback.
fn build_intervals(markers: &[AccountMarkerBoundary]) -> Vec<MarkerInterval> {
    if markers.is_empty() {
        return Vec::new();
    }
    let winning_rank = markers
        .iter()
        .map(|m| m.effective_evidence_class().rank())
        .min()
        .expect("markers is non-empty");

    let qualifying: Vec<&AccountMarkerBoundary> = markers
        .iter()
        .filter(|m| m.effective_evidence_class().rank() == winning_rank)
        .collect();

    let mut sorted = qualifying;
    sorted.sort_by(|a, b| {
        a.observed_at
            .cmp(&b.observed_at)
            .then_with(|| a.source_ordering_key.cmp(&b.source_ordering_key))
    });

    let mut intervals = Vec::with_capacity(sorted.len());
    for (index, marker) in sorted.iter().enumerate() {
        let hi = sorted.get(index + 1).map(|next| next.observed_at);
        intervals.push(MarkerInterval {
            lo: marker.observed_at,
            hi,
            logical_account: marker.logical_account.clone(),
            evidence_class: marker.effective_evidence_class(),
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
        let (target, evidence_class) = match locate(&intervals, event.occurred_at) {
            Some(interval) => (
                AccountSegmentTarget::Account(interval.logical_account.clone()),
                interval.evidence_class,
            ),
            None => (
                AccountSegmentTarget::UnknownAccount,
                AccountEvidenceClass::Unattributed,
            ),
        };
        result.add(target, event.usage, evidence_class);
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
            evidence_class: AccountEvidenceClass::ExplicitLauncherOrHook,
            mapping_validated: true,
        }
    }

    fn marker_with_class(
        account: &str,
        at: i64,
        ordering_key: Option<i64>,
        evidence_class: AccountEvidenceClass,
        mapping_validated: bool,
    ) -> AccountMarkerBoundary {
        AccountMarkerBoundary {
            logical_account: account.to_owned(),
            observed_at: t(at),
            source_ordering_key: ordering_key,
            evidence_class,
            mapping_validated,
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

    // --- evidence ranking and precedence ----------------------------------

    #[test]
    fn precedence_across_all_five_evidence_classes_tested_pairwise() {
        let classes = AccountEvidenceClass::ALL;
        assert_eq!(classes.len(), 5);

        // Verify ranks: 1 through 5 strictly ascending.
        assert_eq!(AccountEvidenceClass::ExplicitLauncherOrHook.rank(), 1);
        assert_eq!(AccountEvidenceClass::ExplicitProviderIdentity.rank(), 2);
        assert_eq!(AccountEvidenceClass::ConfiguredCredentialMapping.rank(), 3);
        assert_eq!(
            AccountEvidenceClass::ConservativeTemporalInference.rank(),
            4
        );
        assert_eq!(AccountEvidenceClass::Unattributed.rank(), 5);

        for (i, &a) in classes.iter().enumerate() {
            // Irreflexivity: a does not take precedence over a.
            assert!(!a.takes_precedence_over(a));

            for (j, &b) in classes.iter().enumerate() {
                if i < j {
                    // a comes before b in the precedence order, so a takes precedence over b.
                    assert!(
                        a.takes_precedence_over(b),
                        "{a:?} (rank {}) must take precedence over {b:?} (rank {})",
                        a.rank(),
                        b.rank()
                    );
                    // Asymmetry: b cannot take precedence over a.
                    assert!(
                        !b.takes_precedence_over(a),
                        "{b:?} must not take precedence over {a:?}"
                    );
                } else if i > j {
                    assert!(
                        !a.takes_precedence_over(b),
                        "{a:?} (rank {}) must not take precedence over {b:?} (rank {})",
                        a.rank(),
                        b.rank()
                    );
                }
            }
        }

        // Transitivity: for all a, b, c, if a > b and b > c then a > c.
        for &a in &classes {
            for &b in &classes {
                for &c in &classes {
                    if a.takes_precedence_over(b) && b.takes_precedence_over(c) {
                        assert!(
                            a.takes_precedence_over(c),
                            "transitivity failed: {a:?} > {b:?} and {b:?} > {c:?} but not {a:?} > {c:?}"
                        );
                    }
                }
            }
        }

        // Test is_inferred, is_explicit, is_eligible_for_passive_calibration.
        assert!(AccountEvidenceClass::ConservativeTemporalInference.is_inferred());
        assert!(!AccountEvidenceClass::ExplicitLauncherOrHook.is_inferred());
        assert!(!AccountEvidenceClass::ExplicitProviderIdentity.is_inferred());
        assert!(!AccountEvidenceClass::ConfiguredCredentialMapping.is_inferred());
        assert!(!AccountEvidenceClass::Unattributed.is_inferred());

        assert!(AccountEvidenceClass::ExplicitLauncherOrHook.is_explicit());
        assert!(AccountEvidenceClass::ExplicitProviderIdentity.is_explicit());
        assert!(!AccountEvidenceClass::ConfiguredCredentialMapping.is_explicit());
        assert!(!AccountEvidenceClass::ConservativeTemporalInference.is_explicit());
        assert!(!AccountEvidenceClass::Unattributed.is_explicit());

        assert!(AccountEvidenceClass::ExplicitLauncherOrHook.is_eligible_for_passive_calibration());
        assert!(
            AccountEvidenceClass::ExplicitProviderIdentity.is_eligible_for_passive_calibration()
        );
        assert!(
            AccountEvidenceClass::ConfiguredCredentialMapping.is_eligible_for_passive_calibration()
        );
        assert!(
            !AccountEvidenceClass::ConservativeTemporalInference
                .is_eligible_for_passive_calibration()
        );
        assert!(!AccountEvidenceClass::Unattributed.is_eligible_for_passive_calibration());

        // Test string parsing and display round-trip for all 5.
        for &class in &classes {
            let s = class.as_str();
            assert_eq!(AccountEvidenceClass::parse(s), Some(class));
            assert_eq!(s.parse::<AccountEvidenceClass>().unwrap(), class);
            assert_eq!(class.to_string(), s);
        }
    }

    #[test]
    fn unvalidated_credential_source_mapping_falls_through_to_next_class() {
        let valid_cred = marker_with_class(
            "account-valid",
            10,
            None,
            AccountEvidenceClass::ConfiguredCredentialMapping,
            true,
        );
        assert_eq!(
            valid_cred.effective_evidence_class(),
            AccountEvidenceClass::ConfiguredCredentialMapping
        );
        assert_eq!(valid_cred.effective_evidence_class().rank(), 3);

        let unvalidated_cred = marker_with_class(
            "account-unvalidated",
            10,
            None,
            AccountEvidenceClass::ConfiguredCredentialMapping,
            false,
        );
        assert_eq!(
            unvalidated_cred.effective_evidence_class(),
            AccountEvidenceClass::ConservativeTemporalInference
        );
        assert_eq!(unvalidated_cred.effective_evidence_class().rank(), 4);

        // When segmented alongside a validated mapping (Rank 3), the unvalidated mapping
        // (effective Rank 4) loses to the validated one and is not used.
        let inputs = AccountSegmentationInputs {
            markers: vec![
                marker_with_class(
                    "account-unvalidated",
                    10,
                    None,
                    AccountEvidenceClass::ConfiguredCredentialMapping,
                    false,
                ),
                marker_with_class(
                    "account-validated",
                    20,
                    None,
                    AccountEvidenceClass::ConfiguredCredentialMapping,
                    true,
                ),
            ],
            usage: vec![event(25, 50)],
        };
        let result = segment(&inputs);
        assert_eq!(result.account_usage("account-validated"), Some(tokens(50)));
        assert_eq!(
            result.account_evidence_class("account-validated"),
            Some(AccountEvidenceClass::ConfiguredCredentialMapping)
        );
        assert_eq!(result.account_usage("account-unvalidated"), None);

        // When only an unvalidated credential mapping is present, it falls through to
        // ConservativeTemporalInference (Rank 4) and attributes as inferred.
        let solo_unvalidated_inputs = AccountSegmentationInputs {
            markers: vec![marker_with_class(
                "account-unvalidated",
                10,
                None,
                AccountEvidenceClass::ConfiguredCredentialMapping,
                false,
            )],
            usage: vec![event(15, 30)],
        };
        let solo_result = segment(&solo_unvalidated_inputs);
        assert_eq!(
            solo_result.account_usage("account-unvalidated"),
            Some(tokens(30))
        );
        assert_eq!(
            solo_result.account_evidence_class("account-unvalidated"),
            Some(AccountEvidenceClass::ConservativeTemporalInference)
        );
        assert!(
            solo_result
                .account_evidence_class("account-unvalidated")
                .unwrap()
                .is_inferred()
        );
    }

    #[test]
    fn explicit_marker_not_overwritten_by_later_inferred_marker() {
        let inputs = AccountSegmentationInputs {
            markers: vec![
                marker_with_class(
                    "account-explicit",
                    10,
                    None,
                    AccountEvidenceClass::ExplicitLauncherOrHook,
                    true,
                ),
                marker_with_class(
                    "account-inferred",
                    20,
                    None,
                    AccountEvidenceClass::ConservativeTemporalInference,
                    true,
                ),
            ],
            usage: vec![event(15, 100), event(25, 200)],
        };
        let result = segment(&inputs);
        assert_eq!(result.account_usage("account-explicit"), Some(tokens(300)));
        assert_eq!(
            result.account_evidence_class("account-explicit"),
            Some(AccountEvidenceClass::ExplicitLauncherOrHook)
        );
        assert_eq!(result.account_usage("account-inferred"), None);
    }

    #[test]
    fn explicit_marker_not_overwritten_by_earlier_inferred_marker() {
        let inputs = AccountSegmentationInputs {
            markers: vec![
                marker_with_class(
                    "account-inferred",
                    10,
                    None,
                    AccountEvidenceClass::ConservativeTemporalInference,
                    true,
                ),
                marker_with_class(
                    "account-explicit",
                    20,
                    None,
                    AccountEvidenceClass::ExplicitLauncherOrHook,
                    true,
                ),
            ],
            usage: vec![event(25, 100)],
        };
        let result = segment(&inputs);
        assert_eq!(result.account_usage("account-explicit"), Some(tokens(100)));
        assert_eq!(
            result.account_evidence_class("account-explicit"),
            Some(AccountEvidenceClass::ExplicitLauncherOrHook)
        );
        assert_eq!(result.account_usage("account-inferred"), None);
    }

    #[test]
    fn inferred_marker_cannot_overwrite_explicit_marker_regardless_of_arrival_order() {
        // Arrival order 1: [explicit, inferred]
        let inputs_order_1 = AccountSegmentationInputs {
            markers: vec![
                AccountMarkerBoundary::explicit("account-explicit", t(10), None),
                AccountMarkerBoundary::inferred("account-inferred", t(20), None),
            ],
            usage: vec![event(15, 50), event(25, 50)],
        };
        let result_1 = segment(&inputs_order_1);
        assert_eq!(
            result_1.account_usage("account-explicit"),
            Some(tokens(100))
        );
        assert_eq!(result_1.account_usage("account-inferred"), None);
        assert_eq!(
            result_1.account_evidence_class("account-explicit"),
            Some(AccountEvidenceClass::ExplicitLauncherOrHook)
        );

        // Arrival order 2: [inferred, explicit]
        let inputs_order_2 = AccountSegmentationInputs {
            markers: vec![
                AccountMarkerBoundary::inferred("account-inferred", t(20), None),
                AccountMarkerBoundary::explicit("account-explicit", t(10), None),
            ],
            usage: vec![event(15, 50), event(25, 50)],
        };
        let result_2 = segment(&inputs_order_2);
        assert_eq!(
            result_2.account_usage("account-explicit"),
            Some(tokens(100))
        );
        assert_eq!(result_2.account_usage("account-inferred"), None);
        assert_eq!(
            result_2.account_evidence_class("account-explicit"),
            Some(AccountEvidenceClass::ExplicitLauncherOrHook)
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
