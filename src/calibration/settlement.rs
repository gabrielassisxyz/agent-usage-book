//! Pure settled-boundary detection for provider meter observation series.
//!
//! Provider accounting can continue to catch up after controlled work stops. This
//! module waits for a recorded plateau rather than assigning individual meter
//! increments to requests. It has no persistence dependency: callers provide the
//! observations, coverage fact, and immutable experiment policy, and receive either
//! a settled boundary or an incomplete outcome.

use std::fmt;

use crate::domain::quota::QuotaUsed;
use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::domain::window::ReportedResolution;

const DEFAULT_POLICY_VERSION: &str = "settled-boundary-v1";
const DEFAULT_SAMPLING_INTERVAL_NANOS: u64 = 300_000_000_000;
const DEFAULT_REQUIRED_OBSERVATIONS: u32 = 3;
const DEFAULT_MINIMUM_SPAN_NANOS: u64 = 600_000_000_000;
const DEFAULT_MAXIMUM_CHANGE_RESOLUTION_UNITS: u32 = 0;
const DEFAULT_MAXIMUM_SETTLEMENT_WINDOW_NANOS: u64 = 3_600_000_000_000;

/// Whether the detector is evaluating the pre-work baseline or post-work terminal
/// boundary. The two roles are separate policy slots even when a provider currently
/// uses the same values for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementRole {
    Baseline,
    Terminal,
}

/// A validation failure while constructing a settlement policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementPolicyError {
    EmptyVersion,
    TooFewObservations,
    ZeroSamplingInterval,
    ZeroMinimumSpan,
    ZeroMaximumSettlementWindow,
    SettlementWindowBeforeMinimumSpan,
    SharedCriteriaReasonRequired,
    EmptySharedCriteriaReason,
}

impl fmt::Display for SettlementPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyVersion => "settlement policy version cannot be empty",
            Self::TooFewObservations => "settlement criterion requires at least two observations",
            Self::ZeroSamplingInterval => "settlement sampling interval must be non-zero",
            Self::ZeroMinimumSpan => "settlement minimum span must be non-zero",
            Self::ZeroMaximumSettlementWindow => "settlement maximum window must be non-zero",
            Self::SettlementWindowBeforeMinimumSpan => {
                "settlement maximum window must cover the minimum span"
            }
            Self::SharedCriteriaReasonRequired => {
                "equal baseline and terminal criteria require a sharing reason"
            }
            Self::EmptySharedCriteriaReason => "settlement sharing reason cannot be empty",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SettlementPolicyError {}

/// The complete criterion recorded for one experiment boundary role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementCriterion {
    sampling_interval: MonotonicDuration,
    required_observations: u32,
    minimum_span: MonotonicDuration,
    maximum_change_resolution_units: u32,
    maximum_settlement_window: MonotonicDuration,
    reported_resolution: ReportedResolution,
}

impl SettlementCriterion {
    /// Constructs a criterion with explicit timing, count, tolerance, and provider
    /// resolution. A zero count of readings or an unbounded settlement window would
    /// make a plateau claim meaningless, so both are rejected here.
    pub fn new(
        sampling_interval: MonotonicDuration,
        required_observations: u32,
        minimum_span: MonotonicDuration,
        maximum_change_resolution_units: u32,
        maximum_settlement_window: MonotonicDuration,
        reported_resolution: ReportedResolution,
    ) -> Result<Self, SettlementPolicyError> {
        if required_observations < 2 {
            return Err(SettlementPolicyError::TooFewObservations);
        }
        if sampling_interval.as_nanos() == 0 {
            return Err(SettlementPolicyError::ZeroSamplingInterval);
        }
        if minimum_span.as_nanos() == 0 {
            return Err(SettlementPolicyError::ZeroMinimumSpan);
        }
        if maximum_settlement_window.as_nanos() == 0 {
            return Err(SettlementPolicyError::ZeroMaximumSettlementWindow);
        }
        if maximum_settlement_window.as_nanos() < minimum_span.as_nanos() {
            return Err(SettlementPolicyError::SettlementWindowBeforeMinimumSpan);
        }
        Ok(Self {
            sampling_interval,
            required_observations,
            minimum_span,
            maximum_change_resolution_units,
            maximum_settlement_window,
            reported_resolution,
        })
    }

