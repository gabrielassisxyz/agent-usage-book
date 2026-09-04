//! Interval reconciliation with strict eligibility gating (aub-dpn.1, PLAN.md 20.2, 33 Phase 9, 35).
//!
//! May not depend on:
//! - presentation (reconciliation produces domain models, never renders text)
//! - provider adapters (reconciliation consumes stored observations, never calls providers)
//!
//! Reconciles observed meter movement against locally explained calibrated movement
//! only where all six eligibility conditions hold, reporting unexplained residual:
//!
//! ```text
//! observed meter delta - locally explained calibrated delta = unexplained residual
//! ```
//!
//! Everywhere else the residual is not computed rather than computed with a caveat.

use std::fmt;

use crate::attribution::account_segment::AccountEvidenceClass;
use crate::calibration::health::CalibrationHealth;
use crate::domain::credits::Credits;
use crate::domain::interval::{DomainQuantity, Interval};
use crate::domain::provenance::{
    EvidenceId, ProvenanceManifest, QuerySemantics, WindowCalibrationId, WitnessId,
};
use crate::domain::quota::{PercentagePoints, QuotaUsed};
use crate::domain::time::UtcTimestamp;
use crate::domain::window::{QuantizationSemantics, ReportedResolution, WindowSemanticKey};
use crate::store::account::AccountId;
use crate::store::calibration::WindowCalibration;

/// The six strict eligibility conditions for interval reconciliation (PLAN.md 35, aub-dpn.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EligibilityCondition {
    /// Both boundary observations share the same account and window semantic key.
    SameAccountAndWindow,
    /// No quota reset occurred inside the candidate interval.
    NoResetInside,
    /// Sampling attempt and measurement coverage over the interval meet acceptable thresholds.
    AcceptableMeterCoverage,
    /// An applicable, currently active calibration in health state Current exists for this scope.
    ApplicableCurrentCalibration,
    /// The interval is sufficiently settled past the provider lag horizon.
    SufficientSettlementAndLagHandling,
    /// Local transcript usage in the interval carries exact measured attribution where required.
    ExactLocalUsageWhereRequired,
}

impl EligibilityCondition {
    /// All six eligibility conditions in evaluation order.
    pub const ALL: [Self; 6] = [
        Self::SameAccountAndWindow,
        Self::NoResetInside,
        Self::AcceptableMeterCoverage,
        Self::ApplicableCurrentCalibration,
        Self::SufficientSettlementAndLagHandling,
        Self::ExactLocalUsageWhereRequired,
    ];

    /// Human-readable title of the eligibility condition.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SameAccountAndWindow => "same account and window",
            Self::NoResetInside => "no reset inside",
            Self::AcceptableMeterCoverage => "acceptable meter coverage",
            Self::ApplicableCurrentCalibration => "applicable current calibration",
            Self::SufficientSettlementAndLagHandling => "sufficient settlement and lag handling",
            Self::ExactLocalUsageWhereRequired => "exact local usage where required",
        }
    }

    /// Machine-readable snake_case identifier.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SameAccountAndWindow => "same_account_and_window",
            Self::NoResetInside => "no_reset_inside",
            Self::AcceptableMeterCoverage => "acceptable_meter_coverage",
            Self::ApplicableCurrentCalibration => "applicable_current_calibration",
            Self::SufficientSettlementAndLagHandling => "sufficient_settlement_and_lag_handling",
            Self::ExactLocalUsageWhereRequired => "exact_local_usage_where_required",
        }
    }
}

impl fmt::Display for EligibilityCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Result of evaluating the six eligibility conditions for a candidate interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibilityAssessment {
    pub evaluated: Vec<(EligibilityCondition, bool)>,
    pub failing: Vec<EligibilityCondition>,
}

impl EligibilityAssessment {
    /// Returns true when all six conditions passed.
    pub fn is_eligible(&self) -> bool {
        self.failing.is_empty()
    }

    /// Returns the list of failing conditions.
    pub fn failing_conditions(&self) -> &[EligibilityCondition] {
        &self.failing
    }
}

/// The four diagnostic interpretation patterns from PLAN.md Section 35.
///
/// Reported strictly as diagnostic patterns, never as asserted causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResidualPattern {
    /// Persistently positive residual across intervals.
    PersistentlyPositive,
    /// Persistently negative residual across intervals.
    PersistentlyNegative,
    /// Sudden step change in residual magnitude.
    StepChange,
    /// Alternating short-interval residuals that net to approximately zero.
    AlternatingNetZero,
}

