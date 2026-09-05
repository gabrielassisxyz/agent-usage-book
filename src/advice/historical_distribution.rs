//! Historical task distribution with eligibility filters (`aub-cab.1`,
//! PLAN.md 26.1, 26.2, 34.15, 34.23).
//!
//! Collapsing history into one average is the shortcut that makes an
//! advisory feel precise and answer wrongly. This module reports a sample
//! count, an empirical median, a documented empirical central range and an
//! upper reference quantile instead, computed only from completed tasks
//! whose evidence justifies comparison, and never claims to predict or
//! forecast the next task: everything here is empirical history.
//!
//! # Eligibility
//!
//! By default a completed task's usage does not enter the reference
//! distribution when any of four conditions holds (PLAN.md 26.1):
//! [`TaskPricing::UnknownTokenComponents`] (the usage carried components no
//! cost model term covers, so `crate::cost_model::convert` refused it), an
//! unattributed account ([`AccountEvidenceClass::Unattributed`]), incomplete
//! task segmentation, or estimated (reconstructed) rather than measured
//! tokens ([`EvidenceQuality`]). [`ineligibility_reason`] checks them in that
//! fixed order; the bead leaves the case of a task failing more than one
//! rule unspecified, so this order is a documented default rather than an
//! unstated implementation accident: a task with no valid price cannot be
//! classified further, so pricing is checked first; account attribution and
//! segmentation are properties of the evidence a cost model never sees, so
//! they are checked next; token-evidence quality is checked last because it
//! is the softest of the four (an estimated task still has *a* number, just
//! a reconstructed one).
//!
//! Every completed task lands in exactly one bucket: included, or excluded
//! for exactly one of the four reasons above. [`build_group_reports`]'s own
//! property test proves the exclusion counts plus the included count always
//! equal the number of samples given for a group, which is this bead's
//! "done when" criterion applied to any input rather than one fixture.
//!
//! # Quantiles and the minimum sample count
//!
//! The default quantiles, minimum sample count and quantile method
//! ([`HistoricalDistributionConfig`]) are `aub-1o3`'s decision (2026-09-04,
//! option A): central range p25-p75, upper reference p90, minimum 12
//! samples, nearest-rank. They are configuration read by `crate::config`,
//! not source constants, so a configured change is visible in every
//! printed report. Below the minimum, [`DistributionVerdict::InsufficientEvidence`]
//! is reported instead of a distribution computed over too few points.
//!
//! # Attribution-quality coverage
//!
//! [`AttributionCoverage`] is `aub-cab.7`'s decision (2026-09-04, option B):
//! a canonical-usage fraction per grouping key, generic over `K` rather than
//! hardcoded to `TaskKind`, per that bead's own requirement that "the
//! distribution and can-run beads... must take the group key as a parameter
//! rather than `TaskKind`". The denominator is the total credits of every
//! completed task in the group and period that resolved to a price at all
//! (including tasks excluded from the reference distribution for unknown
//! account, incomplete segmentation or estimated usage); the numerator is
//! the credits of the tasks that entered the reference distribution. A task
//! with unknown token components has no price to weigh either sum with, so
//! it contributes to neither: it is still counted in
//! [`ExclusionCounts::unknown_token_components`], which is what keeps the
//! "done when" conservation property whole.
//!
//! This module reuses [`AttributionFraction`] and [`AttributionQualityFloor`]
//! from `crate::attribution::quality` rather than a second ratio type, so
//! the floor comparison this gate and `doctor`'s per-token-kind gate both
//! perform is the exact same code, matching the project's rule that a
//! constant, and the arithmetic that judges it, is defined once and read,
//! never copied.

use std::collections::BTreeMap;

use crate::attribution::account_segment::AccountEvidenceClass;
use crate::attribution::quality::{AttributionFraction, AttributionQualityFloor};
use crate::domain::credits::Credits;
use crate::domain::interval::Interval;
use crate::domain::time::UtcTimestamp;
use crate::evidence::EvidenceQuality;

/// A percentile in `[0, 100]`, the unit every configured quantile in this
/// module is expressed in. Private storage with validated construction,
/// matching this project's rule for every ordinary quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Percentile(u8);

impl Percentile {
    /// Builds a percentile, rejecting anything above 100.
    pub fn new(value: u8) -> Option<Self> {
        (value <= 100).then_some(Self(value))
    }

