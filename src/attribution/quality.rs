//! Attribution-quality metric: how much canonical usage carries a justifying
//! account attribution, broken down by evidence class, over all history and
//! over a configurable recent window (`aub-mgv.3`, PLAN.md 20, 36, 45).
//!
//! The metric is a pure function of a set of [`AttributionObservation`]s, one
//! per persisted `account_attribution_segment` row. The store layer assembles
//! the observations; this module never reads SQLite.
//!
//! Two rules shape the arithmetic. First, token kinds are never collapsed into
//! one number: every quantity here is per [`TokenKind`], matching the domain's
//! refusal of a `total_tokens()`. Second, a fraction with a zero denominator is
//! "no ratio", never a fabricated zero: absence of usage is not a coverage
//! failure, and a substituted zero would read as "every token unattributed".
//!
//! The recent window exists because attribution coverage degrades slowly. A
//! launcher that stopped emitting markers, a new CLI with no hook, headless
//! runs that cannot mark themselves: each moves a few percent into the
//! unknown-account bucket, and a lifetime average hides that for months. The
//! window is keyed on the observation's session start, so an observation whose
//! session could not be located is counted in all-history totals but named,
//! not dropped, when the window is computed.

use crate::attribution::account_segment::{AccountEvidenceClass, AccountSegmentationResult};
use crate::domain::time::UtcTimestamp;
use crate::domain::tokens::{KnownTokenVector, TokenKind};

/// One persisted account attribution segment's contribution to the metric.
///
/// `observed_at` is the start of the session the segment belongs to, or `None`
/// when that session could not be located: the observation still counts toward
/// all-history totals (a missing timestamp is not evidence of no usage) but is
/// excluded from, and named by, the recent-window metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionObservation {
    pub evidence_class: AccountEvidenceClass,
    pub usage: KnownTokenVector,
    pub observed_at: Option<UtcTimestamp>,
}

impl AttributionObservation {
    /// The observations one session's segmentation result contributes: one per
    /// named account and one for the unknown-account bucket, each tagged with
    /// the same `observed_at`.
    pub fn from_segmentation(
        result: &AccountSegmentationResult,
        observed_at: Option<UtcTimestamp>,
    ) -> Vec<Self> {
        let mut observations: Vec<Self> = result
            .accounts_with_evidence()
            .map(|(_, usage, evidence_class)| Self {
                evidence_class,
                usage: *usage,
                observed_at,
            })
            .collect();
        if let Some(usage) = result.unknown_account_usage() {
            observations.push(Self {
                evidence_class: result.unknown_account_evidence_class(),
                usage,
                observed_at,
            });
        }
        observations
    }
}

/// An exact ratio of one slice of usage to a total, for one token kind.
///
/// Kept as numerator and denominator rather than a float so the floor
/// comparison is exact and a zero denominator stays representable as "no
/// ratio".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionFraction {
    numerator: u64,
    denominator: u64,
}

impl AttributionFraction {
    /// The numerator: attributed tokens, or the tokens of one evidence class.
    pub fn numerator(self) -> u64 {
        self.numerator
    }

    /// The denominator: total known usage of this token kind over the set.
    pub fn denominator(self) -> u64 {
        self.denominator
    }

    /// The fraction in parts per million, rounded half up, saturating at the
    /// full `1_000_000`. `None` when the denominator is zero: there is no
    /// ratio, and this module never substitutes a zero for one.
    pub fn ppm(self) -> Option<u32> {
        if self.denominator == 0 {
            return None;
        }
        let scaled = (u128::from(self.numerator) * 1_000_000 + u128::from(self.denominator) / 2)
            / u128::from(self.denominator);
        Some(scaled.min(1_000_000) as u32)
    }

    /// The fraction as a bare number in `[0.0, 1.0]` for rendering, or `None`
    /// when there is no ratio.
    pub fn as_f64(self) -> Option<f64> {
        (self.denominator != 0).then(|| self.numerator as f64 / self.denominator as f64)
    }