impl ResidualPattern {
    /// All four diagnostic patterns.
    pub const ALL: [Self; 4] = [
        Self::PersistentlyPositive,
        Self::PersistentlyNegative,
        Self::StepChange,
        Self::AlternatingNetZero,
    ];

    /// Human-readable pattern name.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::PersistentlyPositive => "persistently positive",
            Self::PersistentlyNegative => "persistently negative",
            Self::StepChange => "step change",
            Self::AlternatingNetZero => "alternating short-interval residuals that net to zero",
        }
    }

    /// Diagnostic interpretation: reported strictly as potential hypotheses/patterns,
    /// never stated as a definitive cause (PLAN.md 35, aub-dpn.1).
    pub const fn diagnostic_pattern(&self) -> &'static str {
        match self {
            Self::PersistentlyPositive => {
                "pattern: persistently positive residual: possible web, headless-unlogged, cross-machine or missed transcript consumption"
            }
            Self::PersistentlyNegative => {
                "pattern: persistently negative residual: possible calibration overprediction or provider semantics change"
            }
            Self::StepChange => {
                "pattern: step change in residual: possible plan or provider accounting transition"
            }
            Self::AlternatingNetZero => {
                "pattern: alternating short-interval residuals that net to zero: likely accounting lag"
            }
        }
    }
}

/// Classifies diagnostic patterns across a sequence of residuals (PLAN.md 35).
pub fn classify_patterns(residuals: &[Credits]) -> Vec<ResidualPattern> {
    let mut patterns = Vec::new();
    if residuals.len() < 3 {
        return patterns;
    }

    let all_positive = residuals.iter().all(|r| r.micros() > 0);
    let all_negative = residuals.iter().all(|r| r.micros() < 0);

    if all_positive {
        patterns.push(ResidualPattern::PersistentlyPositive);
    }
    if all_negative {
        patterns.push(ResidualPattern::PersistentlyNegative);
    }

    if residuals.len() >= 4 {
        let mid = residuals.len() / 2;
        let sum_first: i128 = residuals[..mid]
            .iter()
            .map(|r| i128::from(r.micros()))
            .sum();
        let sum_second: i128 = residuals[mid..]
            .iter()
            .map(|r| i128::from(r.micros()))
            .sum();
        let mean_first = sum_first / mid as i128;
        let mean_second = sum_second / (residuals.len() - mid) as i128;
        let step = (mean_second - mean_first).abs();

        let mut alternates = true;
        for i in 1..residuals.len() {
            let prev_sign = residuals[i - 1].micros().signum();
            let curr_sign = residuals[i].micros().signum();
            if prev_sign == 0 || curr_sign == 0 || prev_sign == curr_sign {
                alternates = false;
                break;
            }
        }
        let total_sum: i128 = residuals.iter().map(|r| i128::from(r.micros())).sum();
        let total_abs: i128 = residuals.iter().map(|r| i128::from(r.micros().abs())).sum();

        if alternates && total_abs > 0 && total_sum.abs() * 5 <= total_abs {
            patterns.push(ResidualPattern::AlternatingNetZero);
        }

        if !alternates && step > 0 && total_abs > 0 && step * (residuals.len() as i128) >= total_abs
        {
            patterns.push(ResidualPattern::StepChange);
        }
    }

    patterns
}

/// One observation boundary for candidate reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateObservation {
    pub observation_id: EvidenceId,
    pub account_id: AccountId,
    pub received_at: UtcTimestamp,
    pub window_key: WindowSemanticKey,
    pub quota_used: QuotaUsed,
    pub resets_at: UtcTimestamp,
    /// Smallest increment this reading was reported at, persisted with the
    /// observation (PLAN.md 12.5). Its quantization interval is derived here, not
    /// from one globally guessed tolerance.
    pub reported_resolution: ReportedResolution,
    /// How the provider maps an underlying value onto that resolution (PLAN.md 12.5).
    pub quantization: QuantizationSemantics,
}