    /// The percentile as a whole number in `[0, 100]`.
    pub fn value(self) -> u8 {
        self.0
    }
}

/// The algorithm used to read one quantile off a sorted sample. Exhaustive
/// with no wildcard arm: a second method is a deliberate modeling decision
/// to add here, never a silent default for an unrecognized configured
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantileMethod {
    /// The value at position `ceil(p / 100 * n)` of the sorted sample
    /// (`aub-1o3`, decided 2026-09-04): every reported number is a task
    /// that actually happened, which linear interpolation cannot promise,
    /// since it can print a credit figure no task ever cost.
    NearestRank,
}

impl QuantileMethod {
    /// The stable name this method resolves from and renders under.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NearestRank => "nearest-rank",
        }
    }

    /// Parses the stable name, or `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "nearest-rank" => Some(Self::NearestRank),
            _ => None,
        }
    }
}

/// The default quantiles, minimum sample count and attribution-quality
/// floor, read from `crate::config` rather than compiled in as source
/// constants (`aub-1o3`, `aub-cab.7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoricalDistributionConfig {
    pub central_low: Percentile,
    pub central_high: Percentile,
    pub upper: Percentile,
    pub min_samples: usize,
    pub quantile_method: QuantileMethod,
    pub attribution_floor: AttributionQualityFloor,
}

/// Where one completed historical task's usage landed when priced into
/// credits: [`Priced`](Self::Priced) when a cost model resolved it, or the
/// fail-closed refusal `crate::cost_model::convert` returns (`Derivation::Unavailable`)
/// when the usage carried components no cost-model term covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskPricing {
    Priced {
        credits: Credits,
        quality: EvidenceQuality<Credits>,
    },
    UnknownTokenComponents,
}

/// One completed historical task's evidence for the reference distribution,
/// generic over the grouping key `K` (`TaskKind`, the task-size label, or
/// any future grouping key) per `aub-cab.7`'s explicit requirement that this
/// module take the group key as a parameter rather than hardcoding
/// `TaskKind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskHistorySample<K> {
    pub group: K,
    pub pricing: TaskPricing,
    pub account_evidence: AccountEvidenceClass,
    /// `false` when this task's own usage total was assembled from a
    /// session timeline carrying an ambiguity that could have touched it
    /// (`crate::attribution::segment`'s overhead buckets), so the reported
    /// total is not treated as a clean, defensible measurement of this
    /// task alone.
    pub segmentation_complete: bool,
}

/// Why one completed task's usage was excluded from the reference
/// distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ineligibility {
    UnknownTokenComponents,
    UnknownAccountAttribution,
    IncompleteSegmentation,
    EstimatedTokens,
}

impl Ineligibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownTokenComponents => "unknown_token_components",
            Self::UnknownAccountAttribution => "unknown_account_attribution",
            Self::IncompleteSegmentation => "incomplete_segmentation",
            Self::EstimatedTokens => "estimated_tokens",
        }
    }
}

/// Classifies one completed task's evidence against the four default
/// eligibility rules (PLAN.md 26.1), in the fixed precedence documented on
/// this module: pricing first, then account attribution, then
/// segmentation, then token-evidence quality. `None` means the task is
/// eligible and [`TaskPricing::Priced`]'s credits enter the reference
/// distribution.
pub fn ineligibility_reason<K>(sample: &TaskHistorySample<K>) -> Option<Ineligibility> {
    let quality = match &sample.pricing {
        TaskPricing::UnknownTokenComponents => {
            return Some(Ineligibility::UnknownTokenComponents);
        }
        TaskPricing::Priced { quality, .. } => quality,
    };
    if sample.account_evidence == AccountEvidenceClass::Unattributed {
        return Some(Ineligibility::UnknownAccountAttribution);
    }
    if !sample.segmentation_complete {
        return Some(Ineligibility::IncompleteSegmentation);
    }
    if !matches!(quality, EvidenceQuality::Measured) {
        return Some(Ineligibility::EstimatedTokens);
    }
    None
}

/// Per-reason counts of excluded completed tasks. Every completed task in a
/// group lands in exactly one of these four buckets, or is included, never
/// both and never neither: [`ExclusionCounts::total`] plus the included
/// count always equals the number of samples given for that group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExclusionCounts {
    pub unknown_token_components: usize,
    pub unknown_account_attribution: usize,
    pub incomplete_segmentation: usize,
    pub estimated_tokens: usize,
}