    /// True when this fraction is strictly below `floor`, tested with exact
    /// integer arithmetic. Always false when there is no ratio: a token kind
    /// with no usage cannot have declined.
    pub fn is_below(self, floor: AttributionQualityFloor) -> bool {
        if self.denominator == 0 {
            return false;
        }
        u128::from(self.numerator) * 1_000_000
            < u128::from(floor.ppm()) * u128::from(self.denominator)
    }
}

/// An advisory floor for the attributed fraction: a value in `[0.0, 1.0]`
/// stored in parts per million. The value an operator configures; the number
/// that decides a breach lives once here (PLAN.md 45, the advisory floor is
/// decided by `aub-cab.7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionQualityFloor(u32);

impl AttributionQualityFloor {
    /// Builds a floor from a fraction, rejecting anything outside `[0.0, 1.0]`.
    pub fn new(fraction: f64) -> Option<Self> {
        (0.0..=1.0)
            .contains(&fraction)
            .then(|| Self((fraction * 1_000_000.0).round().clamp(0.0, 1_000_000.0) as u32))
    }

    /// The floor in parts per million.
    pub fn ppm(self) -> u32 {
        self.0
    }

    /// The floor as a bare fraction in `[0.0, 1.0]` for rendering.
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

/// The per-evidence-class token counts for one token kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceClassBreakdown {
    /// Indexed by evidence-class rank minus one, so the array is exhaustive
    /// over [`AccountEvidenceClass::ALL`] by construction.
    per_class: [u64; 5],
}

impl EvidenceClassBreakdown {
    fn empty() -> Self {
        Self { per_class: [0; 5] }
    }

    fn add(&mut self, class: AccountEvidenceClass, tokens: u64) {
        self.per_class[class.rank() as usize - 1] += tokens;
    }

    /// The tokens attributed under one evidence class.
    pub fn tokens(&self, class: AccountEvidenceClass) -> u64 {
        self.per_class[class.rank() as usize - 1]
    }

    /// Total known usage of this token kind over the set.
    pub fn total(&self) -> u64 {
        self.per_class.iter().sum()
    }

    /// Usage carrying any justifying account evidence: every class except the
    /// unattributed (unknown-account) bucket.
    pub fn attributed(&self) -> u64 {
        AccountEvidenceClass::ALL
            .iter()
            .filter(|class| **class != AccountEvidenceClass::Unattributed)
            .map(|class| self.tokens(*class))
            .sum()
    }

    /// Usage in the unknown-account bucket: the unattributed class exactly.
    pub fn unknown_account(&self) -> u64 {
        self.tokens(AccountEvidenceClass::Unattributed)
    }

    /// The attributed fraction of this token kind.
    pub fn attributed_fraction(&self) -> AttributionFraction {
        AttributionFraction {
            numerator: self.attributed(),
            denominator: self.total(),
        }
    }

    /// The fraction of this token kind carried by one evidence class.
    pub fn class_fraction(&self, class: AccountEvidenceClass) -> AttributionFraction {
        AttributionFraction {
            numerator: self.tokens(class),
            denominator: self.total(),
        }
    }
}

/// Which token-kind slot an [`AttributionQuality`] array index holds. An
/// exhaustive match, so a fifth [`TokenKind`] fails to compile here rather than
/// silently sharing a slot.
fn kind_index(kind: TokenKind) -> usize {
    match kind {
        TokenKind::Input => 0,
        TokenKind::Output => 1,
        TokenKind::CacheRead => 2,
        TokenKind::CacheWrite => 3,
    }
}

/// The attribution-quality metric over a selected set of observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionQuality {
    /// Indexed by [`kind_index`], exhaustive over [`TokenKind::ALL`].
    per_kind: [EvidenceClassBreakdown; 4],
    observation_count: u64,
}