impl CandidateObservation {
    /// The admissible interval, in parts per million of quota used, that this
    /// reading asserts once its resolution and quantization semantics are taken
    /// into account (PLAN.md 12.5, 23.5). A reading under round-to-nearest is a
    /// band centred on the reported value, not an infinitely precise scalar.
    fn quantized_used_bounds_ppm(&self) -> (i64, i64) {
        let reading = i64::from(self.quota_used.as_ppm().get());
        let resolution = i64::from(self.reported_resolution.as_ppm().get());
        match self.quantization {
            QuantizationSemantics::Exact => (reading, reading),
            QuantizationSemantics::RoundedToNearest => {
                let below = resolution / 2;
                (reading - below, reading + (resolution - below))
            }
            QuantizationSemantics::RoundedDown => (reading, reading + resolution),
            QuantizationSemantics::RoundedUp => (reading - resolution, reading),
            QuantizationSemantics::Unknown => (reading - resolution, reading + resolution),
        }
    }
}

/// The quantization-widened bounds of observed meter movement over the interval,
/// in parts per million. The reported scalar delta always lies inside it; a coarser
/// provider resolution widens it (PLAN.md 12.5, 23.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterDeltaBounds {
    lower_ppm: i64,
    upper_ppm: i64,
}

impl MeterDeltaBounds {
    pub fn lower_ppm(&self) -> i64 {
        self.lower_ppm
    }

    pub fn upper_ppm(&self) -> i64 {
        self.upper_ppm
    }

    pub fn width_ppm(&self) -> i64 {
        self.upper_ppm - self.lower_ppm
    }
}

/// Timing-alignment uncertainty: the symmetric credit band by which provider
/// accounting lag and imperfect timestamp alignment could shift the residual
/// (PLAN.md 23.5, 35). Represented explicitly so it is never silently assumed to
/// be zero; a caller with no lag model passes [`TimingAlignmentUncertainty::none`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingAlignmentUncertainty {
    half_width: Credits,
}

impl TimingAlignmentUncertainty {
    /// A band of `+/- half_width` credits about the residual. The magnitude is
    /// taken, so a negatively signed argument still widens rather than narrows.
    pub fn from_credit_half_width(half_width: Credits) -> Self {
        Self {
            half_width: Credits::from_micros(half_width.micros().abs()),
        }
    }

    /// No timing-alignment uncertainty. Still explicit: the residual interval
    /// carries this value and the renderers report it.
    pub fn none() -> Self {
        Self {
            half_width: Credits::from_micros(0),
        }
    }

    pub fn half_width(&self) -> Credits {
        self.half_width
    }
}

/// One local usage event within the candidate interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateUsageEvent {
    pub event_id: EvidenceId,
    pub occurred_at: UtcTimestamp,
    pub is_measured: bool,
    pub attribution_class: AccountEvidenceClass,
    pub is_quarantined: bool,
}

/// Local usage events and total credits within the candidate interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntervalUsage {
    pub events: Vec<CandidateUsageEvent>,
    pub total_credits: Credits,
}

impl IntervalUsage {
    pub fn new(events: Vec<CandidateUsageEvent>, total_credits: Credits) -> Self {
        Self {
            events,
            total_credits,
        }
    }

    pub fn empty() -> Self {
        Self {
            events: Vec::new(),
            total_credits: Credits::from_micros(0),
        }
    }

    pub fn has_inexact_usage(&self) -> bool {
        self.events
            .iter()
            .any(|e| !e.is_measured || !e.attribution_class.is_explicit() || e.is_quarantined)
    }
}

/// Meter coverage inputs for candidate reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalCoverage {
    pub coverage_acceptable: bool,
    pub has_destructive_gap: bool,
}

impl IntervalCoverage {
    pub const fn acceptable() -> Self {
        Self {
            coverage_acceptable: true,
            has_destructive_gap: false,
        }
    }

    pub const fn unacceptable() -> Self {
        Self {
            coverage_acceptable: false,
            has_destructive_gap: false,
        }
    }

    pub const fn with_destructive_gap() -> Self {
        Self {
            coverage_acceptable: true,
            has_destructive_gap: true,
        }
    }
}

/// Settlement and lag handling inputs for candidate reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalSettlement {
    pub is_settled: bool,
    pub lag_handling_satisfied: bool,
}

impl IntervalSettlement {
    pub const fn settled() -> Self {
        Self {
            is_settled: true,
            lag_handling_satisfied: true,
        }
    }

    pub const fn unsettled() -> Self {
        Self {
            is_settled: false,
            lag_handling_satisfied: true,
        }
    }

    pub const fn unsatisfied_lag() -> Self {
        Self {
            is_settled: true,
            lag_handling_satisfied: false,
        }
    }
}