    pub fn sampling_interval(self) -> MonotonicDuration {
        self.sampling_interval
    }

    pub fn required_observations(self) -> u32 {
        self.required_observations
    }

    pub fn minimum_span(self) -> MonotonicDuration {
        self.minimum_span
    }

    /// Maximum whole provider-resolution steps allowed between any two readings
    /// in the candidate plateau. The conservative default is zero: changes smaller
    /// than one reported step are tolerated, while one complete step is material.
    pub fn maximum_change_resolution_units(self) -> u32 {
        self.maximum_change_resolution_units
    }

    pub fn maximum_settlement_window(self) -> MonotonicDuration {
        self.maximum_settlement_window
    }

    /// Alias used by callers that describe the same value as the maximum window.
    pub fn maximum_window(self) -> MonotonicDuration {
        self.maximum_settlement_window()
    }

    pub fn reported_resolution(self) -> ReportedResolution {
        self.reported_resolution
    }
}

/// The immutable settlement policy snapshot attached to one experiment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementPolicy {
    version: String,
    baseline: SettlementCriterion,
    terminal: SettlementCriterion,
    shared_criteria_reason: Option<String>,
}

impl SettlementPolicy {
    /// Creates a policy with independently recorded baseline and terminal criteria.
    /// Equal criteria are allowed only with an explicit explanation because sharing
    /// a value is a policy decision that must remain auditable.
    pub fn new(
        version: impl Into<String>,
        baseline: SettlementCriterion,
        terminal: SettlementCriterion,
        shared_criteria_reason: Option<String>,
    ) -> Result<Self, SettlementPolicyError> {
        let version = version.into();
        if version.trim().is_empty() {
            return Err(SettlementPolicyError::EmptyVersion);
        }
        let shared_criteria_reason = shared_criteria_reason.map(|reason| {
            let trimmed = reason.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                trimmed.to_string()
            }
        });
        if shared_criteria_reason
            .as_ref()
            .is_some_and(|reason| reason.is_empty())
        {
            return Err(SettlementPolicyError::EmptySharedCriteriaReason);
        }
        if baseline == terminal && shared_criteria_reason.is_none() {
            return Err(SettlementPolicyError::SharedCriteriaReasonRequired);
        }
        Ok(Self {
            version,
            baseline,
            terminal,
            shared_criteria_reason,
        })
    }

    /// The conservative provider-resolution policy from the settled-boundary design
    /// decision: three readings, five minutes apart, spanning at least ten minutes,
    /// with no complete reported-resolution step over a sixty-minute window.
    pub fn conservative_default(reported_resolution: ReportedResolution) -> Self {
        let criterion = SettlementCriterion::new(
            MonotonicDuration::from_nanos(DEFAULT_SAMPLING_INTERVAL_NANOS),
            DEFAULT_REQUIRED_OBSERVATIONS,
            MonotonicDuration::from_nanos(DEFAULT_MINIMUM_SPAN_NANOS),
            DEFAULT_MAXIMUM_CHANGE_RESOLUTION_UNITS,
            MonotonicDuration::from_nanos(DEFAULT_MAXIMUM_SETTLEMENT_WINDOW_NANOS),
            reported_resolution,
        )
        .expect("the built-in settlement policy is valid");
        Self::new(
            DEFAULT_POLICY_VERSION,
            criterion,
            criterion,
            Some("baseline and terminal share the conservative provider-lag criterion".into()),
        )
        .expect("the built-in settlement policy is valid")
    }

    /// Constructs the fixed default policy for a provider's reported resolution.
    pub fn default_for_resolution(reported_resolution: ReportedResolution) -> Self {
        Self::conservative_default(reported_resolution)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn baseline(&self) -> SettlementCriterion {
        self.baseline
    }

    pub fn terminal(&self) -> SettlementCriterion {
        self.terminal
    }

    pub fn shared_criteria_reason(&self) -> Option<&str> {
        self.shared_criteria_reason.as_deref()
    }

    pub fn criterion(&self, role: SettlementRole) -> SettlementCriterion {
        match role {
            SettlementRole::Baseline => self.baseline,
            SettlementRole::Terminal => self.terminal,
        }
    }
}