impl AttributionQuality {
    /// Computes the metric over every observation in the set: the all-history
    /// scope.
    pub fn over(observations: impl IntoIterator<Item = AttributionObservation>) -> Self {
        let mut per_kind = [EvidenceClassBreakdown::empty(); 4];
        let mut observation_count = 0;
        for observation in observations {
            observation_count += 1;
            for kind in TokenKind::ALL {
                per_kind[kind_index(kind)]
                    .add(observation.evidence_class, observation.usage.value(kind));
            }
        }
        Self {
            per_kind,
            observation_count,
        }
    }

    /// Computes the metric over the observations whose session start is at or
    /// after `since`. Observations with no session start cannot be placed in or
    /// out of the window, so they are excluded and counted, never dropped
    /// silently or folded in.
    pub fn windowed(
        observations: impl IntoIterator<Item = AttributionObservation>,
        since: UtcTimestamp,
    ) -> WindowedAttributionQuality {
        let mut undated_observations = 0;
        let mut kept = Vec::new();
        for observation in observations {
            match observation.observed_at {
                Some(at) if at >= since => kept.push(observation),
                Some(_) => {}
                None => undated_observations += 1,
            }
        }
        WindowedAttributionQuality {
            since,
            quality: Self::over(kept),
            undated_observations,
        }
    }

    /// The breakdown for one token kind.
    pub fn breakdown(&self, kind: TokenKind) -> &EvidenceClassBreakdown {
        &self.per_kind[kind_index(kind)]
    }

    /// The number of observations folded into this metric.
    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// True when no token kind carries any usage over the set: the metric has
    /// no ratio to report, and a renderer must say so rather than print `0%`.
    pub fn is_empty(&self) -> bool {
        TokenKind::ALL
            .iter()
            .all(|kind| self.breakdown(*kind).total() == 0)
    }

    /// One breach per token kind whose attributed fraction is below `floor`.
    /// A token kind with no usage is skipped: there is no ratio and no
    /// evidence of decline.
    pub fn floor_breaches(
        &self,
        scope: MetricScope,
        floor: AttributionQualityFloor,
    ) -> Vec<AttributionFloorBreach> {
        TokenKind::ALL
            .iter()
            .filter_map(|&kind| {
                let fraction = self.breakdown(kind).attributed_fraction();
                fraction.is_below(floor).then_some(AttributionFloorBreach {
                    scope,
                    kind,
                    fraction,
                    floor,
                })
            })
            .collect()
    }
}

/// Which scope a metric or a breach was computed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricScope {
    AllHistory,
    RecentWindow { since: UtcTimestamp },
}

/// The metric over a recent window, with the count of observations left out
/// because their session start was unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowedAttributionQuality {
    pub since: UtcTimestamp,
    pub quality: AttributionQuality,
    pub undated_observations: u64,
}

/// One token kind's attributed fraction fell below the configured floor, in
/// one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionFloorBreach {
    pub scope: MetricScope,
    pub kind: TokenKind,
    pub fraction: AttributionFraction,
    pub floor: AttributionQualityFloor,
}

/// The doctor-facing assessment: the metric over all history and over the
/// recent window, plus every floor breach when a floor is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionQualityAssessment {
    pub all_history: AttributionQuality,
    pub recent_window: WindowedAttributionQuality,
    pub breaches: Vec<AttributionFloorBreach>,
}

impl AttributionQualityAssessment {
    /// Assembles the assessment. `floor` is `None` until an operator configures
    /// one, and the metric is still reported: doctor names the number without
    /// judging it.
    pub fn assess(
        observations: Vec<AttributionObservation>,
        window_since: UtcTimestamp,
        floor: Option<AttributionQualityFloor>,
    ) -> Self {
        let all_history = AttributionQuality::over(observations.iter().cloned());
        let recent_window =
            AttributionQuality::windowed(observations.iter().cloned(), window_since);
        let mut breaches = Vec::new();
        if let Some(floor) = floor {
            breaches.extend(all_history.floor_breaches(MetricScope::AllHistory, floor));
            breaches.extend(recent_window.quality.floor_breaches(
                MetricScope::RecentWindow {
                    since: window_since,
                },
                floor,
            ));
        }
        Self {
            all_history,
            recent_window,
            breaches,
        }
    }

