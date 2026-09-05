//! Calibration candidate fitter over quantized provider readings.
//!
//! Provider readings are not infinitely precise scalars: a reading displaying
//! 41 percent under round-to-nearest is an admissible interval centred on 41
//! points. Fitting to the scalar midpoint manufactures artificial drift and
//! residual alarms out of the provider's rounding (PLAN.md 12.5, 23.5).
//!
//! This module fits candidate calibrations from qualified observations and
//! local credit spend using robust Huber regression over admissible intervals.
//! It produces an immutable candidate and never activates it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rusqlite::Connection;

use crate::cost_model::convert as convert_usage;
use crate::domain::credits::{Credits, CreditsPerPercentagePoint};
use crate::domain::provenance::EvidenceId;
use crate::domain::time::{Clock, UtcTimestamp};
use crate::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenKind,
    UsageVector,
};
use crate::domain::window::QuantizationSemantics;
use crate::error::Error;
use crate::evidence::{CoverageCompleteness, Derivation, EvidenceQuality};
use crate::store::calibration::{
    CandidateId, CoefficientUncertainty, EvidenceDigest, ExcludedSample, ExperimentId, LagHandling,
    WindowCalibrationCandidate, insert_candidate, load_candidate, load_experiment,
    load_experiment_observations, load_experiment_usage, load_latest_experiment,
};
use crate::store::cost_model::load_active_at as load_active_cost_model_at;

/// Possible causes for a large fitted intercept diagnostic finding.
pub const INTERCEPT_POSSIBLE_CAUSES: [&str; 3] =
    ["contamination", "lag mismatch", "incomplete cost model"];

/// The admissible interval of quota movement (in parts per million) derived
/// from a provider reading, reported resolution, and quantization semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissibleInterval {
    lower_ppm: i64,
    upper_ppm: i64,
}

impl AdmissibleInterval {
    /// Derives the admissible interval from a reading, resolution, and semantics.
    pub fn from_reading(
        reading_ppm: i64,
        resolution_ppm: i64,
        quantization: QuantizationSemantics,
    ) -> Self {
        let (lower_ppm, upper_ppm) = match quantization {
            QuantizationSemantics::Exact => (reading_ppm, reading_ppm),
            QuantizationSemantics::RoundedToNearest => {
                let below = resolution_ppm / 2;
                (reading_ppm - below, reading_ppm + (resolution_ppm - below))
            }
            QuantizationSemantics::RoundedDown => (reading_ppm, reading_ppm + resolution_ppm),
            QuantizationSemantics::RoundedUp => (reading_ppm - resolution_ppm, reading_ppm),
            QuantizationSemantics::Unknown => {
                (reading_ppm - resolution_ppm, reading_ppm + resolution_ppm)
            }
        };
        Self {
            lower_ppm,
            upper_ppm,
        }
    }

    pub fn lower_ppm(self) -> i64 {
        self.lower_ppm
    }

    pub fn upper_ppm(self) -> i64 {
        self.upper_ppm
    }

    /// Residual distance from a predicted value to the admissible interval in ppm.
    /// Returns 0.0 if the predicted value falls inside the interval.
    pub fn residual_ppm(self, predicted_ppm: f64) -> f64 {
        let low = self.lower_ppm as f64;
        let high = self.upper_ppm as f64;
        if predicted_ppm < low {
            low - predicted_ppm
        } else if predicted_ppm > high {
            predicted_ppm - high
        } else {
            0.0
        }
    }

    /// Signed deviation from interval: negative if below, positive if above, 0.0 inside.
    pub fn signed_deviation_ppm(self, predicted_ppm: f64) -> f64 {
        let low = self.lower_ppm as f64;
        let high = self.upper_ppm as f64;
        if predicted_ppm < low {
            predicted_ppm - low
        } else if predicted_ppm > high {
            predicted_ppm - high
        } else {
            0.0
        }
    }

    pub fn contains_ppm(self, value_ppm: f64) -> bool {
        (self.lower_ppm as f64) <= value_ppm && value_ppm <= (self.upper_ppm as f64)
    }
}

/// One observation entering calibration fitting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitObservation {
    pub evidence_id: EvidenceId,
    pub at: UtcTimestamp,
    pub quota_used_ppm: i64,
    pub reported_resolution_ppm: i64,
    pub quantization: QuantizationSemantics,
    pub cumulative_credits: Credits,
}

impl FitObservation {
    pub fn new(
        evidence_id: EvidenceId,
        at: UtcTimestamp,
        quota_used_ppm: i64,
        reported_resolution_ppm: i64,
        quantization: QuantizationSemantics,
        cumulative_credits: Credits,
    ) -> Self {
        Self {
            evidence_id,
            at,
            quota_used_ppm,
            reported_resolution_ppm,
            quantization,
            cumulative_credits,
        }
    }

    /// The admissible interval in ppm for this observation.
    pub fn interval(&self) -> AdmissibleInterval {
        AdmissibleInterval::from_reading(
            self.quota_used_ppm,
            self.reported_resolution_ppm,
            self.quantization,
        )
    }
}