/// One provider meter reading used by the settlement detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementMeterObservation {
    at: UtcTimestamp,
    quota_used: QuotaUsed,
    reported_resolution: ReportedResolution,
}

impl SettlementMeterObservation {
    pub fn new(
        at: UtcTimestamp,
        quota_used: QuotaUsed,
        reported_resolution: ReportedResolution,
    ) -> Self {
        Self {
            at,
            quota_used,
            reported_resolution,
        }
    }

    pub fn at(self) -> UtcTimestamp {
        self.at
    }

    pub fn quota_used(self) -> QuotaUsed {
        self.quota_used
    }

    pub fn reported_resolution(self) -> ReportedResolution {
        self.reported_resolution
    }
}

/// The coverage fact accompanying a meter series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementCoverage {
    Complete,
    Insufficient {
        expected_observations: u32,
        observed_observations: u32,
    },
}

impl SettlementCoverage {
    pub const fn complete() -> Self {
        Self::Complete
    }

    pub const fn insufficient(expected_observations: u32, observed_observations: u32) -> Self {
        Self::Insufficient {
            expected_observations,
            observed_observations,
        }
    }

    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    pub const fn expected_observations(self) -> Option<u32> {
        match self {
            Self::Complete => None,
            Self::Insufficient {
                expected_observations,
                ..
            } => Some(expected_observations),
        }
    }

    pub const fn observed_observations(self) -> Option<u32> {
        match self {
            Self::Complete => None,
            Self::Insufficient {
                observed_observations,
                ..
            } => Some(observed_observations),
        }
    }
}

/// A timestamped series plus the independently established coverage fact for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementObservationSeries {
    started_at: UtcTimestamp,
    observations: Vec<SettlementMeterObservation>,
    coverage: SettlementCoverage,
}

impl SettlementObservationSeries {
    pub fn new(
        started_at: UtcTimestamp,
        observations: Vec<SettlementMeterObservation>,
        coverage: SettlementCoverage,
    ) -> Self {
        Self {
            started_at,
            observations,
            coverage,
        }
    }

    pub fn complete(
        started_at: UtcTimestamp,
        observations: Vec<SettlementMeterObservation>,
    ) -> Self {
        Self::new(started_at, observations, SettlementCoverage::Complete)
    }

    pub fn started_at(&self) -> UtcTimestamp {
        self.started_at
    }

    pub fn observations(&self) -> &[SettlementMeterObservation] {
        &self.observations
    }

    pub fn coverage(&self) -> SettlementCoverage {
        self.coverage
    }
}

/// Why a boundary did not settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteSettlementReason {
    InsufficientMeterCoverage,
    NoPlateauWithinWindow,
}

/// The first observation at which the required stable window is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledBoundary {
    observation_index: usize,
    observed_at: UtcTimestamp,
    quota_used: QuotaUsed,
}

impl SettledBoundary {
    pub fn observation_index(self) -> usize {
        self.observation_index
    }

    pub fn observed_at(self) -> UtcTimestamp {
        self.observed_at
    }

    pub fn quota_used(self) -> QuotaUsed {
        self.quota_used
    }
}

/// The detector's exhaustive result for one boundary role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementOutcome {
    Settled {
        boundary: SettledBoundary,
    },
    Incomplete {
        reason: IncompleteSettlementReason,
        deadline: UtcTimestamp,
        observed_until: Option<UtcTimestamp>,
    },
}

impl SettlementOutcome {
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::Settled { .. })
    }

    pub fn is_incomplete(&self) -> bool {
        matches!(self, Self::Incomplete { .. })
    }

    pub fn boundary(&self) -> Option<SettledBoundary> {
        match self {
            Self::Settled { boundary } => Some(*boundary),
            Self::Incomplete { .. } => None,
        }
    }

    pub fn incomplete_reason(&self) -> Option<IncompleteSettlementReason> {
        match self {
            Self::Settled { .. } => None,
            Self::Incomplete { reason, .. } => Some(*reason),
        }
    }
}

/// Both role outcomes for one experiment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentSettlement {
    pub baseline: SettlementOutcome,
    pub terminal: SettlementOutcome,
}

impl ExperimentSettlement {
    pub fn is_incomplete(&self) -> bool {
        self.baseline.is_incomplete() || self.terminal.is_incomplete()
    }