impl ExclusionCounts {
    fn record(&mut self, reason: Ineligibility) {
        match reason {
            Ineligibility::UnknownTokenComponents => self.unknown_token_components += 1,
            Ineligibility::UnknownAccountAttribution => self.unknown_account_attribution += 1,
            Ineligibility::IncompleteSegmentation => self.incomplete_segmentation += 1,
            Ineligibility::EstimatedTokens => self.estimated_tokens += 1,
        }
    }

    /// The total excluded count, across all four reasons.
    pub fn total(&self) -> usize {
        self.unknown_token_components
            + self.unknown_account_attribution
            + self.incomplete_segmentation
            + self.estimated_tokens
    }
}

/// The attribution-quality gate for one group: a canonical-usage fraction
/// (`aub-cab.7`, decided 2026-09-04, option B) and the configured floor it
/// is judged against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributionCoverage {
    pub fraction: AttributionFraction,
    pub floor: AttributionQualityFloor,
}

impl AttributionCoverage {
    /// True when the fraction is strictly below the configured floor.
    pub fn is_below_floor(&self) -> bool {
        self.fraction.is_below(self.floor)
    }
}

/// The historical selection window a report was computed over. Carried on
/// every report, sufficient or not, so the selection period is stated
/// everywhere the statistics appear (PLAN.md 26.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPeriod {
    pub start: UtcTimestamp,
    pub end: UtcTimestamp,
}

/// The distribution itself, or the refusal to compute one over too few
/// samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributionVerdict {
    Distribution {
        median: Credits,
        central_range: Interval<Credits>,
        central_low_percentile: Percentile,
        central_high_percentile: Percentile,
        upper_reference: Credits,
        upper_percentile: Percentile,
        quantile_method: QuantileMethod,
    },
    /// Fewer than `min_samples` eligible completed tasks were available: an
    /// empirical distribution over that few points would not be a
    /// meaningful "usual case", so none is reported (PLAN.md 26.6).
    InsufficientEvidence { min_samples: usize },
}

/// One group's full historical-distribution report: always carries the
/// selection period, the eligible sample count, the exclusion counts and
/// the attribution-quality coverage, regardless of whether the verdict is a
/// distribution or insufficient evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupHistoryReport<K> {
    pub group: K,
    pub period: SelectionPeriod,
    /// The eligible sample count: tasks that entered the reference
    /// distribution, never the total completed-task count for the group.
    pub sample_count: usize,
    pub exclusions: ExclusionCounts,
    pub attribution: AttributionCoverage,
    pub verdict: DistributionVerdict,
}

impl<K> GroupHistoryReport<K> {
    /// A human-readable summary in empirical-history language only: no
    /// wording here ever claims to predict or forecast the next task
    /// (PLAN.md 26.2's own labelling constraint). The selection period and
    /// sample count are stated first, before either verdict branch, so
    /// they appear in both.
    pub fn describe(&self) -> String {
        let period = format!(
            "{}..{}",
            self.period.start.utc_date().iso(),
            self.period.end.utc_date().iso()
        );
        let mut out = format!(
            "historical task evidence: {} eligible completed sample(s), selection period {period}\n",
            self.sample_count
        );
        match &self.verdict {
            DistributionVerdict::Distribution {
                median,
                central_range,
                central_low_percentile,
                central_high_percentile,
                upper_reference,
                upper_percentile,
                quantile_method,
            } => {
                out.push_str(&format!(
                    "median {}; observed range p{}-p{}: {}-{}; p{} upper historical reference: {} (method: {})\n",
                    format_credits(*median),
                    central_low_percentile.value(),
                    central_high_percentile.value(),
                    format_credits(central_range.lower()),
                    format_credits(central_range.upper()),
                    upper_percentile.value(),
                    format_credits(*upper_reference),
                    quantile_method.as_str(),
                ));
            }
            DistributionVerdict::InsufficientEvidence { min_samples } => {
                out.push_str(&format!(
                    "insufficient evidence: fewer than {min_samples} eligible completed sample(s)\n"
                ));
            }
        }
        out.push_str(&format!(
            "excluded: estimated_tokens={}, unknown_account_attribution={}, unknown_token_components={}, incomplete_segmentation={}\n",
            self.exclusions.estimated_tokens,
            self.exclusions.unknown_account_attribution,
            self.exclusions.unknown_token_components,
            self.exclusions.incomplete_segmentation,
        ));
        out.push_str(&format!(
            "attribution coverage: {}/{} micro-credits, floor {:.2}\n",
            self.attribution.fraction.numerator(),
            self.attribution.fraction.denominator(),
            self.attribution.floor.as_f64(),
        ));
        out
    }
}