    /// True when at least one token kind is below the configured floor in some
    /// scope.
    pub fn has_breach(&self) -> bool {
        !self.breaches.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::account_segment::{
        AccountMarkerBoundary, AccountSegmentationInputs, AccountUsageEvent, segment,
    };
    use crate::domain::tokens::{CacheReadTokens, CacheWriteTokens, InputTokens, OutputTokens};
    use proptest::prelude::*;

    fn vector(input: u64, output: u64, cache_read: u64, cache_write: u64) -> KnownTokenVector {
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(cache_write),
        )
    }

    fn observation(
        evidence_class: AccountEvidenceClass,
        input: u64,
        observed_at: Option<i64>,
    ) -> AttributionObservation {
        AttributionObservation {
            evidence_class,
            usage: vector(input, 0, 0, 0),
            observed_at: observed_at.map(UtcTimestamp::from_unix_nanos),
        }
    }

    #[test]
    fn attributed_is_every_class_except_the_unknown_account_bucket() {
        let quality = AttributionQuality::over([
            observation(AccountEvidenceClass::ExplicitLauncherOrHook, 40, None),
            observation(AccountEvidenceClass::ExplicitProviderIdentity, 20, None),
            observation(AccountEvidenceClass::ConfiguredCredentialMapping, 10, None),
            observation(
                AccountEvidenceClass::ConservativeTemporalInference,
                10,
                None,
            ),
            observation(AccountEvidenceClass::Unattributed, 20, None),
        ]);
        let input = quality.breakdown(TokenKind::Input);
        assert_eq!(input.total(), 100);
        assert_eq!(input.attributed(), 80);
        assert_eq!(input.unknown_account(), 20);
        assert_eq!(input.attributed_fraction().ppm(), Some(800_000));
        assert_eq!(
            input
                .class_fraction(AccountEvidenceClass::ConservativeTemporalInference)
                .ppm(),
            Some(100_000)
        );
    }

    #[test]
    fn a_token_kind_with_no_usage_has_no_fraction_rather_than_zero() {
        let quality = AttributionQuality::over([observation(
            AccountEvidenceClass::ExplicitLauncherOrHook,
            50,
            None,
        )]);
        // Input carries usage; output, cache-read and cache-write do not.
        assert_eq!(
            quality
                .breakdown(TokenKind::Input)
                .attributed_fraction()
                .ppm(),
            Some(1_000_000)
        );
        for empty in [
            TokenKind::Output,
            TokenKind::CacheRead,
            TokenKind::CacheWrite,
        ] {
            assert_eq!(
                quality.breakdown(empty).attributed_fraction().ppm(),
                None,
                "an empty token kind must report no ratio, never 0"
            );
        }
    }

    #[test]
    fn the_recent_window_excludes_and_counts_observations_with_no_session_start() {
        let observations = vec![
            observation(
                AccountEvidenceClass::ExplicitLauncherOrHook,
                100,
                Some(1_000),
            ),
            observation(AccountEvidenceClass::Unattributed, 100, Some(100)),
            observation(AccountEvidenceClass::Unattributed, 999, None),
        ];
        let windowed =
            AttributionQuality::windowed(observations, UtcTimestamp::from_unix_nanos(500));
        // Only the first observation (t=1000) is in the window.
        assert_eq!(windowed.quality.breakdown(TokenKind::Input).total(), 100);
        assert_eq!(
            windowed.quality.breakdown(TokenKind::Input).attributed(),
            100
        );
        assert_eq!(windowed.undated_observations, 1);
    }

