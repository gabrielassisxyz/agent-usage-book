//! Contamination detection for controlled calibration experiments.
//!
//! Exclusive account time is a recorded premise of a controlled experiment, never an
//! enforced lock: `aub` does not stop other work on the account. What it can do is
//! notice evidence that the premise was false (PLAN.md 23.7). Four signals are
//! evaluated, each independently meaningful:
//!
//! - quota moving during the pre-burn idle period means something else was consuming;
//! - quota continuing to move far beyond the expected settlement interval means
//!   either contamination or a lag model that is wrong;
//! - local controlled credits flat while meter movement is substantial is the
//!   clearest case;
//! - another locally known session marked against the same account is direct
//!   evidence rather than inference.
//!
//! A contaminated fit is either rejected or published explicitly marked
//! contaminated, and a contaminated candidate is never activatable.
//!
//! This module has no persistence dependency: callers provide the observation
//! series, the locally attributed credits total, the marker timeline slice, the
//! experiment window, and the recorded thresholds, and receive a verdict naming
//! every signal that fired with its magnitude. Thresholds always travel with the
//! experiment (recorded by `begin`), never as source constants read at detection
//! time. The one exception that proves the shape is the constructor for the
//! conservative defaults, which `begin` calls once to fill the row; detection
//! itself takes the struct.
//!
//! May not depend on:
//! - transcripts (the calibration layer never parses transcripts)
//! - presentation
//! - provider adapters

use std::fmt;

use crate::domain::credits::Credits;
use crate::domain::quota::QuotaUsed;
use crate::domain::time::{MonotonicDuration, UtcTimestamp};

/// Conservative defaults, used once by `begin` to fill a new experiment row.
/// Detection never reads these; it reads the row.
const DEFAULT_PRE_BURN_MAX_MOVEMENT_PPM: u32 = 10_000;
const DEFAULT_POST_SETTLEMENT_MAX_MOVEMENT_PPM: u32 = 10_000;
const DEFAULT_POST_SETTLEMENT_GRACE_NANOS: u64 = 3_600_000_000_000;
const DEFAULT_FLAT_CREDITS_MIN_METER_MOVEMENT_PPM: u32 = 20_000;
const DEFAULT_FLAT_CREDITS_MAX_LOCAL_MICROS: i64 = 0;

/// Which of the four contamination signals fired. Four variants, matched
/// exhaustively everywhere: adding a signal is a compile error at every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContaminationSignal {
    /// Quota moved during the pre-burn idle period asserted by `begin`.
    PreBurnIdleMovement,
    /// Quota kept moving far beyond the expected settlement interval.
    ExtendedSettlementDrift,
    /// Local controlled credits stayed flat while meter movement was substantial.
    FlatCreditsWithMeterMovement,
    /// Another locally known session was marked against the same account inside
    /// the experiment window.
    OverlappingSession,
}

impl ContaminationSignal {
    /// A stable snake-case label, so a finding names its signal in one word.
    pub fn label(self) -> &'static str {
        match self {
            Self::PreBurnIdleMovement => "pre_burn_idle_movement",
            Self::ExtendedSettlementDrift => "extended_settlement_drift",
            Self::FlatCreditsWithMeterMovement => "flat_credits_with_meter_movement",
            Self::OverlappingSession => "overlapping_session",
        }
    }

    /// All four signals, in evaluation order, so callers can assert coverage.
    pub fn all() -> [Self; 4] {
        [
            Self::PreBurnIdleMovement,
            Self::ExtendedSettlementDrift,
            Self::FlatCreditsWithMeterMovement,
            Self::OverlappingSession,
        ]
    }
}

impl fmt::Display for ContaminationSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A validation failure while constructing contamination thresholds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContaminationThresholdsError {
    EmptyVersion,
    PostSettlementGraceIsZero,
}

impl fmt::Display for ContaminationThresholdsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyVersion => "contamination thresholds version cannot be empty",
            Self::PostSettlementGraceIsZero => "post-settlement grace must be non-zero",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ContaminationThresholdsError {}