/// A diagnostic finding emitted by calibration fitting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticFinding {
    /// A large fitted intercept was detected, which indicates non-ideal experimental conditions.
    LargeIntercept {
        intercept_ppm: i64,
        threshold_ppm: i64,
        possible_causes: [&'static str; 3],
    },
}

impl DiagnosticFinding {
    pub fn possible_causes(&self) -> &[&'static str] {
        match self {
            Self::LargeIntercept {
                possible_causes, ..
            } => possible_causes,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::LargeIntercept {
                intercept_ppm,
                threshold_ppm,
                possible_causes,
            } => {
                format!(
                    "large fitted intercept ({} ppm exceeds threshold {} ppm); possible causes: {}",
                    intercept_ppm,
                    threshold_ppm,
                    possible_causes.join(", ")
                )
            }
        }
    }
}

/// Typed rejection reasons for calibration fitting.
#[derive(Debug, Clone, PartialEq)]
pub enum FitRejection {
    InsufficientObservations { found: usize, required: usize },
    Underidentified { usable: usize },
    NonPositiveSlope { slope_ppm_per_credit: f64 },
    ZeroCreditSpan,
    BaselinePlateauNotSettled,
    TerminalPlateauNotSettled,
    MissingCostModelTerm { details: String },
    ContaminatedSeries { details: String },
}

impl fmt::Display for FitRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientObservations { found, required } => {
                write!(
                    f,
                    "insufficient observations: found {found}, required at least {required}"
                )
            }
            Self::Underidentified { usable } => {
                write!(
                    f,
                    "fit is underidentified with {usable} usable observations"
                )
            }
            Self::NonPositiveSlope {
                slope_ppm_per_credit,
            } => {
                write!(
                    f,
                    "non-positive slope fitted: {slope_ppm_per_credit} ppm/credit"
                )
            }
            Self::ZeroCreditSpan => {
                write!(f, "zero credit movement across observations")
            }
            Self::BaselinePlateauNotSettled => {
                write!(f, "baseline meter plateau did not settle")
            }
            Self::TerminalPlateauNotSettled => {
                write!(f, "terminal meter plateau did not settle")
            }
            Self::MissingCostModelTerm { details } => {
                write!(f, "missing cost model term: {details}")
            }
            Self::ContaminatedSeries { details } => {
                write!(f, "contaminated observation series: {details}")
            }
        }
    }
}

impl std::error::Error for FitRejection {}

impl FitRejection {
    pub fn into_error(self) -> Error {
        Error::InsufficientEvidence(format!("fit rejected: {self}"))
    }
}

/// The complete result of a calibration fit.
#[derive(Debug, Clone, PartialEq)]
pub struct FitResult {
    pub candidate: WindowCalibrationCandidate,
    pub residual_percentage_points: f64,
    pub lag_handling: LagHandling,
    pub statistical_method: String,
    pub statistical_parameters: String,
    pub usable_observations: u32,
    pub excluded_samples: Vec<ExcludedSample>,
    pub diagnostic_findings: Vec<DiagnosticFinding>,
}