    /// Returns the two settled boundaries only when a fitter may publish a result.
    /// An incomplete terminal interval therefore has no fit-shaped fallback value.
    pub fn publishable_fit(&self) -> Option<SettledExperimentBoundaries> {
        Some(SettledExperimentBoundaries {
            baseline: self.baseline.boundary()?,
            terminal: self.terminal.boundary()?,
        })
    }

    pub fn published_fit(&self) -> Option<SettledExperimentBoundaries> {
        self.publishable_fit()
    }
}

/// The settled endpoints a fitter may consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledExperimentBoundaries {
    pub baseline: SettledBoundary,
    pub terminal: SettledBoundary,
}

/// Detects the earliest complete plateau for one role.
///
/// The input series is never reordered in place. Observations must be covered by an
/// upstream evidence check; an explicit insufficient-coverage fact always wins over
/// apparently stable readings. A candidate requires the configured observation count,
/// minimum first-to-last span, minimum spacing, and pairwise resolution-relative
/// stability, all before the recorded deadline.
pub fn detect_settlement(
    series: &SettlementObservationSeries,
    policy: &SettlementPolicy,
    role: SettlementRole,
) -> SettlementOutcome {
    let criterion = policy.criterion(role);
    let deadline = settlement_deadline(series.started_at, criterion.maximum_settlement_window());
    let observed_until = series
        .observations
        .iter()
        .map(|observation| observation.at)
        .filter(|at| *at >= series.started_at)
        .max();

    if !series.coverage.is_complete() {
        return SettlementOutcome::Incomplete {
            reason: IncompleteSettlementReason::InsufficientMeterCoverage,
            deadline,
            observed_until,
        };
    }

    let mut observations: Vec<(usize, SettlementMeterObservation)> = series
        .observations
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, observation)| {
            observation.at >= series.started_at && observation.at <= deadline
        })
        .collect();
    observations.sort_by_key(|(_, observation)| observation.at);

    let required = criterion.required_observations() as usize;
    if observations.len() >= required {
        for start in 0..=observations.len() - required {
            let window = &observations[start..start + required];
            if !has_required_spacing(window, criterion.sampling_interval())
                || !has_required_span(window, criterion.minimum_span())
                || !has_resolution_stability(window, criterion)
            {
                continue;
            }
            let (observation_index, observation) = window[required - 1];
            return SettlementOutcome::Settled {
                boundary: SettledBoundary {
                    observation_index,
                    observed_at: observation.at,
                    quota_used: observation.quota_used,
                },
            };
        }
    }

    SettlementOutcome::Incomplete {
        reason: IncompleteSettlementReason::NoPlateauWithinWindow,
        deadline,
        observed_until,
    }
}

/// Evaluates baseline and terminal series with their recorded role-specific criteria.
pub fn evaluate_experiment_settlement(
    baseline: &SettlementObservationSeries,
    terminal: &SettlementObservationSeries,
    policy: &SettlementPolicy,
) -> ExperimentSettlement {
    ExperimentSettlement {
        baseline: detect_settlement(baseline, policy, SettlementRole::Baseline),
        terminal: detect_settlement(terminal, policy, SettlementRole::Terminal),
    }
}

fn settlement_deadline(started_at: UtcTimestamp, window: MonotonicDuration) -> UtcTimestamp {
    let window_nanos = i64::try_from(window.as_nanos()).unwrap_or(i64::MAX);
    UtcTimestamp::from_unix_nanos(started_at.unix_nanos().saturating_add(window_nanos))
}

fn has_required_spacing(
    window: &[(usize, SettlementMeterObservation)],
    sampling_interval: MonotonicDuration,
) -> bool {
    window.windows(2).all(|pair| {
        pair[1]
            .1
            .at
            .unix_nanos()
            .abs_diff(pair[0].1.at.unix_nanos())
            >= sampling_interval.as_nanos()
    })
}

fn has_required_span(
    window: &[(usize, SettlementMeterObservation)],
    minimum_span: MonotonicDuration,
) -> bool {
    let first = window.first().map(|(_, observation)| observation.at);
    let last = window.last().map(|(_, observation)| observation.at);
    match (first, last) {
        (Some(first), Some(last)) => {
            last.unix_nanos().abs_diff(first.unix_nanos()) >= minimum_span.as_nanos()
        }
        _ => false,
    }
}