/// The per-signal thresholds recorded on one experiment by `begin`.
///
/// Every numeric threshold lives here, so detection is a pure function of the
/// experiment record plus fresh evidence. The overlapping-session signal has no
/// numeric threshold by nature: any other session marked against the same
/// account inside the recorded window is evidence, and its configuration is the
/// window itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminationThresholds {
    version: String,
    pre_burn_max_movement_ppm: u32,
    post_settlement_max_movement_ppm: u32,
    post_settlement_grace: MonotonicDuration,
    flat_credits_min_meter_movement_ppm: u32,
    flat_credits_max_local: Credits,
}

impl ContaminationThresholds {
    /// Constructs thresholds with explicit per-signal values. Zero movement
    /// tolerances are allowed: zero means any movement at all fires the signal.
    /// Only the grace duration must be non-zero, since a zero grace would make
    /// ordinary settlement lag indistinguishable from drift by construction.
    pub fn new(
        version: impl Into<String>,
        pre_burn_max_movement_ppm: u32,
        post_settlement_max_movement_ppm: u32,
        post_settlement_grace: MonotonicDuration,
        flat_credits_min_meter_movement_ppm: u32,
        flat_credits_max_local: Credits,
    ) -> Result<Self, ContaminationThresholdsError> {
        let version = version.into();
        if version.trim().is_empty() {
            return Err(ContaminationThresholdsError::EmptyVersion);
        }
        if post_settlement_grace.as_nanos() == 0 {
            return Err(ContaminationThresholdsError::PostSettlementGraceIsZero);
        }
        Ok(Self {
            version,
            pre_burn_max_movement_ppm,
            post_settlement_max_movement_ppm,
            post_settlement_grace,
            flat_credits_min_meter_movement_ppm,
            flat_credits_max_local,
        })
    }

    /// The conservative defaults `begin` records on a new experiment: one
    /// reported-resolution step of tolerance before the burn and after the
    /// settlement grace, a sixty-minute grace matching the conservative
    /// settlement window, a two-point meter movement counting as substantial,
    /// and flat meaning exactly no locally attributed credits.
    pub fn conservative_default() -> Self {
        Self::new(
            "contamination-v1",
            DEFAULT_PRE_BURN_MAX_MOVEMENT_PPM,
            DEFAULT_POST_SETTLEMENT_MAX_MOVEMENT_PPM,
            MonotonicDuration::from_nanos(DEFAULT_POST_SETTLEMENT_GRACE_NANOS),
            DEFAULT_FLAT_CREDITS_MIN_METER_MOVEMENT_PPM,
            Credits::from_micros(DEFAULT_FLAT_CREDITS_MAX_LOCAL_MICROS),
        )
        .expect("the built-in contamination thresholds are valid")
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn pre_burn_max_movement_ppm(&self) -> u32 {
        self.pre_burn_max_movement_ppm
    }

    pub fn post_settlement_max_movement_ppm(&self) -> u32 {
        self.post_settlement_max_movement_ppm
    }

    pub fn post_settlement_grace(&self) -> MonotonicDuration {
        self.post_settlement_grace
    }

    pub fn flat_credits_min_meter_movement_ppm(&self) -> u32 {
        self.flat_credits_min_meter_movement_ppm
    }

    pub fn flat_credits_max_local(&self) -> Credits {
        self.flat_credits_max_local
    }
}

/// One meter reading used by the contamination detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContaminationMeterPoint {
    at: UtcTimestamp,
    quota_used: QuotaUsed,
}

impl ContaminationMeterPoint {
    pub fn new(at: UtcTimestamp, quota_used: QuotaUsed) -> Self {
        Self { at, quota_used }
    }

    pub fn at(self) -> UtcTimestamp {
        self.at
    }

    pub fn quota_used(self) -> QuotaUsed {
        self.quota_used
    }
}

/// One marker-timeline entry used by the overlapping-session check: which
/// session was observed against which account, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminationMarkerPoint {
    session_source: String,
    session_native: String,
    logical_account: String,
    observed_at: UtcTimestamp,
}