/// Fits a candidate calibration over quantized provider observations.
///
/// Readings enter the fit as admissible intervals rather than scalars.
/// The regression uses Theil-Sen median initialization and Huber loss minimization
/// over interval residuals to remain robust to quantization plateaus and batched updates.
pub fn fit(
    observations: &[FitObservation],
    experiment: &crate::store::calibration::CalibrationExperiment,
) -> Result<FitResult, FitRejection> {
    if observations.len() < 2 {
        return Err(FitRejection::InsufficientObservations {
            found: observations.len(),
            required: 2,
        });
    }

    let mut sorted = observations.to_vec();
    sorted.sort_by_key(|o| (o.at, o.evidence_id.as_str().to_string()));

    let mut usable = Vec::new();
    let mut excluded_samples = Vec::new();

    let mut prev_ts: Option<UtcTimestamp> = None;
    let mut prev_used_ppm: Option<i64> = None;
    let mut reset_detected = false;

    for obs in &sorted {
        if reset_detected {
            if let Ok(ex) = ExcludedSample::new(
                obs.evidence_id.as_str(),
                "excluded: follows quota reset crossing",
            ) {
                excluded_samples.push(ex);
            }
            continue;
        }

        if let Some(prev) = prev_ts {
            if obs.at < prev {
                if let Ok(ex) = ExcludedSample::new(
                    obs.evidence_id.as_str(),
                    "excluded: out of order timestamp",
                ) {
                    excluded_samples.push(ex);
                }
                continue;
            }
            if obs.at == prev {
                if let Ok(ex) =
                    ExcludedSample::new(obs.evidence_id.as_str(), "excluded: duplicate timestamp")
                {
                    excluded_samples.push(ex);
                }
                continue;
            }
        }

        if let Some(prev_ppm) = prev_used_ppm {
            // A drop exceeding reported resolution indicates a window reset
            if obs.quota_used_ppm < prev_ppm - obs.reported_resolution_ppm {
                reset_detected = true;
                if let Ok(ex) = ExcludedSample::new(
                    obs.evidence_id.as_str(),
                    "excluded: quota reset crossing detected",
                ) {
                    excluded_samples.push(ex);
                }
                continue;
            }
        }

        prev_ts = Some(obs.at);
        prev_used_ppm = Some(obs.quota_used_ppm);
        usable.push(obs.clone());
    }

    if usable.len() < 2 {
        return Err(FitRejection::InsufficientObservations {
            found: usable.len(),
            required: 2,
        });
    }

    let min_credits = usable
        .iter()
        .map(|o| o.cumulative_credits.micros())
        .min()
        .unwrap_or(0);
    let max_credits = usable
        .iter()
        .map(|o| o.cumulative_credits.micros())
        .max()
        .unwrap_or(0);

    if min_credits == max_credits {
        return Err(FitRejection::ZeroCreditSpan);
    }

    // Credits relative to baseline in units of Credits (1 credit = 1_000_000 micros)
    let x_vals: Vec<f64> = usable
        .iter()
        .map(|o| (o.cumulative_credits.micros() - min_credits) as f64 / 1_000_000.0)
        .collect();

    let intervals: Vec<AdmissibleInterval> = usable.iter().map(|o| o.interval()).collect();
    let y_mids: Vec<f64> = intervals
        .iter()
        .map(|inv| (inv.lower_ppm() + inv.upper_ppm()) as f64 / 2.0)
        .collect();

    // Baseline reading for coordinate frame: delta_y = reading - base_reading
    let base_reading_ppm = y_mids[0];
    let delta_intervals: Vec<AdmissibleInterval> = intervals
        .iter()
        .map(|inv| AdmissibleInterval {
            lower_ppm: inv.lower_ppm() - base_reading_ppm.round() as i64,
            upper_ppm: inv.upper_ppm() - base_reading_ppm.round() as i64,
        })
        .collect();
    let delta_y_mids: Vec<f64> = y_mids.iter().map(|ym| ym - base_reading_ppm).collect();

    let n = usable.len();

    // Theil-Sen estimator: median of pairwise slopes
    let mut pairwise_slopes = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let dx = x_vals[j] - x_vals[i];
            if dx.abs() > 1e-6 {
                let dy = delta_y_mids[j] - delta_y_mids[i];
                pairwise_slopes.push(dy / dx);
            }
        }
    }

    if pairwise_slopes.is_empty() {
        return Err(FitRejection::Underidentified { usable: n });
    }

    pairwise_slopes.sort_by(|a, b| a.total_cmp(b));
    let initial_slope = median_sorted(&pairwise_slopes);

    if initial_slope <= 0.0 {
        return Err(FitRejection::NonPositiveSlope {
            slope_ppm_per_credit: initial_slope,
        });
    }

    // Median intercept in delta coordinates: delta_y = intercept + slope * delta_x
    let mut pairwise_intercepts: Vec<f64> = (0..n)
        .map(|i| delta_y_mids[i] - initial_slope * x_vals[i])
        .collect();
    pairwise_intercepts.sort_by(|a, b| a.total_cmp(b));
    let initial_intercept = median_sorted(&pairwise_intercepts);

    // Robust Huber interval optimization
    let resolution_ppm = usable
        .first()
        .map(|o| o.reported_resolution_ppm)
        .unwrap_or(10_000);
    let delta_ppm = (resolution_ppm as f64 / 2.0).clamp(100.0, 5000.0);

    let (fitted_intercept, fitted_slope) = optimize_huber_interval(
        &x_vals,
        &delta_intervals,
        initial_intercept,
        initial_slope,
        delta_ppm,
    );

    if fitted_slope <= 0.0 {
        return Err(FitRejection::NonPositiveSlope {
            slope_ppm_per_credit: fitted_slope,
        });
    }

    // Compute uncertainty interval on slope from pairwise slopes spread or interval bounds
    let p16_idx = (pairwise_slopes.len() as f64 * 0.16).floor() as usize;
    let p84_idx = ((pairwise_slopes.len() as f64 * 0.84).ceil() as usize)
        .min(pairwise_slopes.len().saturating_sub(1));

    let slope_low = pairwise_slopes[p16_idx].max(1e-6).min(fitted_slope);
    let slope_high = pairwise_slopes[p84_idx].max(fitted_slope);

    // Slope is ppm per credit.
    // 1 point of PercentagePoints = 1 ppm.
    // Credits per 1 ppm = 1.0 / (slope ppm/credit) credits = 1_000_000 / slope micro-credits.
    let micros_per_point = (1_000_000.0 / fitted_slope).round() as i64;
    let unc_low_micros = (1_000_000.0 / slope_high).round() as i64;
    let unc_high_micros = (1_000_000.0 / slope_low).round() as i64;

    let (unc_low_micros, unc_high_micros) = if unc_low_micros <= unc_high_micros {
        (unc_low_micros, unc_high_micros)
    } else {
        (unc_high_micros, unc_low_micros)
    };

    let fitted = CreditsPerPercentagePoint::from_micros_per_point(micros_per_point.max(1));
    let uncertainty = CoefficientUncertainty::new(
        CreditsPerPercentagePoint::from_micros_per_point(unc_low_micros.max(1)),
        CreditsPerPercentagePoint::from_micros_per_point(
            unc_high_micros.max(unc_low_micros.max(1)),
        ),
    )
    .map_err(|e| FitRejection::ContaminatedSeries {
        details: format!("invalid uncertainty interval: {e}"),
    })?;

    // Full-window capacity (100% = 1,000,000 ppm) in Credits
    let equivalent_full_window_capacity = Credits::from_micros(
        (micros_per_point as i128 * 1_000_000)
            .min(i64::MAX as i128)
            .max(0) as i64,
    );

    // Compute interval residuals in delta coordinates
    let residuals_ppm: Vec<f64> = delta_intervals
        .iter()
        .zip(x_vals.iter())
        .map(|(inv, &x)| inv.residual_ppm(fitted_intercept + fitted_slope * x))
        .collect();

    let mean_residual_ppm = residuals_ppm.iter().sum::<f64>() / n as f64;
    let residual_percentage_points = mean_residual_ppm / 10_000.0;
    let fit_residual_micros = (mean_residual_ppm * micros_per_point as f64).round() as i64;
    let fit_residual = Credits::from_micros(fit_residual_micros.max(0));

    // Intercept diagnostic: check if fitted intercept (delta at 0 credits) deviates significantly from zero
    let net_intercept_ppm = fitted_intercept.round() as i64;
    let intercept_threshold_ppm = resolution_ppm.max(5000);

    let mut diagnostic_findings = Vec::new();
    if net_intercept_ppm.abs() > intercept_threshold_ppm {
        diagnostic_findings.push(DiagnosticFinding::LargeIntercept {
            intercept_ppm: net_intercept_ppm,
            threshold_ppm: intercept_threshold_ppm,
            possible_causes: INTERCEPT_POSSIBLE_CAUSES,
        });
    }

    // Inputs digest from all consumed observations
    let mut inputs_set: BTreeSet<EvidenceId> = BTreeSet::new();
    for obs in observations {
        inputs_set.insert(obs.evidence_id.clone());
    }
    let inputs = EvidenceDigest::from_inputs(&inputs_set);

    let candidate_id = CandidateId::new(format!(
        "cand-{}-{:016x}",
        experiment.id.as_str(),
        inputs.digest()
    ));

    let candidate = WindowCalibrationCandidate {
        id: candidate_id,
        experiment: experiment.id.clone(),
        provider: experiment.provider.clone(),
        plan_tier: experiment.plan_tier.clone(),
        window_semantic_key: experiment.window_semantic_key.clone(),
        fitted,
        equivalent_full_window_capacity,
        fit_residual,
        uncertainty,
        sample_count: n as u32,
        inputs,
        validity: experiment.validity,
        knowledge_time: experiment.knowledge_time,
    };

    let lag_handling = LagHandling::new("settled-boundary-cancellation");
    let statistical_method = "theil-sen-huber-interval".to_string();
    let statistical_parameters =
        format!("huber_delta_ppm={delta_ppm:.1};init=theil_sen;quantization=admissible_interval");

    Ok(FitResult {
        candidate,
        residual_percentage_points,
        lag_handling,
        statistical_method,
        statistical_parameters,
        usable_observations: n as u32,
        excluded_samples,
        diagnostic_findings,
    })
}