/// Formats a credit amount for this module's own descriptive text only.
/// `Credits` deliberately carries no free-standing `Display` (rendering for
/// a user belongs to a presentation helper with explicit context); this
/// stays local to `describe`'s plain-text summary, which `crate::advice`
/// may not reach `crate::presentation` to build.
fn format_credits(credits: Credits) -> String {
    format!("{:.6}cr", credits.micros() as f64 / 1_000_000.0)
}

/// The value at nearest-rank position `ceil(p / 100 * n)` of a sample
/// already sorted ascending. `debug_assert`s the sample is non-empty:
/// callers only reach this after checking the group's sample count against
/// `min_samples`.
fn nearest_rank(sorted_ascending: &[Credits], percentile: Percentile) -> Credits {
    let n = sorted_ascending.len();
    debug_assert!(n > 0, "nearest_rank requires a non-empty sample");
    let p = u64::from(percentile.value());
    let n64 = n as u64;
    let rank = (p * n64).div_ceil(100).clamp(1, n64);
    sorted_ascending[(rank - 1) as usize]
}

/// A completed task's credits sum can never be negative: `Credits` has no
/// upstream path from a negative usage quantity, so a caller that manages
/// to construct one has a defect elsewhere. Panicking here surfaces that
/// defect rather than silently reporting the fraction as "no usage",
/// which is indistinguishable from a real zero (AGENTS.md's fifth
/// correctness invariant).
fn non_negative_micros(micros: i64) -> u64 {
    u64::try_from(micros).expect("a sum of historical task credits must be non-negative")
}

struct GroupAccumulator {
    included: Vec<Credits>,
    exclusions: ExclusionCounts,
    numerator_micros: i64,
    denominator_micros: i64,
}

impl GroupAccumulator {
    fn new() -> Self {
        Self {
            included: Vec::new(),
            exclusions: ExclusionCounts::default(),
            numerator_micros: 0,
            denominator_micros: 0,
        }
    }
}