/// A candidate interval evaluated for reconciliation.
#[derive(Debug, Clone)]
pub struct CandidateInterval {
    pub account_id: AccountId,
    pub window_key: WindowSemanticKey,
    pub start_observation: CandidateObservation,
    pub end_observation: CandidateObservation,
    pub resets_in_interval: Vec<UtcTimestamp>,
    pub coverage: IntervalCoverage,
    pub active_calibration: Option<WindowCalibration>,
    pub calibration_health: Option<CalibrationHealth>,
    pub settlement: IntervalSettlement,
    pub local_usage: IntervalUsage,
    /// Explicit timing-alignment uncertainty for this interval (PLAN.md 23.5, 35).
    pub timing_alignment: TimingAlignmentUncertainty,
}

/// Evaluates all six eligibility conditions independently for a candidate interval.
pub fn evaluate_eligibility(candidate: &CandidateInterval) -> EligibilityAssessment {
    let mut evaluated = Vec::with_capacity(6);
    let mut failing = Vec::new();

    // Condition 1: Same account and window.
    let same_account = candidate.start_observation.account_id == candidate.account_id
        && candidate.end_observation.account_id == candidate.account_id;
    let same_window = candidate.start_observation.window_key == candidate.window_key
        && candidate.end_observation.window_key == candidate.window_key;
    let cond1 = same_account && same_window;
    evaluated.push((EligibilityCondition::SameAccountAndWindow, cond1));
    if !cond1 {
        failing.push(EligibilityCondition::SameAccountAndWindow);
    }

    // Condition 2: No reset inside.
    let no_reset_records = candidate.resets_in_interval.is_empty();
    let window_not_reset = candidate.start_observation.resets_at
        > candidate.end_observation.received_at
        && candidate.start_observation.resets_at == candidate.end_observation.resets_at;
    let quota_not_dropped = candidate.end_observation.quota_used.as_ppm().get()
        >= candidate.start_observation.quota_used.as_ppm().get();
    let cond2 = no_reset_records && window_not_reset && quota_not_dropped;
    evaluated.push((EligibilityCondition::NoResetInside, cond2));
    if !cond2 {
        failing.push(EligibilityCondition::NoResetInside);
    }

    // Condition 3: Acceptable meter coverage.
    let cond3 = candidate.coverage.coverage_acceptable && !candidate.coverage.has_destructive_gap;
    evaluated.push((EligibilityCondition::AcceptableMeterCoverage, cond3));
    if !cond3 {
        failing.push(EligibilityCondition::AcceptableMeterCoverage);
    }

    // Condition 4: Applicable current calibration.
    let calibration_applicable = candidate
        .active_calibration
        .as_ref()
        .map(|c| c.window_semantic_key() == &candidate.window_key)
        .unwrap_or(false);
    let calibration_current = candidate.calibration_health == Some(CalibrationHealth::Current);
    let cond4 = calibration_applicable && calibration_current;
    evaluated.push((EligibilityCondition::ApplicableCurrentCalibration, cond4));
    if !cond4 {
        failing.push(EligibilityCondition::ApplicableCurrentCalibration);
    }

    // Condition 5: Sufficient settlement and lag handling.
    let cond5 = candidate.settlement.is_settled && candidate.settlement.lag_handling_satisfied;
    evaluated.push((
        EligibilityCondition::SufficientSettlementAndLagHandling,
        cond5,
    ));
    if !cond5 {
        failing.push(EligibilityCondition::SufficientSettlementAndLagHandling);
    }

    // Condition 6: Exact local usage where required.
    let cond6 = if candidate.local_usage.events.is_empty() {
        true
    } else {
        !candidate.local_usage.has_inexact_usage()
    };
    evaluated.push((EligibilityCondition::ExactLocalUsageWhereRequired, cond6));
    if !cond6 {
        failing.push(EligibilityCondition::ExactLocalUsageWhereRequired);
    }

    EligibilityAssessment { evaluated, failing }
}