/// Helper for median of a sorted slice.
fn median_sorted(slice: &[f64]) -> f64 {
    let len = slice.len();
    if len == 0 {
        return 0.0;
    }
    if len % 2 == 1 {
        slice[len / 2]
    } else {
        (slice[len / 2 - 1] + slice[len / 2]) / 2.0
    }
}

/// Optimizes Huber loss over admissible interval residuals.
fn optimize_huber_interval(
    x_vals: &[f64],
    intervals: &[AdmissibleInterval],
    init_intercept: f64,
    init_slope: f64,
    delta: f64,
) -> (f64, f64) {
    let n = x_vals.len();
    let mut intercept = init_intercept;
    let mut slope = init_slope;

    let x_mean = x_vals.iter().sum::<f64>() / n as f64;
    let var_x = x_vals.iter().map(|&x| (x - x_mean).powi(2)).sum::<f64>() / n as f64;
    let step_scale = (1.0 / (1.0 + var_x + x_mean.powi(2))).clamp(1e-5, 0.5) / n as f64;

    for _ in 0..100 {
        let mut grad_alpha = 0.0;
        let mut grad_beta = 0.0;

        for (inv, &x) in intervals.iter().zip(x_vals.iter()) {
            let y_hat = intercept + slope * x;
            let dev = inv.signed_deviation_ppm(y_hat);
            let psi = if dev.abs() <= delta {
                dev
            } else {
                delta * dev.signum()
            };
            grad_alpha += psi;
            grad_beta += psi * x;
        }

        let step_alpha = step_scale * grad_alpha;
        let step_beta = step_scale * grad_beta;

        intercept -= step_alpha;
        slope -= step_beta;

        if step_alpha.abs() < 1e-6 && step_beta.abs() < 1e-7 {
            break;
        }
    }

    (intercept, slope)
}