impl ContaminationMarkerPoint {
    pub fn new(
        session_source: impl Into<String>,
        session_native: impl Into<String>,
        logical_account: impl Into<String>,
        observed_at: UtcTimestamp,
    ) -> Self {
        Self {
            session_source: session_source.into(),
            session_native: session_native.into(),
            logical_account: logical_account.into(),
            observed_at,
        }
    }

    pub fn session_source(&self) -> &str {
        &self.session_source
    }

    pub fn session_native(&self) -> &str {
        &self.session_native
    }

    pub fn logical_account(&self) -> &str {
        &self.logical_account
    }

    pub fn observed_at(&self) -> UtcTimestamp {
        self.observed_at
    }

    /// The session identity as one display string, `source/native`.
    pub fn session_label(&self) -> String {
        format!("{}/{}", self.session_source, self.session_native)
    }
}

/// Everything the detector needs for one experiment at one instant.
///
/// The series are never reordered in place. The plateau period comes from the
/// experiment row (`begin` asserted it); the window end is the recorded `end`
/// once set, else the evaluation instant. Locally attributed credits are the
/// caller-computed total for the controlled window, in the experiment's cost
/// model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminationInputs<'a> {
    pub experiment_account: &'a str,
    pub baseline_plateau_started_at: UtcTimestamp,
    pub started_at: UtcTimestamp,
    pub ended_at: Option<UtcTimestamp>,
    pub evaluated_at: UtcTimestamp,
    pub pre_burn_series: &'a [ContaminationMeterPoint],
    pub post_series: &'a [ContaminationMeterPoint],
    pub controlled_meter_start: QuotaUsed,
    pub controlled_meter_end: QuotaUsed,
    pub local_credits_delta: Credits,
    pub markers: &'a [ContaminationMarkerPoint],
}

/// The magnitude of one fired signal, in that signal's own typed units.
/// Each variant carries the numbers the finding names, so a verdict that says
/// a signal fired always says by how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContaminationMagnitude {
    PreBurnIdleMovement {
        movement_ppm: u32,
    },
    ExtendedSettlementDrift {
        movement_ppm: u32,
    },
    FlatCreditsWithMeterMovement {
        meter_movement_ppm: u32,
        local_credits_micros: i64,
    },
    OverlappingSession {
        sessions: Vec<String>,
    },
}

impl ContaminationMagnitude {
    /// The signal this magnitude belongs to.
    pub fn signal(&self) -> ContaminationSignal {
        match self {
            Self::PreBurnIdleMovement { .. } => ContaminationSignal::PreBurnIdleMovement,
            Self::ExtendedSettlementDrift { .. } => ContaminationSignal::ExtendedSettlementDrift,
            Self::FlatCreditsWithMeterMovement { .. } => {
                ContaminationSignal::FlatCreditsWithMeterMovement
            }
            Self::OverlappingSession { .. } => ContaminationSignal::OverlappingSession,
        }
    }
}

impl fmt::Display for ContaminationMagnitude {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreBurnIdleMovement { movement_ppm } => {
                write!(formatter, "pre-burn movement of {movement_ppm} ppm")
            }
            Self::ExtendedSettlementDrift { movement_ppm } => {
                write!(formatter, "post-settlement movement of {movement_ppm} ppm")
            }
            Self::FlatCreditsWithMeterMovement {
                meter_movement_ppm,
                local_credits_micros,
            } => write!(
                formatter,
                "meter moved {meter_movement_ppm} ppm while local credits totalled {local_credits_micros} micros"
            ),
            Self::OverlappingSession { sessions } => {
                write!(
                    formatter,
                    "{} overlapping session(s): {}",
                    sessions.len(),
                    sessions.join(", ")
                )
            }
        }
    }
}

/// One fired signal with its magnitude and the threshold it exceeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminationFinding {
    pub signal: ContaminationSignal,
    pub magnitude: ContaminationMagnitude,
    pub detail: String,
}

impl ContaminationFinding {
    /// A one-line finding naming the signal and its magnitude, for the
    /// contamination mark on a published fit.
    pub fn summary(&self) -> String {
        format!("{}: {}", self.signal.label(), self.magnitude)
    }
}