/// A successfully computed unexplained residual over an eligible interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledResidual {
    pub account_id: AccountId,
    pub window_key: WindowSemanticKey,
    pub interval_start: UtcTimestamp,
    pub interval_end: UtcTimestamp,
    pub observed_meter_delta: PercentagePoints,
    pub observed_meter_credits: Credits,
    pub locally_explained_credits: Credits,
    pub explained_interval_change: PercentagePoints,
    pub unexplained_residual: Credits,
    pub unexplained_residual_percentage_points: PercentagePoints,
    /// Observed meter movement widened by each endpoint's quantization interval.
    pub observed_meter_delta_bounds: MeterDeltaBounds,
    /// Observed meter movement in credits, widened by both the quantization
    /// bounds and the calibration coefficient's stated uncertainty, propagated
    /// with interval arithmetic.
    pub observed_meter_credits_interval: Interval<Credits>,
    /// The timing-alignment uncertainty that was propagated into the residual.
    pub timing_alignment: TimingAlignmentUncertainty,
    /// The unexplained residual as an interval, carrying uncertainty from all
    /// three sources: meter quantization, calibration uncertainty and timing
    /// alignment (PLAN.md 35). A residual interval containing zero means the two
    /// axes reconcile within measurement uncertainty and is not a finding.
    pub unexplained_residual_interval: Interval<Credits>,
    pub calibration_id: WindowCalibrationId,
    pub provenance: ProvenanceManifest,
}

impl ReconciledResidual {
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    pub fn window_key(&self) -> &WindowSemanticKey {
        &self.window_key
    }

    pub fn interval_start(&self) -> UtcTimestamp {
        self.interval_start
    }

    pub fn interval_end(&self) -> UtcTimestamp {
        self.interval_end
    }

    pub fn observed_meter_delta(&self) -> PercentagePoints {
        self.observed_meter_delta
    }

    pub fn observed_meter_credits(&self) -> Credits {
        self.observed_meter_credits
    }

    pub fn locally_explained_credits(&self) -> Credits {
        self.locally_explained_credits
    }

    pub fn explained_interval_change(&self) -> PercentagePoints {
        self.explained_interval_change
    }

    pub fn unexplained_residual(&self) -> Credits {
        self.unexplained_residual
    }

    pub fn unexplained_residual_percentage_points(&self) -> PercentagePoints {
        self.unexplained_residual_percentage_points
    }

    pub fn observed_meter_delta_bounds(&self) -> MeterDeltaBounds {
        self.observed_meter_delta_bounds
    }

    pub fn observed_meter_credits_interval(&self) -> Interval<Credits> {
        self.observed_meter_credits_interval
    }

    pub fn timing_alignment(&self) -> TimingAlignmentUncertainty {
        self.timing_alignment
    }

    pub fn unexplained_residual_interval(&self) -> Interval<Credits> {
        self.unexplained_residual_interval
    }

    /// True when the residual interval contains zero: the observed and locally
    /// explained movement reconcile within the uncertainty of the measurement,
    /// so this interval is reported as reconciling, never as a finding
    /// (PLAN.md 35).
    pub fn reconciles_within_uncertainty(&self) -> bool {
        self.unexplained_residual_interval.lower().micros() <= 0
            && self.unexplained_residual_interval.upper().micros() >= 0
    }

    pub fn calibration_id(&self) -> &WindowCalibrationId {
        &self.calibration_id
    }

    pub fn provenance(&self) -> &ProvenanceManifest {
        &self.provenance
    }
}

/// Outcome of attempting to reconcile a candidate interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationOutcome {
    /// All six eligibility conditions passed: unexplained residual computed.
    ///
    /// Boxed because the residual carries three uncertainty intervals and a
    /// provenance manifest, and `NotComputed` is a short vector: the same
    /// indirection `transcripts::native` uses for its lopsided event enum.
    Computed(Box<ReconciledResidual>),
    /// One or more conditions failed: residual is not computed, naming failing conditions.
    NotComputed {
        failing_conditions: Vec<EligibilityCondition>,
    },
}

impl ReconciliationOutcome {
    pub fn is_computed(&self) -> bool {
        matches!(self, Self::Computed(_))
    }

    pub fn as_computed(&self) -> Option<&ReconciledResidual> {
        match self {
            Self::Computed(res) => Some(res.as_ref()),
            Self::NotComputed { .. } => None,
        }
    }

    pub fn failing_conditions(&self) -> &[EligibilityCondition] {
        match self {
            Self::Computed(_) => &[],
            Self::NotComputed { failing_conditions } => failing_conditions,
        }
    }
}