/// Fits a scalar regression over the same observations using midpoints/scalar readings.
///
/// Returns (slope_ppm_per_credit, mean_scalar_residual_ppm).
/// This function is used to prove that scalar fitting manufactures artificial drift
/// and residuals when provider readings are quantized (Acceptance Criterion 1).
pub fn fit_scalar_for_comparison(
    observations: &[FitObservation],
    _experiment: &crate::store::calibration::CalibrationExperiment,
) -> Result<(f64, f64), FitRejection> {
    if observations.len() < 2 {
        return Err(FitRejection::InsufficientObservations {
            found: observations.len(),
            required: 2,
        });
    }

    let min_credits = observations
        .iter()
        .map(|o| o.cumulative_credits.micros())
        .min()
        .unwrap_or(0);

    let x_vals: Vec<f64> = observations
        .iter()
        .map(|o| (o.cumulative_credits.micros() - min_credits) as f64 / 1_000_000.0)
        .collect();

    // Scalar reading is the reported scalar quota_used_ppm
    let y_vals: Vec<f64> = observations
        .iter()
        .map(|o| o.quota_used_ppm as f64)
        .collect();

    let n = observations.len();
    let mut slopes = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            let dx = x_vals[j] - x_vals[i];
            if dx.abs() > 1e-6 {
                slopes.push((y_vals[j] - y_vals[i]) / dx);
            }
        }
    }

    if slopes.is_empty() {
        return Err(FitRejection::Underidentified { usable: n });
    }

    slopes.sort_by(|a, b| a.total_cmp(b));
    let slope = median_sorted(&slopes);

    let mut intercepts: Vec<f64> = (0..n).map(|i| y_vals[i] - slope * x_vals[i]).collect();
    intercepts.sort_by(|a, b| a.total_cmp(b));
    let intercept = median_sorted(&intercepts);

    // Compute scalar residuals: distance from the quantized rounded reading
    let mean_scalar_residual = (0..n)
        .map(|i| (intercept + slope * x_vals[i] - y_vals[i]).abs())
        .sum::<f64>()
        / n as f64;

    Ok((slope, mean_scalar_residual))
}