/// Builds one [`GroupHistoryReport`] per distinct group key present in
/// `samples`. A group absent from `samples` never appears in the result:
/// this function reports on the completed tasks it was given, and does not
/// invent a zero-sample report for a group nobody asked about.
pub fn build_group_reports<K: Ord + Clone>(
    samples: impl IntoIterator<Item = TaskHistorySample<K>>,
    period: SelectionPeriod,
    config: &HistoricalDistributionConfig,
) -> BTreeMap<K, GroupHistoryReport<K>> {
    let mut by_group: BTreeMap<K, GroupAccumulator> = BTreeMap::new();

    for sample in samples {
        let group = sample.group.clone();
        let priced_credits = match &sample.pricing {
            TaskPricing::Priced { credits, .. } => Some(*credits),
            TaskPricing::UnknownTokenComponents => None,
        };
        let reason = ineligibility_reason(&sample);
        let entry = by_group.entry(group).or_insert_with(GroupAccumulator::new);
        match reason {
            None => {
                let credits = priced_credits.expect("an eligible sample is always priced");
                entry.included.push(credits);
                entry.numerator_micros += credits.micros();
                entry.denominator_micros += credits.micros();
            }
            Some(reason) => {
                entry.exclusions.record(reason);
                if let Some(credits) = priced_credits {
                    entry.denominator_micros += credits.micros();
                }
            }
        }
    }

    by_group
        .into_iter()
        .map(|(group, mut acc)| {
            acc.included.sort();
            let sample_count = acc.included.len();
            let attribution = AttributionCoverage {
                fraction: AttributionFraction::new(
                    non_negative_micros(acc.numerator_micros),
                    non_negative_micros(acc.denominator_micros),
                ),
                floor: config.attribution_floor,
            };
            let verdict = if sample_count < config.min_samples {
                DistributionVerdict::InsufficientEvidence {
                    min_samples: config.min_samples,
                }
            } else {
                let median_percentile = Percentile::new(50).expect("50 is a valid percentile");
                let median = nearest_rank(&acc.included, median_percentile);
                let central_low = nearest_rank(&acc.included, config.central_low);
                let central_high = nearest_rank(&acc.included, config.central_high);
                let upper_reference = nearest_rank(&acc.included, config.upper);
                DistributionVerdict::Distribution {
                    median,
                    central_range: Interval::new(central_low, central_high).expect(
                        "central_low's percentile is <= central_high's, and nearest_rank is \
                         monotonic in the percentile, so central_low <= central_high",
                    ),
                    central_low_percentile: config.central_low,
                    central_high_percentile: config.central_high,
                    upper_reference,
                    upper_percentile: config.upper,
                    quantile_method: config.quantile_method,
                }
            };
            let report = GroupHistoryReport {
                group: group.clone(),
                period,
                sample_count,
                exclusions: acc.exclusions,
                attribution,
                verdict,
            };
            (group, report)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn config() -> HistoricalDistributionConfig {
        HistoricalDistributionConfig {
            central_low: Percentile::new(25).unwrap(),
            central_high: Percentile::new(75).unwrap(),
            upper: Percentile::new(90).unwrap(),
            min_samples: 12,
            quantile_method: QuantileMethod::NearestRank,
            attribution_floor: AttributionQualityFloor::new(0.80).unwrap(),
        }
    }

    fn period() -> SelectionPeriod {
        SelectionPeriod {
            start: UtcTimestamp::from_unix_nanos(0),
            end: UtcTimestamp::from_unix_nanos(1_000_000_000_000),
        }
    }

    fn measured(credits_value: i64) -> TaskPricing {
        TaskPricing::Priced {
            credits: Credits::from_micros(credits_value),
            quality: EvidenceQuality::Measured,
        }
    }

    fn eligible_sample(group: u8, credits_value: i64) -> TaskHistorySample<u8> {
        TaskHistorySample {
            group,
            pricing: measured(credits_value),
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        }
    }

    // --- quantile computation: hand-computed expected values ---------------

    #[test]
    fn nearest_rank_matches_hand_computed_values_over_twelve_samples() {
        // Twelve values, already sorted ascending: 100, 200, ..., 1200 credits
        // (in whole-credit micros for readability).
        let sorted: Vec<Credits> = (1..=12)
            .map(|n| Credits::from_micros(n * 100 * 1_000_000))
            .collect();

        // p25: ceil(0.25 * 12) = 3 -> the 3rd value, 300 credits.
        assert_eq!(
            nearest_rank(&sorted, Percentile::new(25).unwrap()),
            Credits::from_micros(300 * 1_000_000)
        );
        // p50 (median): ceil(0.5 * 12) = 6 -> the 6th value, 600 credits.
        assert_eq!(
            nearest_rank(&sorted, Percentile::new(50).unwrap()),
            Credits::from_micros(600 * 1_000_000)
        );
        // p75: ceil(0.75 * 12) = 9 -> the 9th value, 900 credits.
        assert_eq!(
            nearest_rank(&sorted, Percentile::new(75).unwrap()),
            Credits::from_micros(900 * 1_000_000)
        );
        // p90: ceil(0.9 * 12) = ceil(10.8) = 11 -> the 11th value, 1100 credits.
        assert_eq!(
            nearest_rank(&sorted, Percentile::new(90).unwrap()),
            Credits::from_micros(1_100 * 1_000_000)
        );
        // The planted negative: a naive floor/round implementation would put
        // p90 at the 10th or 12th value (1000 or 1200), never the 11th.
        assert_ne!(
            nearest_rank(&sorted, Percentile::new(90).unwrap()),
            Credits::from_micros(1_000 * 1_000_000)
        );
        assert_ne!(
            nearest_rank(&sorted, Percentile::new(90).unwrap()),
            Credits::from_micros(1_200 * 1_000_000)
        );
    }

    #[test]
    fn build_group_reports_reports_the_hand_computed_median_and_quantiles() {
        let cfg = config();
        // The same twelve values as the nearest-rank test above, one sample
        // per group member, all eligible.
        let samples: Vec<TaskHistorySample<u8>> = (1..=12)
            .map(|n| eligible_sample(0, n * 100 * 1_000_000))
            .collect();

        let reports = build_group_reports(samples, period(), &cfg);
        let report = reports.get(&0).unwrap();
        assert_eq!(report.sample_count, 12);
        let DistributionVerdict::Distribution {
            median,
            central_range,
            upper_reference,
            ..
        } = report.verdict
        else {
            panic!("expected a distribution over twelve eligible samples");
        };
        assert_eq!(median, Credits::from_micros(600 * 1_000_000));
        assert_eq!(central_range.lower(), Credits::from_micros(300 * 1_000_000));
        assert_eq!(central_range.upper(), Credits::from_micros(900 * 1_000_000));
        assert_eq!(upper_reference, Credits::from_micros(1_100 * 1_000_000));
    }

    #[test]
    fn describe_states_the_selection_period_and_sample_count_for_both_verdicts() {
        let cfg = config();
        let sufficient: Vec<TaskHistorySample<u8>> =
            (0..12).map(|i| eligible_sample(0, 100 + i)).collect();
        let insufficient: Vec<TaskHistorySample<u8>> =
            (0..3).map(|i| eligible_sample(1, 100 + i)).collect();
        let mut samples = sufficient;
        samples.extend(insufficient);

        let reports = build_group_reports(samples, period(), &cfg);

        let sufficient_text = reports.get(&0).unwrap().describe();
        assert!(sufficient_text.contains("12 eligible completed sample"));
        assert!(sufficient_text.contains(&period().start.utc_date().iso()));
        assert!(sufficient_text.contains(&period().end.utc_date().iso()));

        let insufficient_text = reports.get(&1).unwrap().describe();
        assert!(insufficient_text.contains("3 eligible completed sample"));
        assert!(insufficient_text.contains(&period().start.utc_date().iso()));
        assert!(insufficient_text.contains(&period().end.utc_date().iso()));
    }

    // --- one exclusion case per eligibility rule ----------------------------

    #[test]
    fn unknown_token_components_is_excluded_for_that_reason() {
        let sample = TaskHistorySample {
            group: 0u8,
            pricing: TaskPricing::UnknownTokenComponents,
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        };
        assert_eq!(
            ineligibility_reason(&sample),
            Some(Ineligibility::UnknownTokenComponents)
        );
    }

    #[test]
    fn unknown_account_attribution_is_excluded_for_that_reason() {
        let sample = TaskHistorySample {
            group: 0u8,
            pricing: measured(100),
            account_evidence: AccountEvidenceClass::Unattributed,
            segmentation_complete: true,
        };
        assert_eq!(
            ineligibility_reason(&sample),
            Some(Ineligibility::UnknownAccountAttribution)
        );
    }

    #[test]
    fn incomplete_segmentation_is_excluded_for_that_reason() {
        let sample = TaskHistorySample {
            group: 0u8,
            pricing: measured(100),
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: false,
        };
        assert_eq!(
            ineligibility_reason(&sample),
            Some(Ineligibility::IncompleteSegmentation)
        );
    }

    #[test]
    fn estimated_tokens_is_excluded_for_that_reason() {
        let sample = TaskHistorySample {
            group: 0u8,
            pricing: TaskPricing::Priced {
                credits: Credits::from_micros(100),
                quality: EvidenceQuality::estimated([], None),
            },
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        };
        assert_eq!(
            ineligibility_reason(&sample),
            Some(Ineligibility::EstimatedTokens)
        );
    }

    #[test]
    fn a_sample_meeting_every_rule_is_eligible() {
        let sample = eligible_sample(0, 100);
        assert_eq!(ineligibility_reason(&sample), None);
    }

    // --- minimum sample count: insufficient evidence, not a thin distribution ---

    #[test]
    fn fewer_than_the_minimum_eligible_samples_reports_insufficient_evidence() {
        let mut small_config = config();
        small_config.min_samples = 12;
        let samples: Vec<TaskHistorySample<u8>> =
            (0..3).map(|i| eligible_sample(0, 100 + i)).collect();

        let reports = build_group_reports(samples, period(), &small_config);
        let report = reports.get(&0).unwrap();
        assert_eq!(report.sample_count, 3);
        assert_eq!(
            report.verdict,
            DistributionVerdict::InsufficientEvidence { min_samples: 12 }
        );
    }

    #[test]
    fn at_least_the_minimum_eligible_samples_reports_a_distribution() {
        let mut small_config = config();
        small_config.min_samples = 3;
        let samples: Vec<TaskHistorySample<u8>> =
            (0..3).map(|i| eligible_sample(0, 100 + i)).collect();

        let reports = build_group_reports(samples, period(), &small_config);
        let report = reports.get(&0).unwrap();
        assert_eq!(report.sample_count, 3);
        assert!(matches!(
            report.verdict,
            DistributionVerdict::Distribution { .. }
        ));
    }

    // --- configured quantiles are visible in the output ---------------------

    #[test]
    fn configured_percentiles_change_which_quantiles_are_reported_and_printed() {
        let default_config = config();
        let mut custom_config = config();
        custom_config.central_low = Percentile::new(10).unwrap();
        custom_config.central_high = Percentile::new(90).unwrap();
        custom_config.upper = Percentile::new(95).unwrap();
        custom_config.min_samples = 12;

        let samples: Vec<TaskHistorySample<u8>> =
            (0..12).map(|i| eligible_sample(0, 100 * (i + 1))).collect();

        let default_reports = build_group_reports(samples.clone(), period(), &default_config);
        let custom_reports = build_group_reports(samples, period(), &custom_config);

        let DistributionVerdict::Distribution {
            central_low_percentile: default_low,
            upper_percentile: default_upper,
            ..
        } = default_reports.get(&0).unwrap().verdict
        else {
            panic!("expected a distribution");
        };
        let DistributionVerdict::Distribution {
            central_low_percentile: custom_low,
            upper_percentile: custom_upper,
            ..
        } = custom_reports.get(&0).unwrap().verdict
        else {
            panic!("expected a distribution");
        };
        assert_eq!(default_low.value(), 25);
        assert_eq!(default_upper.value(), 90);
        assert_eq!(custom_low.value(), 10);
        assert_eq!(custom_upper.value(), 95);
        assert_ne!(default_low, custom_low);

        let custom_text = custom_reports.get(&0).unwrap().describe();
        assert!(
            custom_text.contains("p10-p90"),
            "configured percentiles must appear in the rendered text: {custom_text}"
        );
        assert!(
            custom_text.contains("p95 upper"),
            "the configured upper percentile must appear in the rendered text: {custom_text}"
        );
    }

    // --- no output describes the distribution as a prediction or forecast ---

    #[test]
    fn rendered_text_never_claims_to_predict_or_forecast() {
        let cfg = config();
        let sufficient: Vec<TaskHistorySample<u8>> =
            (0..12).map(|i| eligible_sample(0, 100 * (i + 1))).collect();
        let insufficient: Vec<TaskHistorySample<u8>> = vec![eligible_sample(1, 100)];

        let mut samples = sufficient;
        samples.extend(insufficient);
        let reports = build_group_reports(samples, period(), &cfg);

        for report in reports.values() {
            let text = report.describe().to_lowercase();
            assert!(
                !text.contains("predict"),
                "rendered text must never claim to predict: {text}"
            );
            assert!(
                !text.contains("forecast"),
                "rendered text must never claim to forecast: {text}"
            );
        }
    }

    // --- attribution-quality floor: below, at, and above -------------------

    #[test]
    fn attribution_coverage_is_exact_below_at_and_above_the_floor() {
        let cfg = config();
        // Denominator 100, numerator 79: fraction 0.79, strictly below 0.80.
        let below = AttributionCoverage {
            fraction: AttributionFraction::new(79, 100),
            floor: cfg.attribution_floor,
        };
        assert!(below.is_below_floor());

        // Exactly at the floor: 80 of 100.
        let at = AttributionCoverage {
            fraction: AttributionFraction::new(80, 100),
            floor: cfg.attribution_floor,
        };
        assert!(!at.is_below_floor());

        // Above the floor: 81 of 100.
        let above = AttributionCoverage {
            fraction: AttributionFraction::new(81, 100),
            floor: cfg.attribution_floor,
        };
        assert!(!above.is_below_floor());
    }

    #[test]
    fn build_group_reports_files_each_excluded_sample_under_its_own_reason() {
        let cfg = config();
        let samples = vec![
            TaskHistorySample {
                group: 0u8,
                pricing: TaskPricing::UnknownTokenComponents,
                account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
                segmentation_complete: true,
            },
            TaskHistorySample {
                group: 0,
                pricing: measured(100),
                account_evidence: AccountEvidenceClass::Unattributed,
                segmentation_complete: true,
            },
            TaskHistorySample {
                group: 0,
                pricing: measured(100),
                account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
                segmentation_complete: false,
            },
            TaskHistorySample {
                group: 0,
                pricing: TaskPricing::Priced {
                    credits: Credits::from_micros(100),
                    quality: EvidenceQuality::estimated([], None),
                },
                account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
                segmentation_complete: true,
            },
        ];

        let reports = build_group_reports(samples, period(), &cfg);
        let exclusions = reports.get(&0).unwrap().exclusions;
        // The planted negative: each count is exactly 1 in its own bucket
        // and 0 everywhere else, so a reason misfiled into the wrong bucket
        // (e.g. an estimated-tokens sample counted as incomplete
        // segmentation) fails this even though the total stays 4.
        assert_eq!(exclusions.unknown_token_components, 1);
        assert_eq!(exclusions.unknown_account_attribution, 1);
        assert_eq!(exclusions.incomplete_segmentation, 1);
        assert_eq!(exclusions.estimated_tokens, 1);
    }

    #[test]
    fn attribution_coverage_denominator_includes_excluded_but_priced_tasks() {
        let cfg = config();
        let mut samples: Vec<TaskHistorySample<u8>> =
            (0..12).map(|i| eligible_sample(0, 100 * (i + 1))).collect();
        // One more priced task, excluded for unknown account attribution:
        // it must count in the denominator but not the numerator.
        samples.push(TaskHistorySample {
            group: 0,
            pricing: measured(500),
            account_evidence: AccountEvidenceClass::Unattributed,
            segmentation_complete: true,
        });
        // One unpriced task: contributes to neither sum.
        samples.push(TaskHistorySample {
            group: 0,
            pricing: TaskPricing::UnknownTokenComponents,
            account_evidence: AccountEvidenceClass::ExplicitLauncherOrHook,
            segmentation_complete: true,
        });

        let reports = build_group_reports(samples, period(), &cfg);
        let report = reports.get(&0).unwrap();
        let expected_numerator: u64 = (1..=12u64).map(|i| 100 * i).sum();
        let expected_denominator = expected_numerator + 500;
        assert_eq!(report.attribution.fraction.numerator(), expected_numerator);
        assert_eq!(
            report.attribution.fraction.denominator(),
            expected_denominator
        );
        assert_eq!(report.exclusions.unknown_account_attribution, 1);
        assert_eq!(report.exclusions.unknown_token_components, 1);
    }

    // --- property: exclusions plus included equals total, over generated histories ---

    fn arb_pricing() -> impl Strategy<Value = TaskPricing> {
        prop_oneof![
            Just(TaskPricing::UnknownTokenComponents),
            (1i64..1000, any::<bool>()).prop_map(|(value, measured)| TaskPricing::Priced {
                credits: Credits::from_micros(value),
                quality: if measured {
                    EvidenceQuality::Measured
                } else {
                    EvidenceQuality::estimated([], None)
                },
            }),
        ]
    }

    fn arb_account_evidence() -> impl Strategy<Value = AccountEvidenceClass> {
        prop_oneof![
            Just(AccountEvidenceClass::ExplicitLauncherOrHook),
            Just(AccountEvidenceClass::Unattributed),
        ]
    }

    fn arb_sample() -> impl Strategy<Value = TaskHistorySample<u8>> {
        (0u8..3, arb_pricing(), arb_account_evidence(), any::<bool>()).prop_map(
            |(group, pricing, account_evidence, segmentation_complete)| TaskHistorySample {
                group,
                pricing,
                account_evidence,
                segmentation_complete,
            },
        )
    }

    proptest! {
        #[test]
        fn prop_exclusions_plus_included_equals_total_completed_tasks_per_group(
            samples in proptest::collection::vec(arb_sample(), 0..50),
        ) {
            let cfg = config();
            let mut totals: BTreeMap<u8, usize> = BTreeMap::new();
            for sample in &samples {
                *totals.entry(sample.group).or_insert(0) += 1;
            }

            let reports = build_group_reports(samples, period(), &cfg);
            for (group, total) in totals {
                let report = reports.get(&group).expect("a group with samples has a report");
                prop_assert_eq!(
                    report.exclusions.total() + report.sample_count,
                    total,
                    "exclusions plus included must equal total completed tasks for group {}",
                    group
                );
            }
        }
    }
}