/// Reconciles a candidate interval into a signed unexplained residual, or reports
/// not computed with the failing eligibility conditions (PLAN.md 35, aub-dpn.1).
pub fn reconcile(candidate: &CandidateInterval) -> ReconciliationOutcome {
    let assessment = evaluate_eligibility(candidate);
    if !assessment.is_eligible() {
        return ReconciliationOutcome::NotComputed {
            failing_conditions: assessment.failing,
        };
    }

    let calibration = candidate
        .active_calibration
        .as_ref()
        .expect("calibration is present when condition 4 passed");

    let observed_meter_delta = candidate.end_observation.quota_used.as_ppm()
        - candidate.start_observation.quota_used.as_ppm();

    let observed_meter_credits = calibration.fitted() * observed_meter_delta;
    let locally_explained_credits = candidate.local_usage.total_credits;

    let micros_per_point = calibration.fitted().micros_per_point();
    let explained_points_i32 = if micros_per_point > 0 {
        let num = i128::from(locally_explained_credits.micros());
        let den = i128::from(micros_per_point);
        let half = den / 2;
        let rounded = if num >= 0 {
            (num + half) / den
        } else {
            (num - half) / den
        };
        rounded.clamp(
            i128::from(PercentagePoints::MIN),
            i128::from(PercentagePoints::MAX),
        ) as i32
    } else {
        0
    };
    let explained_interval_change =
        PercentagePoints::new(explained_points_i32).expect("clamped to valid percentage points");

    let unexplained_residual = observed_meter_credits - locally_explained_credits;

    let res_points_i32 = (observed_meter_delta.get() as i64 - explained_points_i32 as i64)
        .clamp(PercentagePoints::MIN as i64, PercentagePoints::MAX as i64)
        as i32;
    let unexplained_residual_percentage_points =
        PercentagePoints::new(res_points_i32).expect("clamped to valid percentage points");

    let (
        observed_meter_delta_bounds,
        observed_meter_credits_interval,
        unexplained_residual_interval,
    ) = propagate_residual_uncertainty(candidate, calibration, locally_explained_credits);

    let mut inputs = vec![
        candidate.start_observation.observation_id.clone(),
        candidate.end_observation.observation_id.clone(),
    ];
    inputs.extend(
        candidate
            .local_usage
            .events
            .iter()
            .map(|e| e.event_id.clone()),
    );

    let witnesses = vec![WitnessId::WindowCalibration(calibration.id().clone())];
    let query_semantics = QuerySemantics::new(
        "interval_reconciliation",
        format!(
            "{}..{}",
            candidate.start_observation.received_at.unix_nanos(),
            candidate.end_observation.received_at.unix_nanos()
        ),
    );
    let provenance = ProvenanceManifest::new(inputs, witnesses, query_semantics);

    ReconciliationOutcome::Computed(Box::new(ReconciledResidual {
        account_id: candidate.account_id,
        window_key: candidate.window_key.clone(),
        interval_start: candidate.start_observation.received_at,
        interval_end: candidate.end_observation.received_at,
        observed_meter_delta,
        observed_meter_credits,
        locally_explained_credits,
        explained_interval_change,
        unexplained_residual,
        unexplained_residual_percentage_points,
        observed_meter_delta_bounds,
        observed_meter_credits_interval,
        timing_alignment: candidate.timing_alignment,
        unexplained_residual_interval,
        calibration_id: calibration.id().clone(),
        provenance,
    }))
}

/// Propagates residual uncertainty from meter quantization, calibration
/// uncertainty and timing alignment into an interval over the unexplained
/// residual (PLAN.md 35). Widening any of the three inputs can only widen the
/// result: each combination is monotone in the endpoint it is built from, which
/// is the non-narrowing law the property test pins.
fn propagate_residual_uncertainty(
    candidate: &CandidateInterval,
    calibration: &WindowCalibration,
    locally_explained_credits: Credits,
) -> (MeterDeltaBounds, Interval<Credits>, Interval<Credits>) {
    let (start_lower, start_upper) = candidate.start_observation.quantized_used_bounds_ppm();
    let (end_lower, end_upper) = candidate.end_observation.quantized_used_bounds_ppm();
    // Interval subtraction of the two quantization bands: the widest and
    // narrowest movement both readings admit.
    let delta_lower = end_lower - start_upper;
    let delta_upper = end_upper - start_lower;
    let bounds = MeterDeltaBounds {
        lower_ppm: delta_lower,
        upper_ppm: delta_upper,
    };

    let coefficient = calibration.uncertainty();
    let coefficient_lower = i128::from(coefficient.lower().micros_per_point());
    let coefficient_upper = i128::from(coefficient.upper().micros_per_point());
    let corners = [
        coefficient_lower * i128::from(delta_lower),
        coefficient_lower * i128::from(delta_upper),
        coefficient_upper * i128::from(delta_lower),
        coefficient_upper * i128::from(delta_upper),
    ];
    let credits_lower = *corners.iter().min().expect("four corners");
    let credits_upper = *corners.iter().max().expect("four corners");

    let explained = i128::from(locally_explained_credits.micros());
    let timing = i128::from(candidate.timing_alignment.half_width().micros());
    (
        bounds,
        credits_interval(credits_lower, credits_upper),
        credits_interval(
            credits_lower - explained - timing,
            credits_upper - explained + timing,
        ),
    )
}