/// The detector's exhaustive result: every signal that fired, possibly none.
/// An empty verdict is a clean experiment, never a lack of evidence: each
/// signal with no usable evidence simply does not fire (invariant 5: a number
/// the code cannot justify is not printed, and neither is a finding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminationVerdict {
    findings: Vec<ContaminationFinding>,
}

impl ContaminationVerdict {
    pub fn clean() -> Self {
        Self {
            findings: Vec::new(),
        }
    }

    pub fn is_contaminated(&self) -> bool {
        !self.findings.is_empty()
    }

    pub fn findings(&self) -> &[ContaminationFinding] {
        &self.findings
    }

    pub fn findings_for(&self, signal: ContaminationSignal) -> Vec<&ContaminationFinding> {
        self.findings
            .iter()
            .filter(|finding| finding.signal == signal)
            .collect()
    }

    /// The explicit mark a publisher attaches to a fit made from a
    /// contaminated experiment, naming every fired signal with its magnitude.
    /// `None` for a clean experiment: clean fits carry no mark.
    pub fn contamination_mark(&self) -> Option<String> {
        if self.findings.is_empty() {
            return None;
        }
        let parts = self
            .findings
            .iter()
            .map(ContaminationFinding::summary)
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!("contaminated({parts})"))
    }
}

/// Evaluates all four signals for one experiment. Every signal is always
/// computed; the verdict collects each one that fires.
pub fn evaluate_contamination(
    inputs: &ContaminationInputs<'_>,
    thresholds: &ContaminationThresholds,
) -> ContaminationVerdict {
    let mut findings = Vec::new();
    if let Some(finding) = check_pre_burn_idle(inputs, thresholds) {
        findings.push(finding);
    }
    if let Some(finding) = check_extended_settlement_drift(inputs, thresholds) {
        findings.push(finding);
    }
    if let Some(finding) = check_flat_credits(inputs, thresholds) {
        findings.push(finding);
    }
    if let Some(finding) = check_overlapping_sessions(inputs) {
        findings.push(finding);
    }
    ContaminationVerdict { findings }
}

/// Quota moving during the pre-burn idle period asserted by `begin`.
/// Only readings inside the recorded plateau period count: movement outside
/// that window is some other window's business.
fn check_pre_burn_idle(
    inputs: &ContaminationInputs<'_>,
    thresholds: &ContaminationThresholds,
) -> Option<ContaminationFinding> {
    let mut in_window: Vec<QuotaUsed> = inputs
        .pre_burn_series
        .iter()
        .filter(|point| {
            point.at >= inputs.baseline_plateau_started_at && point.at <= inputs.started_at
        })
        .map(|point| point.quota_used)
        .collect();
    if in_window.len() < 2 {
        return None;
    }
    in_window.sort_by_key(|quota| quota.as_ppm().get());
    let lowest = in_window.first().expect("two points are present");
    let highest = in_window.last().expect("two points are present");
    let movement = highest.as_ppm().get().saturating_sub(lowest.as_ppm().get());
    if movement > thresholds.pre_burn_max_movement_ppm {
        Some(ContaminationFinding {
            signal: ContaminationSignal::PreBurnIdleMovement,
            magnitude: ContaminationMagnitude::PreBurnIdleMovement {
                movement_ppm: movement,
            },
            detail: format!(
                "quota moved {movement} ppm inside the asserted idle plateau, above the recorded tolerance of {} ppm",
                thresholds.pre_burn_max_movement_ppm
            ),
        })
    } else {
        None
    }
}