/// Executes fitting from the database and inserts the resulting candidate row immutably.
///
/// Never activates the candidate (creates no `calibration_lifecycle` entry).
pub fn fit_and_record_candidate(
    conn: &Connection,
    experiment_id: Option<&ExperimentId>,
    clock: &impl Clock,
) -> Result<FitResult, Error> {
    let experiment = match experiment_id {
        Some(id) => load_experiment(conn, id)?.ok_or_else(|| {
            Error::InsufficientEvidence(format!("no calibration experiment '{}'", id.as_str()))
        })?,
        None => load_latest_experiment(conn)?.ok_or_else(|| {
            Error::InsufficientEvidence("no calibration experiment found in ledger".into())
        })?,
    };

    let stored_obs = load_experiment_observations(conn, &experiment)?;
    if stored_obs.is_empty() {
        return Err(Error::InsufficientEvidence(format!(
            "no meter observations found for experiment '{}'",
            experiment.id.as_str()
        )));
    }

    let cost_model = load_active_cost_model_at(conn, experiment.validity.valid_from())?
        .ok_or_else(|| {
            Error::InsufficientEvidence(format!(
                "no active cost model found for experiment '{}'",
                experiment.id.as_str()
            ))
        })?;

    let usage_events = load_experiment_usage(
        conn,
        experiment.validity.valid_from(),
        experiment.validity.valid_until(),
    )?;

    // Aggregate tokens per usage event
    let mut event_tokens: BTreeMap<String, (UtcTimestamp, KnownTokenVector)> = BTreeMap::new();
    for event in usage_events {
        let entry = event_tokens
            .entry(event.canonical_event_id)
            .or_insert_with(|| {
                (
                    event.timestamp,
                    KnownTokenVector::new(
                        InputTokens::new(0),
                        OutputTokens::new(0),
                        CacheReadTokens::new(0),
                        CacheWriteTokens::new(0),
                    ),
                )
            });

        let current = entry.1;
        let kind = match event.token_class.as_str() {
            "input" => TokenKind::Input,
            "output" => TokenKind::Output,
            "cache_read" => TokenKind::CacheRead,
            "cache_write" => TokenKind::CacheWrite,
            other => {
                return Err(Error::InsufficientEvidence(format!(
                    "unknown token class '{other}' in usage event"
                )));
            }
        };

        let new_known = match kind {
            TokenKind::Input => KnownTokenVector::new(
                InputTokens::new(current.input().value() + event.count),
                current.output(),
                current.cache_read(),
                current.cache_write(),
            ),
            TokenKind::Output => KnownTokenVector::new(
                current.input(),
                OutputTokens::new(current.output().value() + event.count),
                current.cache_read(),
                current.cache_write(),
            ),
            TokenKind::CacheRead => KnownTokenVector::new(
                current.input(),
                current.output(),
                CacheReadTokens::new(current.cache_read().value() + event.count),
                current.cache_write(),
            ),
            TokenKind::CacheWrite => KnownTokenVector::new(
                current.input(),
                current.output(),
                current.cache_read(),
                CacheWriteTokens::new(current.cache_write().value() + event.count),
            ),
        };
        entry.1 = new_known;
    }

    // Convert each usage event to Credits via the active CostModel
    let mut event_credits: Vec<(UtcTimestamp, Credits)> = Vec::new();
    for (_event_id, (ts, known)) in event_tokens {
        let usage = UsageVector::new(
            known,
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        );
        match convert_usage(&cost_model, &usage) {
            Derivation::Available(qualified) => {
                let (credits, _, _, _) = qualified.into_parts();
                event_credits.push((ts, credits));
            }
            Derivation::Unavailable { missing, .. } => {
                let facts = missing
                    .into_iter()
                    .map(|f| format!("{f:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(Error::InsufficientEvidence(format!(
                    "incomplete cost model: missing facts [{facts}]"
                )));
            }
        }
    }

    event_credits.sort_by_key(|(ts, _)| *ts);

    // Build FitObservations with cumulative credits up to observation timestamp
    let mut fit_observations = Vec::new();
    for obs in stored_obs {
        let cumulative_micros: i64 = event_credits
            .iter()
            .filter(|(ts, _)| *ts <= obs.at)
            .map(|(_, credits)| credits.micros())
            .sum();

        fit_observations.push(FitObservation::new(
            obs.evidence_id,
            obs.at,
            obs.quota_used_ppm,
            obs.reported_resolution_ppm,
            obs.quantization,
            Credits::from_micros(cumulative_micros),
        ));
    }

    // Execute fit
    let mut result = fit(&fit_observations, &experiment).map_err(|rej| rej.into_error())?;

    // Update knowledge time with the clock
    result.candidate.knowledge_time = clock.now();

    // Persist candidate immutably if not already recorded
    if load_candidate(conn, &result.candidate.id)?.is_none() {
        insert_candidate(conn, &result.candidate)?;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::settlement::{SettlementCriterion, SettlementPolicy};
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::domain::window::{ReportedResolution, WindowSemanticKey};
    use crate::store::calibration::{PlanTier, load_candidate};
    use crate::store::cost_model::{ProviderKey, ValidityInterval};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-calibration-fitter-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_experiment(id: &str) -> crate::store::calibration::CalibrationExperiment {
        let res = ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap();
        let criterion = SettlementCriterion::new(
            MonotonicDuration::from_nanos(300_000_000_000),
            3,
            MonotonicDuration::from_nanos(600_000_000_000),
            0,
            MonotonicDuration::from_nanos(3_600_000_000_000),
            res,
        )
        .unwrap();
        let policy = SettlementPolicy::new(
            "test-policy-v1",
            criterion,
            criterion,
            Some("shared for test".into()),
        )
        .unwrap();

        crate::store::calibration::CalibrationExperiment {
            id: ExperimentId::new(id),
            provider: ProviderKey::new("test-provider"),
            plan_tier: PlanTier::new("default"),
            window_semantic_key: WindowSemanticKey::new("seven_day"),
            meter_semantics_id: crate::domain::ids::MeterSemanticsId::new("semantics-v1"),
            billing_semantics_id: crate::domain::ids::BillingSemanticsId::new("billing-v1"),
            settlement_policy: policy,
            validity: ValidityInterval::new(
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                UtcTimestamp::from_unix_nanos(10_000_000_000),
            )
            .unwrap(),
            knowledge_time: UtcTimestamp::from_unix_nanos(10_000_000_000),
        }
    }

    /// Criterion 1: Interval-based fitting compared against scalar-based fitting on the
    /// same quantized series, asserting the two differ and that the scalar version manufactures drift.
    #[test]
    fn criterion_1_interval_vs_scalar_fitting_manufactures_drift() {
        let experiment = test_experiment("exp-crit-1");

        // True underlying usage moves from 40.6% to 41.4% (slope = 100 ppm/credit)
        // Provider resolution is 1% (10,000 ppm), round to nearest.
        // All readings report exactly 410,000 ppm (41%).
        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-1"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                410_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-2"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                410_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(2_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-3"),
                UtcTimestamp::from_unix_nanos(3_000_000_000),
                410_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(5_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-4"),
                UtcTimestamp::from_unix_nanos(4_000_000_000),
                420_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(10_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-5"),
                UtcTimestamp::from_unix_nanos(5_000_000_000),
                420_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(14_000_000),
            ),
        ];

        let interval_result = fit(&observations, &experiment).expect("interval fit succeeds");
        let (_scalar_slope, scalar_residual) =
            fit_scalar_for_comparison(&observations, &experiment).expect("scalar fit succeeds");

        // The interval fit residual should be strictly zero or near zero because the trajectory
        // stays entirely within the admissible intervals [405_000, 415_000] and [415_000, 425_000].
        assert_eq!(
            interval_result.residual_percentage_points, 0.0,
            "interval fit must have 0 residual for trajectory within admissible intervals"
        );

        // The scalar fit penalizes deviations from the rounded points (410_000 and 420_000),
        // manufacturing non-zero residual/drift.
        assert!(
            scalar_residual > 0.0,
            "scalar fit must manufacture non-zero residual drift from rounding"
        );
        assert!(
            (interval_result.residual_percentage_points * 10_000.0) < scalar_residual,
            "interval residual must be strictly less than scalar residual"
        );

        // Verify readings enter the fit as admissible intervals derived from semantics
        let inv_nearest = observations[0].interval();
        assert_eq!(inv_nearest.lower_ppm(), 405_000);
        assert_eq!(inv_nearest.upper_ppm(), 415_000);

        let inv_down =
            AdmissibleInterval::from_reading(410_000, 10_000, QuantizationSemantics::RoundedDown);
        assert_eq!(inv_down.lower_ppm(), 410_000);
        assert_eq!(inv_down.upper_ppm(), 420_000);

        let inv_up =
            AdmissibleInterval::from_reading(410_000, 10_000, QuantizationSemantics::RoundedUp);
        assert_eq!(inv_up.lower_ppm(), 400_000);
        assert_eq!(inv_up.upper_ppm(), 410_000);
    }

    /// Criterion 2: The regression is robust to quantization and batched updates,
    /// and records method and parameters.
    #[test]
    fn criterion_2_records_robust_method_and_parameters() {
        let experiment = test_experiment("exp-crit-2");

        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-1"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-2"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(1_000_000),
            ),
            // Batched jump: provider held back updates and dumped them in one reading
            FitObservation::new(
                EvidenceId::new("ev-3"),
                UtcTimestamp::from_unix_nanos(3_000_000_000),
                130_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(3_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-4"),
                UtcTimestamp::from_unix_nanos(4_000_000_000),
                140_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(4_000_000),
            ),
        ];

        let result = fit(&observations, &experiment).expect("fit succeeds");

        assert_eq!(result.statistical_method, "theil-sen-huber-interval");
        assert!(
            result.statistical_parameters.contains("huber_delta_ppm="),
            "parameters must record huber delta"
        );
        assert!(
            result.statistical_parameters.contains("init=theil_sen"),
            "parameters must record theil_sen init"
        );
        assert!(
            result
                .statistical_parameters
                .contains("quantization=admissible_interval"),
            "parameters must record interval quantization"
        );
        assert!(
            result.candidate.fitted.micros_per_point() > 0,
            "fitted coefficient must be positive"
        );
    }

    /// Criterion 3: The result reports coefficient, equivalent capacity, residual in pp,
    /// lag handling, uncertainty interval, usable count, and exclusions with reason.
    #[test]
    fn criterion_3_all_output_fields_present_including_exclusions() {
        let experiment = test_experiment("exp-crit-3");

        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-1"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            // Duplicate timestamp: should be excluded
            FitObservation::new(
                EvidenceId::new("ev-dup"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-2"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                110_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(1_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-3"),
                UtcTimestamp::from_unix_nanos(3_000_000_000),
                120_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(2_000_000),
            ),
        ];

        let result = fit(&observations, &experiment).expect("fit succeeds");

        // 1. coefficient
        assert!(result.candidate.fitted.micros_per_point() > 0);
        // 2. equivalent full-window capacity
        assert!(result.candidate.equivalent_full_window_capacity.micros() > 0);
        // 3. residual in percentage points
        assert!(result.residual_percentage_points >= 0.0);
        // 4. lag handling
        assert_eq!(
            result.lag_handling.as_str(),
            "settled-boundary-cancellation"
        );
        // 5. uncertainty interval
        assert!(
            result.candidate.uncertainty.lower().micros_per_point()
                <= result.candidate.uncertainty.upper().micros_per_point()
        );
        // 6. usable observation count
        assert_eq!(result.usable_observations, 3);
        assert_eq!(result.candidate.sample_count, 3);
        // 7. excluded samples with reason
        assert_eq!(result.excluded_samples.len(), 1);
        assert_eq!(result.excluded_samples[0].sample_ref(), "ev-dup");
        assert!(
            result.excluded_samples[0]
                .reason()
                .contains("duplicate timestamp")
        );
    }

    /// Criterion 4: Intercept diagnostic names the three causes on a contaminated series.
    #[test]
    fn criterion_4_intercept_diagnostic_names_three_causes_on_contaminated_series() {
        let experiment = test_experiment("exp-crit-4");

        // Contaminated series: background usage causes a 25,000 ppm jump before workload takes over
        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-1"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-2"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                135_000, // Jump of 35_000 ppm for 1 credit (expected 10_000, so +25_000 offset)
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(1_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-3"),
                UtcTimestamp::from_unix_nanos(3_000_000_000),
                145_000, // 10_000 ppm/credit slope resumes
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(2_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-4"),
                UtcTimestamp::from_unix_nanos(4_000_000_000),
                155_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(3_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-5"),
                UtcTimestamp::from_unix_nanos(5_000_000_000),
                165_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(4_000_000),
            ),
        ];

        let result = fit(&observations, &experiment).expect("fit succeeds");

        assert!(
            !result.diagnostic_findings.is_empty(),
            "intercept diagnostic must fire on contaminated series"
        );

        let finding = &result.diagnostic_findings[0];
        let causes = finding.possible_causes();

        assert_eq!(causes, &INTERCEPT_POSSIBLE_CAUSES);
        assert!(causes.contains(&"contamination"));
        assert!(causes.contains(&"lag mismatch"));
        assert!(causes.contains(&"incomplete cost model"));

        let msg = finding.message();
        assert!(msg.contains("contamination"));
        assert!(msg.contains("lag mismatch"));
        assert!(msg.contains("incomplete cost model"));
    }

    /// Criterion 5: Published coefficient is converted to typed fixed-point representation.
    #[test]
    fn criterion_5_published_coefficient_converted_to_typed_fixed_point() {
        let experiment = test_experiment("exp-crit-5");

        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-1"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-2"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                110_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(1_000_000),
            ),
        ];

        let result = fit(&observations, &experiment).expect("fit succeeds");

        // The candidate stores typed types: CreditsPerPercentagePoint, Credits, CoefficientUncertainty
        let fitted: CreditsPerPercentagePoint = result.candidate.fitted;
        let capacity: Credits = result.candidate.equivalent_full_window_capacity;
        let residual: Credits = result.candidate.fit_residual;
        let uncertainty: CoefficientUncertainty = result.candidate.uncertainty;

        assert_eq!(fitted.micros_per_point(), 100);
        assert_eq!(capacity.micros(), 100_000_000);
        assert_eq!(residual.micros(), 0);
        assert!(uncertainty.lower().micros_per_point() <= uncertainty.upper().micros_per_point());
    }

    /// Criterion 6: Fit writes an immutable candidate and never activates it.
    #[test]
    fn criterion_6_fit_writes_candidate_and_never_activates() {
        let scratch = ScratchDir::new();
        let policy = crate::store::connection::PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = crate::store::connection::open(
            &scratch.path().join("calibration.db"),
            crate::store::connection::AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        let clock = FakeClock::new(UtcTimestamp::from_unix_nanos(10_000_000_000));
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &clock,
        )
        .unwrap();

        let experiment = test_experiment("exp-crit-6");
        crate::store::calibration::insert_experiment(&conn, &experiment).unwrap();

        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-1"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                100_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-2"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                110_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(1_000_000),
            ),
        ];

        let mut fit_result = fit(&observations, &experiment).unwrap();
        fit_result.candidate.knowledge_time = clock.now();

        // Write candidate to database
        insert_candidate(&conn, &fit_result.candidate).expect("insert candidate succeeds");

        // Verify candidate exists
        let loaded = load_candidate(&conn, &fit_result.candidate.id)
            .expect("query succeeds")
            .expect("candidate exists");
        assert_eq!(loaded.id, fit_result.candidate.id);

        // Verify immutability: updates and deletes are rejected by SQLite triggers
        let update_err =
            crate::store::calibration::try_update_candidate(&conn, &fit_result.candidate.id)
                .unwrap_err();
        assert!(
            update_err.to_string().contains("immutable"),
            "candidate table must reject update via trigger, got: {update_err}"
        );

        let delete_err =
            crate::store::calibration::try_delete_candidate(&conn, &fit_result.candidate.id)
                .unwrap_err();
        assert!(
            delete_err.to_string().contains("immutable"),
            "candidate table must reject delete via trigger, got: {delete_err}"
        );

        // Verify no activation occurred (no lifecycle row)
        let lifecycle_count =
            crate::store::calibration::count_calibration_lifecycles(&conn).unwrap();
        assert_eq!(
            lifecycle_count, 0,
            "fitter must never insert into calibration_lifecycle"
        );
    }

    /// Criterion 7: Rerunning the fitter on the same evidence produces the same inputs_hash
    /// and the same coefficient.
    #[test]
    fn criterion_7_fit_reproducibility_same_evidence_same_hash_and_coefficient() {
        let experiment = test_experiment("exp-crit-7");

        let observations = vec![
            FitObservation::new(
                EvidenceId::new("ev-a"),
                UtcTimestamp::from_unix_nanos(1_000_000_000),
                200_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(0),
            ),
            FitObservation::new(
                EvidenceId::new("ev-b"),
                UtcTimestamp::from_unix_nanos(2_000_000_000),
                210_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(1_000_000),
            ),
            FitObservation::new(
                EvidenceId::new("ev-c"),
                UtcTimestamp::from_unix_nanos(3_000_000_000),
                225_000,
                10_000,
                QuantizationSemantics::RoundedToNearest,
                Credits::from_micros(2_500_000),
            ),
        ];

        let run_1 = fit(&observations, &experiment).unwrap();
        let run_2 = fit(&observations, &experiment).unwrap();

        assert_eq!(
            run_1.candidate.inputs.digest(),
            run_2.candidate.inputs.digest(),
            "inputs_hash must be identical across runs on same evidence"
        );
        assert_eq!(
            run_1.candidate.fitted.micros_per_point(),
            run_2.candidate.fitted.micros_per_point(),
            "coefficient must be identical across runs on same evidence"
        );
        assert_eq!(
            run_1.candidate.equivalent_full_window_capacity.micros(),
            run_2.candidate.equivalent_full_window_capacity.micros()
        );
        assert_eq!(run_1.candidate.uncertainty, run_2.candidate.uncertainty);
        assert_eq!(
            run_1.residual_percentage_points,
            run_2.residual_percentage_points
        );
    }
}