/// Builds an [`Interval<Credits>`] from an ordered pair of `i128` micro-credit
/// bounds, saturating each endpoint into `i64` range.
fn credits_interval(lower: i128, upper: i128) -> Interval<Credits> {
    let saturate = |value: i128| value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    Interval::new(
        Credits::from_micros(saturate(lower)),
        Credits::from_micros(saturate(upper)),
    )
    .expect("bounds are passed in nondecreasing order")
}

/// Formats a reconciliation outcome for human display.
///
/// Output term is "unexplained residual" on every human surface (aub-dpn.1).
pub fn render_reconciliation_human(outcome: &ReconciliationOutcome) -> String {
    match outcome {
        ReconciliationOutcome::Computed(res) => {
            let sign = if res.unexplained_residual.micros() >= 0 {
                "+"
            } else {
                ""
            };
            let verdict = if res.reconciles_within_uncertainty() {
                "reconciles within uncertainty: the two axes agree within the uncertainty of the measurement (residual interval contains zero)"
            } else {
                "residual interval excludes zero"
            };
            format!(
                "reconciliation: account={} window={} interval=[{} .. {}]
  observed meter delta: {} ppm
  locally explained calibrated delta: {} ppm ({} credits)
  unexplained residual: {}{} credits ({}{} ppm)
  meter quantization delta bounds: [{} .. {}] ppm
  observed meter credits interval: [{} .. {}] credits
  timing alignment uncertainty: +/-{} credits
  unexplained residual interval: [{} .. {}] credits
  {}",
                res.account_id.value(),
                res.window_key.as_str(),
                res.interval_start.unix_nanos(),
                res.interval_end.unix_nanos(),
                res.observed_meter_delta.get(),
                res.explained_interval_change.get(),
                res.locally_explained_credits.micros(),
                sign,
                res.unexplained_residual.micros(),
                sign,
                res.unexplained_residual_percentage_points.get(),
                res.observed_meter_delta_bounds.lower_ppm(),
                res.observed_meter_delta_bounds.upper_ppm(),
                res.observed_meter_credits_interval.lower().micros(),
                res.observed_meter_credits_interval.upper().micros(),
                res.timing_alignment.half_width().micros(),
                res.unexplained_residual_interval.lower().micros(),
                res.unexplained_residual_interval.upper().micros(),
                verdict,
            )
        }
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            let names: Vec<&'static str> = failing_conditions.iter().map(|c| c.name()).collect();
            format!(
                "reconciliation: unexplained residual: not computed (failing eligibility conditions: {})",
                names.join(", ")
            )
        }
    }
}