fn has_resolution_stability(
    window: &[(usize, SettlementMeterObservation)],
    criterion: SettlementCriterion,
) -> bool {
    if window
        .iter()
        .any(|(_, observation)| observation.reported_resolution != criterion.reported_resolution())
    {
        return false;
    }

    for (left_index, (_, left)) in window.iter().enumerate() {
        for (_, right) in window.iter().skip(left_index + 1) {
            let change = left
                .quota_used
                .as_ppm()
                .get()
                .abs_diff(right.quota_used.as_ppm().get());
            let resolution = u64::from(criterion.reported_resolution().as_ppm().get());
            let change_units = u64::from(change) / resolution;
            if change_units > u64::from(criterion.maximum_change_resolution_units()) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::domain::quota::QuotaFractionPpm;

    const FIVE_MINUTES: u64 = 300_000_000_000;
    const TEN_MINUTES: u64 = 600_000_000_000;
    const SIXTY_MINUTES: u64 = 3_600_000_000_000;

    fn timestamp(minutes: u64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos((minutes * 60_000_000_000) as i64)
    }

    fn quota(ppm: i32) -> QuotaUsed {
        QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap())
    }

    fn resolution(ppm: i32) -> ReportedResolution {
        ReportedResolution::new(QuotaFractionPpm::new(ppm).unwrap()).unwrap()
    }

    fn observation(minutes: u64, ppm: i32, resolution_ppm: i32) -> SettlementMeterObservation {
        SettlementMeterObservation::new(timestamp(minutes), quota(ppm), resolution(resolution_ppm))
    }

    fn default_policy() -> SettlementPolicy {
        SettlementPolicy::conservative_default(resolution(10_000))
    }

    fn criterion(
        required_observations: u32,
        minimum_span_nanos: u64,
        maximum_window_nanos: u64,
        resolution_ppm: i32,
    ) -> SettlementCriterion {
        SettlementCriterion::new(
            MonotonicDuration::from_nanos(FIVE_MINUTES),
            required_observations,
            MonotonicDuration::from_nanos(minimum_span_nanos),
            0,
            MonotonicDuration::from_nanos(maximum_window_nanos),
            resolution(resolution_ppm),
        )
        .unwrap()
    }

    #[test]
    fn known_lag_series_settles_at_first_stable_window() {
        let series = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 300_000, 10_000),
                observation(10, 450_000, 10_000),
                observation(15, 600_000, 10_000),
                observation(20, 600_000, 10_000),
                observation(25, 600_000, 10_000),
            ],
        );

        let result = detect_settlement(&series, &default_policy(), SettlementRole::Terminal);
        let boundary = result.boundary().expect("known lag should settle");
        assert_eq!(boundary.observed_at(), timestamp(25));
        assert_eq!(boundary.quota_used(), quota(600_000));
        assert_eq!(boundary.observation_index(), 5);
    }

    #[test]
    fn never_settling_terminal_is_incomplete_and_publishes_no_fit() {
        let baseline = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 100_000, 10_000),
                observation(10, 100_000, 10_000),
            ],
        );
        let terminal = SettlementObservationSeries::complete(
            timestamp(0),
            (0..=12)
                .map(|minute| observation(minute * 5, 100_000 + (minute as i32) * 20_000, 10_000))
                .collect(),
        );

        let settlement = evaluate_experiment_settlement(&baseline, &terminal, &default_policy());
        assert!(settlement.baseline.is_settled());
        assert_eq!(
            settlement.terminal.incomplete_reason(),
            Some(IncompleteSettlementReason::NoPlateauWithinWindow)
        );
        assert!(settlement.is_incomplete());
        assert!(settlement.publishable_fit().is_none());
    }

    #[test]
    fn plateau_after_maximum_window_is_incomplete() {
        let series = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(55, 600_000, 10_000),
                observation(60, 600_000, 10_000),
                observation(65, 600_000, 10_000),
            ],
        );

        let result = detect_settlement(&series, &default_policy(), SettlementRole::Terminal);
        assert_eq!(
            result.incomplete_reason(),
            Some(IncompleteSettlementReason::NoPlateauWithinWindow)
        );
    }

    #[test]
    fn below_resolution_change_does_not_block_settlement() {
        let series = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 104_999, 10_000),
                observation(10, 100_001, 10_000),
            ],
        );

        let result = detect_settlement(&series, &default_policy(), SettlementRole::Baseline);
        assert!(result.is_settled());
        assert_eq!(result.boundary().unwrap().observed_at(), timestamp(10));
    }

    #[test]
    fn one_full_resolution_step_is_material() {
        let series = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 110_000, 10_000),
                observation(10, 100_000, 10_000),
            ],
        );

        let result = detect_settlement(&series, &default_policy(), SettlementRole::Baseline);
        assert_eq!(
            result.incomplete_reason(),
            Some(IncompleteSettlementReason::NoPlateauWithinWindow)
        );
    }

    proptest! {
        #[test]
        fn fewer_observations_than_required_never_settle(observation_count in 0usize..3) {
            let observations = (0..observation_count)
                .map(|index| observation(index as u64 * 5, 100_000, 10_000))
                .collect();
            let series = SettlementObservationSeries::complete(timestamp(0), observations);
            let result = detect_settlement(&series, &default_policy(), SettlementRole::Terminal);
            prop_assert!(!result.is_settled());
        }
    }

    #[test]
    fn baseline_and_terminal_use_their_recorded_criteria() {
        let baseline_criterion = criterion(2, FIVE_MINUTES, TEN_MINUTES, 10_000);
        let terminal_criterion = criterion(3, TEN_MINUTES, SIXTY_MINUTES, 10_000);
        let policy = SettlementPolicy::new(
            "role-specific-v1",
            baseline_criterion,
            terminal_criterion,
            None,
        )
        .unwrap();
        let series = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 100_000, 10_000),
                observation(10, 100_000, 10_000),
            ],
        );

        let settlement = evaluate_experiment_settlement(&series, &series, &policy);
        assert_eq!(
            settlement.baseline.boundary().unwrap().observed_at(),
            timestamp(5)
        );
        assert_eq!(
            settlement.terminal.boundary().unwrap().observed_at(),
            timestamp(10)
        );
    }

    #[test]
    fn equal_role_criteria_require_an_explicit_sharing_reason() {
        let criterion = criterion(3, TEN_MINUTES, SIXTY_MINUTES, 10_000);
        assert_eq!(
            SettlementPolicy::new("missing-reason", criterion, criterion, None),
            Err(SettlementPolicyError::SharedCriteriaReasonRequired)
        );
        let policy = SettlementPolicy::new(
            "shared-v1",
            criterion,
            criterion,
            Some("the provider exposes one lag contract for both roles".to_string()),
        )
        .unwrap();
        assert!(policy.shared_criteria_reason().is_some());
    }

    #[test]
    fn insufficient_meter_coverage_cannot_settle_stable_readings() {
        let baseline = SettlementObservationSeries::complete(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 100_000, 10_000),
                observation(10, 100_000, 10_000),
            ],
        );
        let terminal = SettlementObservationSeries::new(
            timestamp(0),
            vec![
                observation(0, 100_000, 10_000),
                observation(5, 100_000, 10_000),
                observation(10, 100_000, 10_000),
            ],
            SettlementCoverage::insufficient(4, 3),
        );

        let settlement = evaluate_experiment_settlement(&baseline, &terminal, &default_policy());
        assert!(settlement.baseline.is_settled());
        assert_eq!(
            settlement.terminal.incomplete_reason(),
            Some(IncompleteSettlementReason::InsufficientMeterCoverage)
        );
        assert!(settlement.publishable_fit().is_none());
    }

    #[test]
    fn detection_is_replayable_without_mutating_the_series() {
        let series = SettlementObservationSeries::new(
            timestamp(0),
            vec![
                observation(10, 100_000, 10_000),
                observation(0, 100_000, 10_000),
                observation(5, 100_000, 10_000),
            ],
            SettlementCoverage::Complete,
        );
        let original = series.clone();
        let first = detect_settlement(&series, &default_policy(), SettlementRole::Terminal);
        let second = detect_settlement(&series, &default_policy(), SettlementRole::Terminal);
        assert_eq!(first, second);
        assert_eq!(first.boundary().unwrap().observed_at(), timestamp(10));
        assert_eq!(series, original);
    }
}