    #[test]
    fn a_slow_decline_is_visible_in_the_window_but_not_in_the_lifetime_average() {
        // Old history: fully attributed. Recent history: half unattributed.
        let mut observations = Vec::new();
        for _ in 0..9 {
            observations.push(observation(
                AccountEvidenceClass::ExplicitLauncherOrHook,
                100,
                Some(10),
            ));
        }
        observations.push(observation(
            AccountEvidenceClass::ExplicitLauncherOrHook,
            50,
            Some(1_000),
        ));
        observations.push(observation(
            AccountEvidenceClass::Unattributed,
            50,
            Some(1_000),
        ));

        let all = AttributionQuality::over(observations.iter().cloned());
        let window = AttributionQuality::windowed(
            observations.iter().cloned(),
            UtcTimestamp::from_unix_nanos(500),
        );

        // Lifetime: 950 of 1000 attributed, 95%.
        assert_eq!(
            all.breakdown(TokenKind::Input).attributed_fraction().ppm(),
            Some(950_000)
        );
        // Recent window: 50 of 100 attributed, 50%.
        assert_eq!(
            window
                .quality
                .breakdown(TokenKind::Input)
                .attributed_fraction()
                .ppm(),
            Some(500_000)
        );

        let floor = AttributionQualityFloor::new(0.8).unwrap();
        assert!(
            all.floor_breaches(MetricScope::AllHistory, floor)
                .is_empty()
        );
        let breaches = window.quality.floor_breaches(
            MetricScope::RecentWindow {
                since: UtcTimestamp::from_unix_nanos(500),
            },
            floor,
        );
        assert_eq!(breaches.len(), 1);
        assert_eq!(breaches[0].kind, TokenKind::Input);
    }

    #[test]
    fn the_floor_breach_test_is_exact_at_the_boundary() {
        // 799_999 attributed of 1_000_000: just under an 0.8 floor.
        let quality = AttributionQuality::over([
            observation(AccountEvidenceClass::ExplicitLauncherOrHook, 799_999, None),
            observation(AccountEvidenceClass::Unattributed, 200_001, None),
        ]);
        let floor = AttributionQualityFloor::new(0.8).unwrap();
        assert!(
            quality
                .breakdown(TokenKind::Input)
                .attributed_fraction()
                .is_below(floor)
        );

        // Exactly at the floor is not a breach.
        let exact = AttributionQuality::over([
            observation(AccountEvidenceClass::ExplicitLauncherOrHook, 800_000, None),
            observation(AccountEvidenceClass::Unattributed, 200_000, None),
        ]);
        assert!(
            !exact
                .breakdown(TokenKind::Input)
                .attributed_fraction()
                .is_below(floor)
        );
    }

    #[test]
    fn an_empty_set_reports_no_ratio_and_no_breach() {
        let quality = AttributionQuality::over([]);
        assert!(quality.is_empty());
        for kind in TokenKind::ALL {
            assert_eq!(quality.breakdown(kind).attributed_fraction().ppm(), None);
        }
        let floor = AttributionQualityFloor::new(0.99).unwrap();
        assert!(
            quality
                .floor_breaches(MetricScope::AllHistory, floor)
                .is_empty()
        );
    }

    #[test]
    fn removing_every_marker_moves_the_whole_corpus_into_the_unknown_account_bucket() {
        let usage = vec![
            AccountUsageEvent {
                occurred_at: UtcTimestamp::from_unix_nanos(10),
                usage: vector(30, 7, 2, 1),
            },
            AccountUsageEvent {
                occurred_at: UtcTimestamp::from_unix_nanos(20),
                usage: vector(40, 9, 3, 1),
            },
        ];
        let with_marker = segment(&AccountSegmentationInputs {
            markers: vec![AccountMarkerBoundary::explicit(
                "account-a",
                UtcTimestamp::from_unix_nanos(0),
                None,
            )],
            usage: usage.clone(),
        });
        let without_marker = segment(&AccountSegmentationInputs {
            markers: vec![],
            usage,
        });

        let attributed = AttributionQuality::over(AttributionObservation::from_segmentation(
            &with_marker,
            None,
        ));
        let unattributed = AttributionQuality::over(AttributionObservation::from_segmentation(
            &without_marker,
            None,
        ));

        for kind in TokenKind::ALL {
            let total = attributed.breakdown(kind).total();
            assert_eq!(
                unattributed.breakdown(kind).total(),
                total,
                "conservation must hold per kind"
            );
            if total > 0 {
                assert_eq!(
                    attributed.breakdown(kind).attributed_fraction().ppm(),
                    Some(1_000_000)
                );
                assert_eq!(
                    unattributed.breakdown(kind).unknown_account(),
                    total,
                    "with no marker the whole corpus is unknown-account"
                );
                assert_eq!(unattributed.breakdown(kind).attributed(), 0);
            }
        }
    }