/// Serializes a reconciliation outcome to JSON.
///
/// Output term is "unexplained_residual" on every JSON surface (aub-dpn.1).
pub fn reconciliation_json(outcome: &ReconciliationOutcome) -> serde_json::Value {
    match outcome {
        ReconciliationOutcome::Computed(res) => serde_json::json!({
            "status": "computed",
            "account_id": res.account_id.value(),
            "window_semantic_key": res.window_key.as_str(),
            "interval_start_nanos": res.interval_start.unix_nanos(),
            "interval_end_nanos": res.interval_end.unix_nanos(),
            "observed_meter_delta_ppm": res.observed_meter_delta.get(),
            "observed_meter_credits_micros": res.observed_meter_credits.micros(),
            "locally_explained_credits_micros": res.locally_explained_credits.micros(),
            "explained_interval_change_ppm": res.explained_interval_change.get(),
            "unexplained_residual": {
                "credits_micros": res.unexplained_residual.micros(),
                "percentage_points_ppm": res.unexplained_residual_percentage_points.get()
            },
            "observed_meter_delta_bounds_ppm": {
                "lower": res.observed_meter_delta_bounds.lower_ppm(),
                "upper": res.observed_meter_delta_bounds.upper_ppm()
            },
            "observed_meter_credits_interval": {
                "lower": res.observed_meter_credits_interval.lower().to_exact_string(),
                "upper": res.observed_meter_credits_interval.upper().to_exact_string(),
                "unit": Credits::unit()
            },
            "timing_alignment_uncertainty": {
                "credits_micros_half_width": res.timing_alignment.half_width().micros()
            },
            "unexplained_residual_interval": {
                "lower": res.unexplained_residual_interval.lower().to_exact_string(),
                "upper": res.unexplained_residual_interval.upper().to_exact_string(),
                "unit": Credits::unit()
            },
            "reconciles_within_uncertainty": res.reconciles_within_uncertainty(),
            "calibration_id": res.calibration_id.as_str(),
            "provenance": {
                "inputs_count": res.provenance.input_count(),
                "inputs_hash": res.provenance.inputs_hash().to_hex()
            }
        }),
        ReconciliationOutcome::NotComputed { failing_conditions } => {
            let conditions: Vec<&'static str> =
                failing_conditions.iter().map(|c| c.as_str()).collect();
            serde_json::json!({
                "status": "not_computed",
                "unexplained_residual": null,
                "failing_conditions": conditions
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eligibility_conditions_have_consistent_labels() {
        assert_eq!(EligibilityCondition::ALL.len(), 6);
        for cond in EligibilityCondition::ALL {
            assert!(!cond.name().is_empty());
            assert!(!cond.as_str().is_empty());
            assert_eq!(format!("{cond}"), cond.name());
        }
    }

    #[test]
    fn residual_patterns_have_consistent_diagnostics() {
        assert_eq!(ResidualPattern::ALL.len(), 4);
        for pattern in ResidualPattern::ALL {
            assert!(!pattern.name().is_empty());
            let diag = pattern.diagnostic_pattern();
            assert!(diag.starts_with("pattern: "));
            assert!(diag.contains("possible ") || diag.contains("likely "));
            assert!(!diag.contains("caused by"));
        }
    }

    #[test]
    fn interval_coverage_constructors() {
        let acc = IntervalCoverage::acceptable();
        assert!(acc.coverage_acceptable);
        assert!(!acc.has_destructive_gap);

        let unacc = IntervalCoverage::unacceptable();
        assert!(!unacc.coverage_acceptable);

        let gap = IntervalCoverage::with_destructive_gap();
        assert!(gap.has_destructive_gap);
    }

    #[test]
    fn interval_settlement_constructors() {
        let set = IntervalSettlement::settled();
        assert!(set.is_settled);
        assert!(set.lag_handling_satisfied);

        let unset = IntervalSettlement::unsettled();
        assert!(!unset.is_settled);

        let unlag = IntervalSettlement::unsatisfied_lag();
        assert!(!unlag.lag_handling_satisfied);
    }

    #[test]
    fn interval_usage_detection() {
        let empty = IntervalUsage::empty();
        assert!(!empty.has_inexact_usage());

        let measured = CandidateUsageEvent {
            event_id: EvidenceId::new("ev-1"),
            occurred_at: UtcTimestamp::from_unix_nanos(10),
            is_measured: true,
            attribution_class: AccountEvidenceClass::ExplicitLauncherOrHook,
            is_quarantined: false,
        };
        let usage = IntervalUsage::new(vec![measured], Credits::from_micros(100));
        assert!(!usage.has_inexact_usage());

        let unmeasured = CandidateUsageEvent {
            event_id: EvidenceId::new("ev-2"),
            occurred_at: UtcTimestamp::from_unix_nanos(20),
            is_measured: false,
            attribution_class: AccountEvidenceClass::ExplicitLauncherOrHook,
            is_quarantined: false,
        };
        let usage_bad = IntervalUsage::new(vec![unmeasured], Credits::from_micros(100));
        assert!(usage_bad.has_inexact_usage());
    }

    #[test]
    fn outcome_helpers() {
        let not_comp = ReconciliationOutcome::NotComputed {
            failing_conditions: vec![EligibilityCondition::NoResetInside],
        };
        assert!(!not_comp.is_computed());
        assert!(not_comp.as_computed().is_none());
        assert_eq!(
            not_comp.failing_conditions(),
            &[EligibilityCondition::NoResetInside]
        );
    }
}