/// Quota continuing to move far beyond the expected settlement interval: past
/// the end of controlled work plus the recorded grace. A lag model that is
/// wrong looks the same as contamination here, and the finding says so.
fn check_extended_settlement_drift(
    inputs: &ContaminationInputs<'_>,
    thresholds: &ContaminationThresholds,
) -> Option<ContaminationFinding> {
    let work_end = inputs.ended_at.unwrap_or(inputs.evaluated_at);
    let grace_nanos =
        i64::try_from(thresholds.post_settlement_grace.as_nanos()).unwrap_or(i64::MAX);
    let drift_start =
        UtcTimestamp::from_unix_nanos(work_end.unix_nanos().saturating_add(grace_nanos));
    let mut tail: Vec<QuotaUsed> = inputs
        .post_series
        .iter()
        .filter(|point| point.at > drift_start)
        .map(|point| point.quota_used)
        .collect();
    if tail.len() < 2 {
        return None;
    }
    tail.sort_by_key(|quota| quota.as_ppm().get());
    let lowest = tail.first().expect("two points are present");
    let highest = tail.last().expect("two points are present");
    let movement = highest.as_ppm().get().saturating_sub(lowest.as_ppm().get());
    if movement > thresholds.post_settlement_max_movement_ppm {
        Some(ContaminationFinding {
            signal: ContaminationSignal::ExtendedSettlementDrift,
            magnitude: ContaminationMagnitude::ExtendedSettlementDrift {
                movement_ppm: movement,
            },
            detail: format!(
                "quota moved {movement} ppm past the settlement grace, above the recorded tolerance of {} ppm; either contamination or a wrong lag model",
                thresholds.post_settlement_max_movement_ppm
            ),
        })
    } else {
        None
    }
}

/// Local controlled credits flat while meter movement is substantial: the
/// clearest case. Both halves come from the recorded thresholds.
fn check_flat_credits(
    inputs: &ContaminationInputs<'_>,
    thresholds: &ContaminationThresholds,
) -> Option<ContaminationFinding> {
    let start_ppm = inputs.controlled_meter_start.as_ppm().get();
    let end_ppm = inputs.controlled_meter_end.as_ppm().get();
    let meter_movement = start_ppm.abs_diff(end_ppm);
    let local_micros = inputs.local_credits_delta.micros();
    if meter_movement >= thresholds.flat_credits_min_meter_movement_ppm
        && local_micros <= thresholds.flat_credits_max_local.micros()
    {
        Some(ContaminationFinding {
            signal: ContaminationSignal::FlatCreditsWithMeterMovement,
            magnitude: ContaminationMagnitude::FlatCreditsWithMeterMovement {
                meter_movement_ppm: meter_movement,
                local_credits_micros: local_micros,
            },
            detail: format!(
                "meter moved {meter_movement} ppm across controlled work while local credits totalled {local_micros} micros (flat at or under {} micros)",
                thresholds.flat_credits_max_local.micros()
            ),
        })
    } else {
        None
    }
}

/// Another locally known session marked against the same account inside the
/// experiment window. Reports which sessions overlapped.
fn check_overlapping_sessions(inputs: &ContaminationInputs<'_>) -> Option<ContaminationFinding> {
    let window_end = inputs.ended_at.unwrap_or(inputs.evaluated_at);
    let mut sessions: Vec<String> = inputs
        .markers
        .iter()
        .filter(|marker| {
            marker.logical_account == inputs.experiment_account
                && marker.observed_at >= inputs.started_at
                && marker.observed_at <= window_end
        })
        .map(ContaminationMarkerPoint::session_label)
        .collect();
    sessions.sort();
    sessions.dedup();
    if sessions.is_empty() {
        None
    } else {
        Some(ContaminationFinding {
            signal: ContaminationSignal::OverlappingSession,
            magnitude: ContaminationMagnitude::OverlappingSession {
                sessions: sessions.clone(),
            },
            detail: format!(
                "{} other session(s) marked against the same account inside the experiment window",
                sessions.len()
            ),
        })
    }
}

/// A contaminated candidate refused for activation: the refusal names the
/// first fired signal with its magnitude, so the operator knows what to chase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContaminatedCandidateRefusal {
    pub signal: ContaminationSignal,
    pub magnitude: ContaminationMagnitude,
}

impl fmt::Display for ContaminatedCandidateRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "candidate refused for activation: contaminated by {}: {}",
            self.signal.label(),
            self.magnitude
        )
    }
}

impl std::error::Error for ContaminatedCandidateRefusal {}