    proptest! {
        /// Over a randomized marker set, the sum of the per-account usage and
        /// the unknown-account bucket equals the total canonical usage of the
        /// selected set, per token kind, with no remainder.
        #[test]
        fn prop_per_account_plus_unknown_equals_total_over_randomized_markers(
            marker_times in proptest::collection::vec(0i64..500, 0..4),
            usage_specs in proptest::collection::vec((0i64..500, 1u64..100, 0u64..50), 1..10),
        ) {
            let markers: Vec<AccountMarkerBoundary> = marker_times
                .iter()
                .enumerate()
                .map(|(i, &at)| {
                    AccountMarkerBoundary::explicit(
                        format!("account-{i}"),
                        UtcTimestamp::from_unix_nanos(at),
                        None,
                    )
                })
                .collect();
            let usage: Vec<AccountUsageEvent> = usage_specs
                .iter()
                .map(|&(at, input, output)| AccountUsageEvent {
                    occurred_at: UtcTimestamp::from_unix_nanos(at),
                    usage: vector(input, output, 0, 0),
                })
                .collect();
            let expected = usage
                .iter()
                .fold((0u64, 0u64), |(i, o), event| {
                    (i + event.usage.value(TokenKind::Input), o + event.usage.value(TokenKind::Output))
                });

            let result = segment(&AccountSegmentationInputs { markers, usage });
            let quality = AttributionQuality::over(AttributionObservation::from_segmentation(&result, None));

            let input = quality.breakdown(TokenKind::Input);
            prop_assert_eq!(input.attributed() + input.unknown_account(), expected.0);
            prop_assert_eq!(input.total(), expected.0);
            let output = quality.breakdown(TokenKind::Output);
            prop_assert_eq!(output.attributed() + output.unknown_account(), expected.1);
            prop_assert_eq!(output.total(), expected.1);
        }
    }

    #[test]
    fn the_floor_rejects_a_fraction_outside_the_unit_interval() {
        assert!(AttributionQualityFloor::new(-0.01).is_none());
        assert!(AttributionQualityFloor::new(1.01).is_none());
        assert_eq!(AttributionQualityFloor::new(0.0).unwrap().ppm(), 0);
        assert_eq!(AttributionQualityFloor::new(1.0).unwrap().ppm(), 1_000_000);
    }

    #[test]
    fn the_assessment_reports_the_metric_even_with_no_floor_configured() {
        let observations = vec![
            observation(AccountEvidenceClass::ExplicitLauncherOrHook, 60, Some(10)),
            observation(AccountEvidenceClass::Unattributed, 40, Some(10)),
        ];
        let assessment = AttributionQualityAssessment::assess(
            observations,
            UtcTimestamp::from_unix_nanos(0),
            None,
        );
        assert!(!assessment.has_breach());
        assert_eq!(
            assessment
                .all_history
                .breakdown(TokenKind::Input)
                .attributed_fraction()
                .ppm(),
            Some(600_000)
        );
    }

    #[test]
    fn the_assessment_flags_a_breach_in_either_scope() {
        let observations = vec![
            observation(AccountEvidenceClass::ExplicitLauncherOrHook, 10, Some(10)),
            observation(AccountEvidenceClass::Unattributed, 90, Some(10)),
        ];
        let floor = AttributionQualityFloor::new(0.5);
        let assessment = AttributionQualityAssessment::assess(
            observations,
            UtcTimestamp::from_unix_nanos(0),
            floor,
        );
        assert!(assessment.has_breach());
        // Both scopes see the same low fraction here.
        assert_eq!(assessment.breaches.len(), 2);
    }
}