/// The activation gate: a contaminated candidate is never activatable.
/// A clean verdict passes; any finding refuses with the first signal named.
pub fn require_uncontaminated_for_activation(
    verdict: &ContaminationVerdict,
) -> Result<(), ContaminatedCandidateRefusal> {
    match verdict.findings.first() {
        None => Ok(()),
        Some(finding) => Err(ContaminatedCandidateRefusal {
            signal: finding.signal,
            magnitude: finding.magnitude.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::QuotaFractionPpm;

    fn timestamp(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    fn quota(ppm: i32) -> QuotaUsed {
        QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap())
    }

    fn point(at_nanos: i64, ppm: i32) -> ContaminationMeterPoint {
        ContaminationMeterPoint::new(timestamp(at_nanos), quota(ppm))
    }

    fn marker(
        source: &str,
        native: &str,
        account: &str,
        at_nanos: i64,
    ) -> ContaminationMarkerPoint {
        ContaminationMarkerPoint::new(source, native, account, timestamp(at_nanos))
    }

    fn clean_inputs<'a>(
        pre_burn_series: &'a [ContaminationMeterPoint],
        post_series: &'a [ContaminationMeterPoint],
        markers: &'a [ContaminationMarkerPoint],
    ) -> ContaminationInputs<'a> {
        ContaminationInputs {
            experiment_account: "work-a",
            baseline_plateau_started_at: timestamp(0),
            started_at: timestamp(1_000),
            ended_at: Some(timestamp(2_000)),
            evaluated_at: timestamp(3_000),
            pre_burn_series,
            post_series,
            controlled_meter_start: quota(100_000),
            controlled_meter_end: quota(100_000),
            local_credits_delta: Credits::from_micros(5_000_000),
            markers,
        }
    }

    #[test]
    fn all_four_signals_are_computed_for_every_experiment() {
        let pre = vec![point(0, 100_000), point(500, 150_000)];
        let post = vec![
            point(2_000 + 3_600_000_000_000 + 1, 100_000),
            point(2_000 + 3_600_000_000_000 + 2, 150_000),
        ];
        let markers = vec![marker("claude-code", "sess-other", "work-a", 1_500)];
        let inputs = ContaminationInputs {
            controlled_meter_start: quota(100_000),
            controlled_meter_end: quota(200_000),
            local_credits_delta: Credits::from_micros(0),
            ..clean_inputs(&pre, &post, &markers)
        };
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        for signal in ContaminationSignal::all() {
            assert_eq!(
                verdict.findings_for(signal).len(),
                1,
                "signal {signal} must fire exactly once on the fully contaminated fixture"
            );
        }
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn pre_burn_idle_movement_fires_and_reports_its_magnitude() {
        let pre = vec![point(0, 100_000), point(500, 130_000)];
        let post = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        let findings = verdict.findings_for(ContaminationSignal::PreBurnIdleMovement);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].magnitude,
            ContaminationMagnitude::PreBurnIdleMovement {
                movement_ppm: 30_000
            }
        );
        assert!(findings[0].summary().contains("pre_burn_idle_movement"));
        assert!(findings[0].summary().contains("30000"));
    }

    /// The planted negative for the pre-burn check: the same movement with a
    /// tighter recorded tolerance stays quiet, so the test proves the
    /// threshold is read rather than assumed.
    #[test]
    fn pre_burn_idle_movement_below_tolerance_stays_clean() {
        let pre = vec![point(0, 100_000), point(500, 105_000)];
        let post = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(
            verdict
                .findings_for(ContaminationSignal::PreBurnIdleMovement)
                .is_empty()
        );
        assert!(!verdict.is_contaminated());
    }

    #[test]
    fn extended_settlement_drift_fires_and_reports_its_magnitude() {
        let grace = DEFAULT_POST_SETTLEMENT_GRACE_NANOS as i64;
        let post = vec![
            point(2_000 + grace + 1, 100_000),
            point(2_000 + grace + 2, 140_000),
        ];
        let pre = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        let findings = verdict.findings_for(ContaminationSignal::ExtendedSettlementDrift);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].magnitude,
            ContaminationMagnitude::ExtendedSettlementDrift {
                movement_ppm: 40_000
            }
        );
    }

    /// Movement inside the grace period is ordinary settlement lag, not drift.
    #[test]
    fn movement_inside_the_settlement_grace_is_not_drift() {
        let grace = DEFAULT_POST_SETTLEMENT_GRACE_NANOS as i64;
        let post = vec![point(2_000 + 1, 100_000), point(2_000 + grace - 1, 500_000)];
        let pre = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(
            verdict
                .findings_for(ContaminationSignal::ExtendedSettlementDrift)
                .is_empty()
        );
    }

    #[test]
    fn flat_credits_with_meter_movement_fires_and_reports_both_halves() {
        let pre = vec![];
        let post = vec![];
        let markers = vec![];
        let inputs = ContaminationInputs {
            controlled_meter_start: quota(100_000),
            controlled_meter_end: quota(160_000),
            local_credits_delta: Credits::from_micros(0),
            ..clean_inputs(&pre, &post, &markers)
        };
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        let findings = verdict.findings_for(ContaminationSignal::FlatCreditsWithMeterMovement);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].magnitude,
            ContaminationMagnitude::FlatCreditsWithMeterMovement {
                meter_movement_ppm: 60_000,
                local_credits_micros: 0,
            }
        );
        assert!(findings[0].summary().contains("60000"));
    }

    /// The planted negative for the flat-credits check: substantial local
    /// credits explain the meter movement, so no finding fires.
    #[test]
    fn explained_meter_movement_with_real_local_credits_stays_clean() {
        let pre = vec![];
        let post = vec![];
        let markers = vec![];
        let inputs = ContaminationInputs {
            controlled_meter_start: quota(100_000),
            controlled_meter_end: quota(160_000),
            local_credits_delta: Credits::from_micros(9_000_000),
            ..clean_inputs(&pre, &post, &markers)
        };
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(
            verdict
                .findings_for(ContaminationSignal::FlatCreditsWithMeterMovement)
                .is_empty()
        );
        assert!(!verdict.is_contaminated());
    }

    #[test]
    fn overlapping_session_reports_which_sessions_overlapped() {
        let pre = vec![];
        let post = vec![];
        let markers = vec![
            marker("claude-code", "sess-other", "work-a", 1_500),
            marker("codex", "sess-elsewhere", "work-a", 1_800),
            marker("claude-code", "sess-mine-elsewhere", "personal", 1_500),
            marker("claude-code", "sess-before", "work-a", 500),
        ];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        let findings = verdict.findings_for(ContaminationSignal::OverlappingSession);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].magnitude,
            ContaminationMagnitude::OverlappingSession {
                sessions: vec![
                    "claude-code/sess-other".to_string(),
                    "codex/sess-elsewhere".to_string(),
                ],
            }
        );
    }

    /// The planted negative for the overlap check: markers for other accounts
    /// and markers outside the window are not evidence against this experiment.
    #[test]
    fn markers_for_other_accounts_or_outside_the_window_do_not_overlap() {
        let pre = vec![];
        let post = vec![];
        let markers = vec![
            marker("claude-code", "sess-other-account", "personal", 1_500),
            marker("claude-code", "sess-after", "work-a", 2_500),
        ];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(
            verdict
                .findings_for(ContaminationSignal::OverlappingSession)
                .is_empty()
        );
    }

    #[test]
    fn the_pre_burn_check_uses_only_the_asserted_plateau_period() {
        // Movement before the asserted plateau start is some earlier window's
        // business, even when it is large.
        let pre = vec![
            point(-500, 100_000),
            point(-100, 400_000),
            point(0, 400_000),
            point(500, 400_000),
        ];
        let post = vec![];
        let markers = vec![];
        let mut inputs = clean_inputs(&pre, &post, &markers);
        inputs.baseline_plateau_started_at = timestamp(0);
        inputs.controlled_meter_start = quota(400_000);
        inputs.controlled_meter_end = quota(400_000);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(
            verdict
                .findings_for(ContaminationSignal::PreBurnIdleMovement)
                .is_empty(),
            "movement before the asserted plateau start must not fire the pre-burn signal"
        );
    }

    #[test]
    fn each_threshold_is_read_from_configuration_not_from_a_constant() {
        // The pre-burn series is stable, so the only signal that can fire is
        // flat-credits: the strict-versus-permissive difference below is then
        // attributable to the flat-credits meter threshold alone.
        let pre = vec![point(0, 100_000), point(500, 100_000)];
        let empty_pre: Vec<ContaminationMeterPoint> = vec![];
        let markers: Vec<ContaminationMarkerPoint> = vec![];
        let permissive = ContaminationThresholds::new(
            "permissive-v1",
            50_000,
            50_000,
            MonotonicDuration::from_nanos(DEFAULT_POST_SETTLEMENT_GRACE_NANOS),
            100_000,
            Credits::from_micros(0),
        )
        .unwrap();
        let inputs = ContaminationInputs {
            controlled_meter_start: quota(100_000),
            controlled_meter_end: quota(130_000),
            local_credits_delta: Credits::from_micros(0),
            ..clean_inputs(&pre, &empty_pre, &markers)
        };
        let strict_verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(strict_verdict.is_contaminated());
        assert_eq!(
            strict_verdict
                .findings_for(ContaminationSignal::FlatCreditsWithMeterMovement)
                .len(),
            1,
            "with a stable pre-burn series only flat-credits may fire under the strict thresholds"
        );
        let permissive_verdict = evaluate_contamination(&inputs, &permissive);
        assert!(
            !permissive_verdict.is_contaminated(),
            "the same evidence under permissive recorded thresholds must read clean"
        );
        assert_eq!(permissive.version(), "permissive-v1");
    }

    #[test]
    fn a_contaminated_candidate_is_refused_for_activation_with_signal_named() {
        let pre = vec![point(0, 100_000), point(500, 150_000)];
        let post = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(verdict.is_contaminated());
        let refusal = require_uncontaminated_for_activation(&verdict).unwrap_err();
        assert_eq!(refusal.signal, ContaminationSignal::PreBurnIdleMovement);
        assert!(refusal.to_string().contains("pre_burn_idle_movement"));
        assert!(refusal.to_string().contains("50000"));
    }

    #[test]
    fn a_clean_verdict_passes_the_activation_gate() {
        let pre = vec![point(0, 100_000), point(500, 100_000)];
        let post = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(!verdict.is_contaminated());
        assert!(require_uncontaminated_for_activation(&verdict).is_ok());
        assert!(verdict.contamination_mark().is_none());
    }

    #[test]
    fn the_contamination_mark_names_every_fired_signal_with_magnitude() {
        let pre = vec![point(0, 100_000), point(500, 150_000)];
        let post = vec![];
        let markers = vec![marker("claude-code", "sess-other", "work-a", 1_500)];
        let inputs = ContaminationInputs {
            controlled_meter_start: quota(100_000),
            controlled_meter_end: quota(200_000),
            local_credits_delta: Credits::from_micros(0),
            ..clean_inputs(&pre, &post, &markers)
        };
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        let mark = verdict.contamination_mark().expect("must be marked");
        assert!(mark.starts_with("contaminated("));
        assert!(mark.contains("pre_burn_idle_movement"));
        assert!(mark.contains("flat_credits_with_meter_movement"));
        assert!(mark.contains("overlapping_session"));
        assert!(mark.contains("claude-code/sess-other"));
    }

    #[test]
    fn empty_evidence_fires_no_signal() {
        let pre = vec![];
        let post = vec![];
        let markers = vec![];
        let inputs = clean_inputs(&pre, &post, &markers);
        let verdict =
            evaluate_contamination(&inputs, &ContaminationThresholds::conservative_default());
        assert!(!verdict.is_contaminated());
    }

    #[test]
    fn threshold_construction_refuses_an_empty_version_and_a_zero_grace() {
        assert_eq!(
            ContaminationThresholds::new(
                "  ",
                1,
                1,
                MonotonicDuration::from_nanos(1),
                1,
                Credits::from_micros(0),
            ),
            Err(ContaminationThresholdsError::EmptyVersion)
        );
        assert_eq!(
            ContaminationThresholds::new(
                "v1",
                1,
                1,
                MonotonicDuration::from_nanos(0),
                1,
                Credits::from_micros(0),
            ),
            Err(ContaminationThresholdsError::PostSettlementGraceIsZero)
        );
    }
}
