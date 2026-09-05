//! Multivariate calibration fitting with identifiability gating.
//!
//! A joint fit across token kinds is only meaningful when deliberately varied
//! workloads make the individual coefficients identifiable (PLAN.md 22.1). Input
//! and cache usage move together in ordinary traffic, so a regression over
//! passive data will happily print one coefficient per kind with several decimal
//! places while no amount of that data could separate them. This module fits the
//! joint model and rejects it unless the design earns it: the condition number
//! of the token-count design is the gate, and a rejection names the pair it
//! could not separate plus their correlation, which is the unavailable-fact
//! statement PLAN.md 45 asks for instead of an unstable estimate.
//!
//! May not depend on:
//! - transcripts (calibration never parses transcripts)
//! - presentation
//!
//! The fit runs through the origin: quota movement at zero usage is zero by
//! construction, and an intercept would absorb quantization offset while hiding
//! the kind-level collinearity the gate must see. Intercept diagnostics stay
//! with the univariate fitter, which answers a different question.

// Index-based loops over symmetric matrices and augmented systems read as the
// algebra they implement (Jacobi sweeps, Gauss-Jordan pivots); iterator forms of
// the same nested index arithmetic obscure which element is being rotated.
#![allow(clippy::needless_range_loop)]

use super::fitter::{EntangledCoefficientPair, FitRejection};
use crate::domain::provenance::EvidenceId;
use crate::domain::tokens::{KnownTokenVector, TokenKind};
use crate::store::calibration::ExcludedSample;

/// The normal-approximation multiplier for two-sided 95 percent intervals.
///
/// One famous constant rather than a t-table: with the small samples this gate
/// sees, a t critical value would be wider and more honest, but every entry of
/// such a table is a transcribed fact that can be mistyped, while 1.96 cannot.
/// The approximation is stated in the parameters string of every result so a
/// reader knows what the interval claims.
const NORMAL_975_QUANTILE: f64 = 1.96;

/// Exclusion reason for rows carrying no usage in any fitted kind. A row of
/// zeros constrains no coefficient, so it leaves the fit with its reason
/// recorded rather than silently padding the sample count.
const ZERO_USAGE_EXCLUSION_REASON: &str = "excluded: no usage in fitted token kinds";

/// What went wrong constructing a [`MultivariateFitConfig`].
#[derive(Debug, Clone, PartialEq)]
pub enum MultivariateFitConfigError {
    ThresholdMustExceedOne { got: f64 },
    MinimumMustBePositive,
    RidgeMustBeFiniteNonNegative { got: f64 },
}

impl std::fmt::Display for MultivariateFitConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThresholdMustExceedOne { got } => write!(
                f,
                "multivariate fit condition-number threshold must exceed 1, got {got}"
            ),
            Self::MinimumMustBePositive => {
                write!(
                    f,
                    "multivariate fit minimum observations must be at least 1"
                )
            }
            Self::RidgeMustBeFiniteNonNegative { got } => write!(
                f,
                "multivariate fit ridge penalty must be finite and non-negative, got {got}"
            ),
        }
    }
}

impl std::error::Error for MultivariateFitConfigError {}

/// The configured gate for multivariate fitting.
///
/// Every field is caller-supplied and echoed back on the result, so the
/// threshold that rejected a fit is readable from the fit itself rather than
/// from a source constant. The fit function takes this struct and never names
/// any of the defaults below.
#[derive(Debug, Clone, PartialEq)]
pub struct MultivariateFitConfig {
    condition_number_threshold: f64,
    minimum_observations: usize,
    ridge_penalty: f64,
    enforce_non_negativity: bool,
}

impl MultivariateFitConfig {
    /// Belsley, Kuh and Welsch (1980): values above 30 mark strong
    /// collinearity. A joint token-kind experiment that cannot beat 30 has no
    /// business publishing per-kind coefficients.
    pub const DEFAULT_CONDITION_NUMBER_THRESHOLD: f64 = 30.0;
    /// Four token kinds exist, so eight usable observations leave at least four
    /// residual degrees of freedom for standard errors when fitting all of
    /// them. A caller fitting fewer kinds passes its own smaller minimum.
    pub const DEFAULT_MINIMUM_OBSERVATIONS: usize = 8;
    /// No regularization unless configured: an unregularized rejection is a
    /// statement about the experiment, not about the penalty.
    pub const DEFAULT_RIDGE_PENALTY: f64 = 0.0;

    pub fn new(
        condition_number_threshold: f64,
        minimum_observations: usize,
        ridge_penalty: f64,
        enforce_non_negativity: bool,
    ) -> Result<Self, MultivariateFitConfigError> {
        if !condition_number_threshold.is_finite() || condition_number_threshold <= 1.0 {
            return Err(MultivariateFitConfigError::ThresholdMustExceedOne {
                got: condition_number_threshold,
            });
        }
        if minimum_observations == 0 {
            return Err(MultivariateFitConfigError::MinimumMustBePositive);
        }
        if !ridge_penalty.is_finite() || ridge_penalty < 0.0 {
            return Err(MultivariateFitConfigError::RidgeMustBeFiniteNonNegative {
                got: ridge_penalty,
            });
        }
        Ok(Self {
            condition_number_threshold,
            minimum_observations,
            ridge_penalty,
            enforce_non_negativity,
        })
    }

    pub fn condition_number_threshold(&self) -> f64 {
        self.condition_number_threshold
    }

    pub fn minimum_observations(&self) -> usize {
        self.minimum_observations
    }

    pub fn ridge_penalty(&self) -> f64 {
        self.ridge_penalty
    }

    pub fn enforce_non_negativity(&self) -> bool {
        self.enforce_non_negativity
    }
}

/// What went wrong constructing a [`MultivariateFitObservation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultivariateObservationError {
    NonFiniteQuotaDelta { evidence: String, got: String },
}

impl std::fmt::Display for MultivariateObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFiniteQuotaDelta { evidence, got } => write!(
                f,
                "multivariate fit observation '{evidence}' has a non-finite quota delta: {got}"
            ),
        }
    }
}

impl std::error::Error for MultivariateObservationError {}

/// One observation entering a multivariate fit: the token usage behind a quota
/// movement and the movement itself in parts per million.
#[derive(Debug, Clone)]
pub struct MultivariateFitObservation {
    evidence_id: EvidenceId,
    tokens: KnownTokenVector,
    quota_delta_ppm: f64,
}

impl MultivariateFitObservation {
    pub fn new(
        evidence_id: EvidenceId,
        tokens: KnownTokenVector,
        quota_delta_ppm: f64,
    ) -> Result<Self, MultivariateObservationError> {
        if !quota_delta_ppm.is_finite() {
            return Err(MultivariateObservationError::NonFiniteQuotaDelta {
                evidence: evidence_id.as_str().to_string(),
                got: format!("{quota_delta_ppm}"),
            });
        }
        Ok(Self {
            evidence_id,
            tokens,
            quota_delta_ppm,
        })
    }

    pub fn evidence_id(&self) -> &EvidenceId {
        &self.evidence_id
    }

    pub fn tokens(&self) -> KnownTokenVector {
        self.tokens
    }

    pub fn quota_delta_ppm(&self) -> f64 {
        self.quota_delta_ppm
    }
}

/// One fitted token-kind coefficient with its uncertainty, all in quota parts
/// per million per token of that kind.
#[derive(Debug, Clone, PartialEq)]
pub struct FittedTokenKindCoefficient {
    kind: TokenKind,
    estimate_ppm_per_token: f64,
    std_error_ppm_per_token: f64,
    interval_low_ppm_per_token: f64,
    interval_high_ppm_per_token: f64,
}

impl FittedTokenKindCoefficient {
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    pub fn estimate_ppm_per_token(&self) -> f64 {
        self.estimate_ppm_per_token
    }

    pub fn std_error_ppm_per_token(&self) -> f64 {
        self.std_error_ppm_per_token
    }

    pub fn interval_low_ppm_per_token(&self) -> f64 {
        self.interval_low_ppm_per_token
    }

    pub fn interval_high_ppm_per_token(&self) -> f64 {
        self.interval_high_ppm_per_token
    }
}

/// The Pearson correlation between two fitted kinds over the usable
/// observations. Recorded on every accepted fit, so a barely-passing design
/// still shows how close it came to the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenKindCorrelation {
    first: TokenKind,
    second: TokenKind,
    correlation: f64,
}

impl TokenKindCorrelation {
    pub fn first(&self) -> TokenKind {
        self.first
    }

    pub fn second(&self) -> TokenKind {
        self.second
    }

    pub fn correlation(&self) -> f64 {
        self.correlation
    }
}

/// The complete result of an accepted multivariate fit.
#[derive(Debug, Clone, PartialEq)]
pub struct MultivariateFitResult {
    coefficients: Vec<FittedTokenKindCoefficient>,
    pairwise_correlations: Vec<TokenKindCorrelation>,
    condition_number: f64,
    condition_number_threshold: f64,
    minimum_observations: usize,
    ridge_penalty: f64,
    regularization: String,
    non_negativity: String,
    statistical_method: String,
    statistical_parameters: String,
    phase_design: String,
    usable_observations: u32,
    excluded_samples: Vec<ExcludedSample>,
}

impl MultivariateFitResult {
    pub fn coefficients(&self) -> &[FittedTokenKindCoefficient] {
        &self.coefficients
    }

    pub fn pairwise_correlations(&self) -> &[TokenKindCorrelation] {
        &self.pairwise_correlations
    }

    pub fn condition_number(&self) -> f64 {
        self.condition_number
    }

    pub fn condition_number_threshold(&self) -> f64 {
        self.condition_number_threshold
    }

    pub fn minimum_observations(&self) -> usize {
        self.minimum_observations
    }

    pub fn ridge_penalty(&self) -> f64 {
        self.ridge_penalty
    }

    pub fn regularization(&self) -> &str {
        &self.regularization
    }

    pub fn non_negativity(&self) -> &str {
        &self.non_negativity
    }

    pub fn statistical_method(&self) -> &str {
        &self.statistical_method
    }

    pub fn statistical_parameters(&self) -> &str {
        &self.statistical_parameters
    }

    pub fn phase_design(&self) -> &str {
        &self.phase_design
    }

    pub fn usable_observations(&self) -> u32 {
        self.usable_observations
    }

    pub fn excluded_samples(&self) -> &[ExcludedSample] {
        &self.excluded_samples
    }
}

/// Fits quota movement as a joint linear function of per-kind token counts.
///
/// The gate order is cheapest-first: sample count, then identifiability
/// (condition number against the configured threshold), then sign. A fit that
/// passes all three records its coefficients with standard errors and
/// intervals, the regularization actually applied, the non-negativity outcome,
/// and the phase design it was fitted from.
pub fn fit_multivariate(
    observations: &[MultivariateFitObservation],
    kinds: &[TokenKind],
    config: &MultivariateFitConfig,
    phase_design: &str,
) -> Result<MultivariateFitResult, FitRejection> {
    let mut selected: Vec<TokenKind> = Vec::new();
    for kind in kinds {
        if !selected.contains(kind) {
            selected.push(*kind);
        }
    }

    let mut usable: Vec<&MultivariateFitObservation> = Vec::new();
    let mut excluded_samples = Vec::new();
    for observation in observations {
        let carries_signal = selected
            .iter()
            .any(|kind| observation.tokens.value(*kind) > 0);
        if carries_signal {
            usable.push(observation);
        } else if let Ok(excluded) = ExcludedSample::new(
            observation.evidence_id.as_str(),
            ZERO_USAGE_EXCLUSION_REASON,
        ) {
            excluded_samples.push(excluded);
        }
    }

    if usable.len() < config.minimum_observations() {
        return Err(FitRejection::InsufficientObservations {
            found: usable.len(),
            required: config.minimum_observations(),
        });
    }

    // Standard errors need residual degrees of freedom: n must exceed p, and
    // an empty kind list identifies nothing at all.
    if selected.is_empty() || usable.len() <= selected.len() {
        return Err(FitRejection::Underidentified {
            usable: usable.len(),
        });
    }

    let columns: Vec<Vec<f64>> = selected
        .iter()
        .map(|kind| {
            usable
                .iter()
                .map(|observation| observation.tokens.value(*kind) as f64)
                .collect()
        })
        .collect();
    let response: Vec<f64> = usable
        .iter()
        .map(|observation| observation.quota_delta_ppm)
        .collect();

    let mut entangled: Vec<EntangledCoefficientPair> = Vec::new();
    for (i, first) in selected.iter().enumerate() {
        for (j, second) in selected.iter().enumerate().skip(i + 1) {
            if let Some(correlation) = pearson_correlation(&columns[i], &columns[j]) {
                entangled.push(EntangledCoefficientPair::new(*first, *second, correlation));
            }
        }
    }
    entangled.sort_by(|a, b| b.correlation().abs().total_cmp(&a.correlation().abs()));

    let mut gram: Vec<Vec<f64>> = (0..selected.len())
        .map(|i| {
            (0..selected.len())
                .map(|j| {
                    columns[i]
                        .iter()
                        .zip(columns[j].iter())
                        .map(|(x, y)| x * y)
                        .sum()
                })
                .collect()
        })
        .collect();
    if config.ridge_penalty() > 0.0 {
        for (i, row) in gram.iter_mut().enumerate() {
            row[i] += config.ridge_penalty();
        }
    }

    let eigenvalues = symmetric_eigenvalues(gram.clone());
    let largest = eigenvalues.iter().cloned().fold(0.0_f64, f64::max);
    let smallest = eigenvalues.iter().cloned().fold(f64::INFINITY, f64::min);
    let condition_number = if smallest <= 0.0 || !smallest.is_finite() {
        f64::INFINITY
    } else {
        (largest / smallest).sqrt()
    };

    if condition_number > config.condition_number_threshold() {
        return Err(FitRejection::IllConditioned {
            condition_number,
            threshold: config.condition_number_threshold(),
            entangled,
        });
    }

    // Unreachable in exact arithmetic: the gate above established a
    // positive-definite gram, which always inverts. If float pathology lands
    // here anyway, Underidentified is the closest true statement, since no
    // usable solution exists.
    let inverse = invert_matrix(&gram).ok_or(FitRejection::Underidentified {
        usable: usable.len(),
    })?;

    let projected: Vec<f64> = (0..selected.len())
        .map(|i| {
            columns[i]
                .iter()
                .zip(response.iter())
                .map(|(x, y)| x * y)
                .sum()
        })
        .collect();
    let estimates: Vec<f64> = inverse
        .iter()
        .map(|row| row.iter().zip(projected.iter()).map(|(a, b)| a * b).sum())
        .collect();

    for (kind, estimate) in selected.iter().zip(estimates.iter()) {
        if *estimate <= 0.0 {
            return Err(FitRejection::NonPositiveCoefficient {
                kind: *kind,
                estimate_ppm_per_token: *estimate,
            });
        }
    }

    let predictions: Vec<f64> = (0..usable.len())
        .map(|row| {
            estimates
                .iter()
                .enumerate()
                .map(|(i, beta)| beta * columns[i][row])
                .sum()
        })
        .collect();
    let residual_sum_squares: f64 = predictions
        .iter()
        .zip(response.iter())
        .map(|(predicted, actual)| (predicted - actual).powi(2))
        .sum();
    let degrees_of_freedom = (usable.len() - selected.len()) as f64;
    let residual_variance = residual_sum_squares / degrees_of_freedom;

    let coefficients: Vec<FittedTokenKindCoefficient> = selected
        .iter()
        .zip(estimates.iter())
        .enumerate()
        .map(|(i, (kind, estimate))| {
            let variance = (residual_variance * inverse[i][i]).max(0.0);
            let std_error = variance.sqrt();
            FittedTokenKindCoefficient {
                kind: *kind,
                estimate_ppm_per_token: *estimate,
                std_error_ppm_per_token: std_error,
                interval_low_ppm_per_token: *estimate - NORMAL_975_QUANTILE * std_error,
                interval_high_ppm_per_token: *estimate + NORMAL_975_QUANTILE * std_error,
            }
        })
        .collect();

    let pairwise_correlations: Vec<TokenKindCorrelation> = entangled
        .iter()
        .map(|pair| TokenKindCorrelation {
            first: pair.first(),
            second: pair.second(),
            correlation: pair.correlation(),
        })
        .collect();

    let regularization = if config.ridge_penalty() > 0.0 {
        format!(
            "ridge penalty {} added to gram diagonal",
            config.ridge_penalty()
        )
    } else {
        "none".to_string()
    };
    let non_negativity = if config.enforce_non_negativity() {
        format!(
            "enforced: all {} coefficients constrained to >= 0 ppm/token; unconstrained solution satisfied every bound",
            selected.len()
        )
    } else {
        format!(
            "not enforced by configuration; all {} fitted coefficients are positive",
            selected.len()
        )
    };
    let statistical_method = if config.ridge_penalty() > 0.0 {
        "multivariate-ridge-through-origin".to_string()
    } else {
        "multivariate-ols-through-origin".to_string()
    };
    let kind_labels: Vec<&str> = selected.iter().map(|kind| kind.label()).collect();
    let statistical_parameters = format!(
        "kinds=[{}];n={};p={};dof={};residual_variance_ppm2={:.3};ridge_penalty={};intervals=normal-approx-1.96",
        kind_labels.join(","),
        usable.len(),
        selected.len(),
        degrees_of_freedom as usize,
        residual_variance,
        config.ridge_penalty(),
    );

    Ok(MultivariateFitResult {
        coefficients,
        pairwise_correlations,
        condition_number,
        condition_number_threshold: config.condition_number_threshold(),
        minimum_observations: config.minimum_observations(),
        ridge_penalty: config.ridge_penalty(),
        regularization,
        non_negativity,
        statistical_method,
        statistical_parameters,
        phase_design: phase_design.to_string(),
        usable_observations: usable.len() as u32,
        excluded_samples,
    })
}

/// Pearson correlation, or None when either series never moves. A kind with no
/// variation cannot be entangled with anything; it is singular on its own, and
/// the condition gate says that instead.
fn pearson_correlation(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let count = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / count;
    let mean_y = ys.iter().sum::<f64>() / count;
    let mut cross = 0.0;
    let mut sum_xx = 0.0;
    let mut sum_yy = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = x - mean_x;
        let dy = y - mean_y;
        cross += dx * dy;
        sum_xx += dx * dx;
        sum_yy += dy * dy;
    }
    if sum_xx <= 0.0 || sum_yy <= 0.0 {
        return None;
    }
    Some((cross / (sum_xx * sum_yy).sqrt()).clamp(-1.0, 1.0))
}

/// Eigenvalues of a symmetric matrix via cyclic Jacobi rotations. Exact for 1x1
/// and converging for the small gram matrices this fitter builds; the caller
/// only needs the extremes for the condition number, not the vectors.
fn symmetric_eigenvalues(mut matrix: Vec<Vec<f64>>) -> Vec<f64> {
    let size = matrix.len();
    if size == 0 {
        return Vec::new();
    }
    for _ in 0..50 {
        let off_diagonal: f64 = matrix
            .iter()
            .enumerate()
            .map(|(i, row)| row[i + 1..].iter().map(|v| v * v).sum::<f64>())
            .sum();
        if off_diagonal <= 1e-24 {
            break;
        }
        for i in 0..size {
            for j in (i + 1)..size {
                let element = matrix[i][j];
                if element.abs() < 1e-300 {
                    continue;
                }
                let theta = (matrix[j][j] - matrix[i][i]) / (2.0 * element);
                let tangent = if theta >= 0.0 {
                    1.0 / (theta + (1.0 + theta * theta).sqrt())
                } else {
                    1.0 / (theta - (1.0 + theta * theta).sqrt())
                };
                let cosine = 1.0 / (1.0 + tangent * tangent).sqrt();
                let sine = tangent * cosine;
                let diagonal_i = matrix[i][i];
                let diagonal_j = matrix[j][j];
                matrix[i][i] = cosine * cosine * diagonal_i - 2.0 * sine * cosine * element
                    + sine * sine * diagonal_j;
                matrix[j][j] = sine * sine * diagonal_i
                    + 2.0 * sine * cosine * element
                    + cosine * cosine * diagonal_j;
                matrix[i][j] = 0.0;
                matrix[j][i] = 0.0;
                for k in 0..size {
                    if k != i && k != j {
                        let ik = matrix[i][k];
                        let jk = matrix[j][k];
                        matrix[i][k] = cosine * ik - sine * jk;
                        matrix[k][i] = matrix[i][k];
                        matrix[j][k] = sine * ik + cosine * jk;
                        matrix[k][j] = matrix[j][k];
                    }
                }
            }
        }
    }
    (0..size).map(|i| matrix[i][i]).collect()
}

/// Matrix inverse via Gauss-Jordan elimination with partial pivoting, or None
/// when a pivot vanishes and the matrix is singular in floating point.
fn invert_matrix(matrix: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let size = matrix.len();
    if size == 0 {
        return None;
    }
    let mut augmented: Vec<Vec<f64>> = matrix
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut extended = row.clone();
            extended.extend((0..size).map(|j| if i == j { 1.0 } else { 0.0 }));
            extended
        })
        .collect();
    for column in 0..size {
        let mut pivot = column;
        for row in (column + 1)..size {
            if augmented[row][column].abs() > augmented[pivot][column].abs() {
                pivot = row;
            }
        }
        if augmented[pivot][column].abs() < 1e-300 {
            return None;
        }
        augmented.swap(column, pivot);
        let divisor = augmented[column][column];
        for j in 0..2 * size {
            augmented[column][j] /= divisor;
        }
        for row in 0..size {
            if row != column {
                let factor = augmented[row][column];
                if factor != 0.0 {
                    for j in 0..2 * size {
                        augmented[row][j] -= factor * augmented[column][j];
                    }
                }
            }
        }
    }
    Some(augmented.iter().map(|row| row[size..].to_vec()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::tokens::{CacheReadTokens, CacheWriteTokens, InputTokens, OutputTokens};
    use proptest::prelude::*;

    const INPUT_TRUTH_PPM_PER_TOKEN: f64 = 0.5;
    const CACHE_READ_TRUTH_PPM_PER_TOKEN: f64 = 0.1;

    fn tokens(input: u64, cache_read: u64) -> KnownTokenVector {
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(0),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(0),
        )
    }

    fn observation(
        id: &str,
        input: u64,
        cache_read: u64,
        delta_ppm: f64,
    ) -> MultivariateFitObservation {
        MultivariateFitObservation::new(EvidenceId::new(id), tokens(input, cache_read), delta_ppm)
            .expect("test observation must be valid")
    }

    fn exact_delta(input: u64, cache_read: u64) -> f64 {
        INPUT_TRUTH_PPM_PER_TOKEN * input as f64
            + CACHE_READ_TRUTH_PPM_PER_TOKEN * cache_read as f64
    }

    fn config(threshold: f64, minimum: usize) -> MultivariateFitConfig {
        MultivariateFitConfig::new(threshold, minimum, 0.0, true).expect("test config valid")
    }

    fn kinds() -> Vec<TokenKind> {
        vec![TokenKind::Input, TokenKind::CacheRead]
    }

    /// Six observations with cache usage in a fixed 4:1 proportion to input:
    /// no amount of this data separates the two coefficients.
    fn collinear_observations() -> Vec<MultivariateFitObservation> {
        (1..=6)
            .map(|step| {
                let input = 100 * step;
                let cache = 400 * step;
                observation(
                    &format!("ev-collinear-{step}"),
                    input,
                    cache,
                    exact_delta(input, cache),
                )
            })
            .collect()
    }

    /// The same truth observed through two deliberately varied phases: three
    /// input-heavy rows and three cache-heavy rows, with small deterministic
    /// noise so the reported intervals have nonzero width.
    fn varied_observations() -> Vec<MultivariateFitObservation> {
        let rows: [(u64, u64, f64); 6] = [
            (1000, 100, 3.0),
            (2000, 150, -2.0),
            (1500, 120, 1.0),
            (150, 1200, -1.0),
            (200, 2400, 2.0),
            (120, 1800, -3.0),
        ];
        rows.iter()
            .enumerate()
            .map(|(i, (input, cache, noise))| {
                observation(
                    &format!("ev-varied-{i}"),
                    *input,
                    *cache,
                    exact_delta(*input, *cache) + noise,
                )
            })
            .collect()
    }

    /// A synthetic experiment with perfectly collinear token kinds is rejected,
    /// naming which coefficients could not be separated and their correlation.
    #[test]
    fn collinear_token_kinds_rejected_naming_pair_and_correlation() {
        let observations = collinear_observations();
        let rejection = fit_multivariate(
            &observations,
            &kinds(),
            &config(30.0, 4),
            "single-phase-fixed-proportion",
        )
        .expect_err("collinear design must be rejected");
        let message = rejection.to_string();

        match rejection {
            FitRejection::IllConditioned {
                condition_number,
                threshold,
                entangled,
            } => {
                assert!(
                    condition_number > threshold,
                    "condition {condition_number} must exceed threshold {threshold}"
                );
                assert_eq!(threshold, 30.0);
                assert!(
                    !entangled.is_empty(),
                    "rejection must name the entangled pair"
                );
                let pair = &entangled[0];
                assert_eq!(pair.first(), TokenKind::Input);
                assert_eq!(pair.second(), TokenKind::CacheRead);
                assert!(
                    (pair.correlation() - 1.0).abs() < 1e-9,
                    "fixed proportions correlate at r=1, got r={}",
                    pair.correlation()
                );
            }
            other @ (FitRejection::InsufficientObservations { .. }
            | FitRejection::Underidentified { .. }
            | FitRejection::NonPositiveSlope { .. }
            | FitRejection::NonPositiveCoefficient { .. }
            | FitRejection::ZeroCreditSpan
            | FitRejection::BaselinePlateauNotSettled
            | FitRejection::TerminalPlateauNotSettled
            | FitRejection::MissingCostModelTerm { .. }
            | FitRejection::ContaminatedSeries { .. }) => {
                panic!("expected ill-conditioned rejection, got {other}")
            }
        }

        assert!(message.contains("input"), "message names input: {message}");
        assert!(
            message.contains("cache_read"),
            "message names cache_read: {message}"
        );
        assert!(
            message.contains("correlation"),
            "message reports the correlation: {message}"
        );
    }

    /// The same experiment with deliberately varied phases is accepted, and
    /// every recorded field is present.
    #[test]
    fn varied_phases_accepted_with_complete_fit_record() {
        let observations = varied_observations();
        let result = fit_multivariate(
            &observations,
            &kinds(),
            &config(30.0, 4),
            "varied-two-phase(input-heavy,cache-heavy)",
        )
        .expect("varied design must be accepted");

        assert!(
            result.condition_number() > 1.0 && result.condition_number() < 30.0,
            "varied design must sit comfortably inside the gate, got {}",
            result.condition_number()
        );
        assert_eq!(result.condition_number_threshold(), 30.0);
        assert_eq!(result.minimum_observations(), 4);
        assert_eq!(
            result.phase_design(),
            "varied-two-phase(input-heavy,cache-heavy)"
        );
        assert_eq!(result.usable_observations(), 6);
        assert!(result.excluded_samples().is_empty());
        assert_eq!(
            result.statistical_method(),
            "multivariate-ols-through-origin"
        );
        assert_eq!(result.regularization(), "none");
        assert!(
            result.non_negativity().contains("enforced"),
            "non-negativity outcome must be recorded: {}",
            result.non_negativity()
        );
        assert!(
            result
                .statistical_parameters()
                .contains("kinds=[input,cache_read]"),
            "parameters must name the fitted kinds: {}",
            result.statistical_parameters()
        );

        assert_eq!(result.coefficients().len(), 2);
        assert_eq!(result.coefficients()[0].kind(), TokenKind::Input);
        assert_eq!(result.coefficients()[1].kind(), TokenKind::CacheRead);
        for coefficient in result.coefficients() {
            let truth = match coefficient.kind() {
                TokenKind::Input => INPUT_TRUTH_PPM_PER_TOKEN,
                TokenKind::CacheRead => CACHE_READ_TRUTH_PPM_PER_TOKEN,
                TokenKind::Output => 0.3,
                TokenKind::CacheWrite => 0.05,
            };
            assert!(
                coefficient.std_error_ppm_per_token() > 0.0,
                "standard errors must be recorded and positive"
            );
            assert!(
                coefficient.interval_low_ppm_per_token()
                    < coefficient.interval_high_ppm_per_token(),
                "intervals must have nonzero width"
            );
            assert!(
                coefficient.interval_low_ppm_per_token() <= truth
                    && truth <= coefficient.interval_high_ppm_per_token(),
                "reported interval must recover the seeded coefficient: {:?} vs truth {truth}",
                coefficient
            );
        }

        assert_eq!(result.pairwise_correlations().len(), 1);
        assert!(
            result.pairwise_correlations()[0].correlation().abs() < 0.9,
            "varied phases must not be near-collinear, got r={}",
            result.pairwise_correlations()[0].correlation()
        );

        // A duplicated kind estimates one coefficient, not two.
        let deduplicated = fit_multivariate(
            &observations,
            &[TokenKind::Input, TokenKind::CacheRead, TokenKind::Input],
            &config(30.0, 4),
            "varied-two-phase(input-heavy,cache-heavy)",
        )
        .expect("duplicated kind must not change the fit");
        assert_eq!(deduplicated.coefficients().len(), 2);

        // Ridge regularization is recorded in the method, not silently applied.
        let ridge_config =
            MultivariateFitConfig::new(30.0, 4, 0.5, true).expect("ridge config valid");
        let ridge = fit_multivariate(
            &observations,
            &kinds(),
            &ridge_config,
            "varied-two-phase(input-heavy,cache-heavy)",
        )
        .expect("ridge fit of a varied design must be accepted");
        assert_eq!(
            ridge.statistical_method(),
            "multivariate-ridge-through-origin"
        );
        assert!(ridge.regularization().contains("ridge"));

        // Switching enforcement off is recorded rather than silently assumed.
        let unenforced_config =
            MultivariateFitConfig::new(30.0, 4, 0.0, false).expect("unenforced config valid");
        let unenforced = fit_multivariate(
            &observations,
            &kinds(),
            &unenforced_config,
            "varied-two-phase(input-heavy,cache-heavy)",
        )
        .expect("positive fit without enforcement must be accepted");
        assert!(unenforced.non_negativity().contains("not enforced"));
    }

    /// A fit with fewer usable observations than the configured minimum is
    /// rejected with that reason; rows with no usage in the fitted kinds do
    /// not count toward the minimum.
    #[test]
    fn fewer_usable_observations_than_minimum_rejected() {
        let mut observations = varied_observations()[..3].to_vec();
        observations.push(observation("ev-empty", 0, 0, 0.0));
        let rejection =
            fit_multivariate(&observations, &kinds(), &config(5.0, 5), "varied-two-phase")
                .expect_err("3 usable observations against a minimum of 5 must be rejected");

        match rejection {
            FitRejection::InsufficientObservations { found, required } => {
                assert_eq!(found, 3);
                assert_eq!(required, 5);
            }
            other @ (FitRejection::Underidentified { .. }
            | FitRejection::NonPositiveSlope { .. }
            | FitRejection::NonPositiveCoefficient { .. }
            | FitRejection::IllConditioned { .. }
            | FitRejection::ZeroCreditSpan
            | FitRejection::BaselinePlateauNotSettled
            | FitRejection::TerminalPlateauNotSettled
            | FitRejection::MissingCostModelTerm { .. }
            | FitRejection::ContaminatedSeries { .. }) => {
                panic!("expected insufficient-observations rejection, got {other}")
            }
        }
        let message = rejection.to_string();
        assert!(
            message.contains("found 3"),
            "message states the count: {message}"
        );
        assert!(
            message.contains('5'),
            "message states the minimum: {message}"
        );
    }

    /// A non-positive slope is rejected, since token usage cannot reduce the
    /// used fraction. Single-kind adversarial series: quota falls while usage
    /// climbs, so the unconstrained coefficient goes negative.
    #[test]
    fn non_positive_slope_rejected() {
        let observations = vec![
            observation("ev-down-1", 100, 0, 3000.0),
            observation("ev-down-2", 200, 0, 1000.0),
            observation("ev-down-3", 300, 0, -1000.0),
            observation("ev-down-4", 400, 0, -3000.0),
        ];
        let rejection = fit_multivariate(
            &observations,
            &[TokenKind::Input],
            &config(30.0, 2),
            "adversarial-falling-quota",
        )
        .expect_err("falling quota under rising usage must be rejected");

        match rejection {
            FitRejection::NonPositiveCoefficient {
                kind,
                estimate_ppm_per_token,
            } => {
                assert_eq!(kind, TokenKind::Input);
                assert!(
                    estimate_ppm_per_token < 0.0,
                    "estimate must be negative, got {estimate_ppm_per_token}"
                );
            }
            other @ (FitRejection::InsufficientObservations { .. }
            | FitRejection::Underidentified { .. }
            | FitRejection::NonPositiveSlope { .. }
            | FitRejection::IllConditioned { .. }
            | FitRejection::ZeroCreditSpan
            | FitRejection::BaselinePlateauNotSettled
            | FitRejection::TerminalPlateauNotSettled
            | FitRejection::MissingCostModelTerm { .. }
            | FitRejection::ContaminatedSeries { .. }) => {
                panic!("expected non-positive-coefficient rejection, got {other}")
            }
        }
        let message = rejection.to_string();
        assert!(
            message.contains("input"),
            "message names the kind: {message}"
        );
        assert!(
            message.contains("cannot reduce"),
            "message states why: {message}"
        );
    }

    // A fixed collinear design is rejected, while separately generated
    // deliberately varied designs are accepted and recover the seeded
    // coefficients within their reported intervals. The varied rows scale two
    // fixed, well-separated phase directions by generated weights, so every
    // draw stays far inside the gate by construction rather than by luck.
    proptest::proptest! {
        #[test]
        fn prop_fixed_collinear_rejected_while_varied_designs_recover_truth(
            s0 in 0.5f64..2.0, s1 in 0.5f64..2.0, s2 in 0.5f64..2.0,
            t0 in 0.5f64..2.0, t1 in 0.5f64..2.0, t2 in 0.5f64..2.0,
        ) {
            let rejected = fit_multivariate(
                &collinear_observations(),
                &kinds(),
                &config(30.0, 4),
                "single-phase-fixed-proportion",
            );
            prop_assert!(
                matches!(rejected, Err(FitRejection::IllConditioned { .. })),
                "fixed collinear design must always be rejected"
            );

            let scales = [s0, s1, s2, t0, t1, t2];
            let directions = [
                (2000.0, 150.0),
                (2000.0, 150.0),
                (2000.0, 150.0),
                (150.0, 2000.0),
                (150.0, 2000.0),
                (150.0, 2000.0),
            ];
            let varied: Vec<MultivariateFitObservation> = scales
                .iter()
                .zip(directions.iter())
                .enumerate()
                .map(|(i, (scale, (base_in, base_cache)))| {
                    let input = (*base_in * scale) as u64;
                    let cache = (*base_cache * scale) as u64;
                    observation(
                        &format!("ev-prop-{i}"),
                        input,
                        cache,
                        exact_delta(input, cache),
                    )
                })
                .collect();
            let result = fit_multivariate(
                &varied,
                &kinds(),
                &config(30.0, 4),
                "varied-two-phase(generated)",
            );
            prop_assert!(
                result.is_ok(),
                "generated varied design must be accepted"
            );
            let result = result.unwrap();
            prop_assert!(
                result.condition_number() < 30.0,
                "generated design must sit inside the gate, got {}",
                result.condition_number()
            );
            for coefficient in result.coefficients() {
                let truth = match coefficient.kind() {
                    TokenKind::Input => INPUT_TRUTH_PPM_PER_TOKEN,
                    TokenKind::Output => 0.3,
                    TokenKind::CacheRead => CACHE_READ_TRUTH_PPM_PER_TOKEN,
                    TokenKind::CacheWrite => 0.05,
                };
                // Float-rounding allowance: the y values are exact, but the
                // normal-equation solve still rounds at the 1e-15 level.
                let slack = 1e-6 * truth.abs().max(1.0);
                prop_assert!(
                    (coefficient.estimate_ppm_per_token() - truth).abs() <= slack,
                    "estimate {} must recover truth {truth}",
                    coefficient.estimate_ppm_per_token()
                );
                prop_assert!(
                    coefficient.interval_low_ppm_per_token() - slack <= truth
                        && truth <= coefficient.interval_high_ppm_per_token() + slack,
                    "reported interval must contain truth {truth} up to float rounding"
                );
            }
        }
    }

    /// The threshold is read from configuration and recorded on the result: the
    /// same varied design is accepted under a threshold of 30 and rejected
    /// under a threshold of 1.
    #[test]
    fn threshold_read_from_configuration_and_recorded_on_result() {
        let observations = varied_observations();
        let accepted = fit_multivariate(
            &observations,
            &kinds(),
            &config(30.0, 4),
            "varied-two-phase",
        )
        .expect("varied design must pass a threshold of 30");
        assert_eq!(accepted.condition_number_threshold(), 30.0);

        let strict = config(1.0 + 1e-9, 4);
        let rejection = fit_multivariate(&observations, &kinds(), &strict, "varied-two-phase")
            .expect_err("any non-orthogonal design must fail a threshold at the floor");
        match rejection {
            FitRejection::IllConditioned { threshold, .. } => {
                assert_eq!(threshold, 1.0 + 1e-9);
            }
            other @ (FitRejection::InsufficientObservations { .. }
            | FitRejection::Underidentified { .. }
            | FitRejection::NonPositiveSlope { .. }
            | FitRejection::NonPositiveCoefficient { .. }
            | FitRejection::ZeroCreditSpan
            | FitRejection::BaselinePlateauNotSettled
            | FitRejection::TerminalPlateauNotSettled
            | FitRejection::MissingCostModelTerm { .. }
            | FitRejection::ContaminatedSeries { .. }) => {
                panic!("expected ill-conditioned rejection, got {other}")
            }
        }
    }

    /// Degenerate inputs are refused or named: bad configs, non-finite deltas,
    /// and a kind with no variation at all, which the gate rejects with no
    /// correlation pair to name.
    #[test]
    fn degenerate_inputs_refused_or_named() {
        assert!(matches!(
            MultivariateFitConfig::new(1.0, 4, 0.0, true),
            Err(MultivariateFitConfigError::ThresholdMustExceedOne { .. })
        ));
        assert!(matches!(
            MultivariateFitConfig::new(f64::NAN, 4, 0.0, true),
            Err(MultivariateFitConfigError::ThresholdMustExceedOne { .. })
        ));
        assert_eq!(
            MultivariateFitConfig::new(30.0, 0, 0.0, true),
            Err(MultivariateFitConfigError::MinimumMustBePositive)
        );
        assert!(matches!(
            MultivariateFitConfig::new(30.0, 4, -0.5, true),
            Err(MultivariateFitConfigError::RidgeMustBeFiniteNonNegative { .. })
        ));
        assert!(matches!(
            MultivariateFitObservation::new(EvidenceId::new("ev-nan"), tokens(1, 1), f64::NAN),
            Err(MultivariateObservationError::NonFiniteQuotaDelta { .. })
        ));

        let flat_cache: Vec<MultivariateFitObservation> = (1..=4)
            .map(|step| {
                observation(
                    &format!("ev-flat-{step}"),
                    100 * step,
                    0,
                    50.0 * step as f64,
                )
            })
            .collect();
        let rejection =
            fit_multivariate(&flat_cache, &kinds(), &config(30.0, 2), "flat-cache-kind")
                .expect_err("a kind with no variation must be rejected");
        match &rejection {
            FitRejection::IllConditioned { entangled, .. } => {
                assert!(
                    entangled.is_empty(),
                    "no pair varies, so no correlation exists to report"
                );
            }
            other @ (FitRejection::InsufficientObservations { .. }
            | FitRejection::Underidentified { .. }
            | FitRejection::NonPositiveSlope { .. }
            | FitRejection::NonPositiveCoefficient { .. }
            | FitRejection::ZeroCreditSpan
            | FitRejection::BaselinePlateauNotSettled
            | FitRejection::TerminalPlateauNotSettled
            | FitRejection::MissingCostModelTerm { .. }
            | FitRejection::ContaminatedSeries { .. }) => {
                panic!("expected ill-conditioned rejection, got {other}")
            }
        }
        assert!(
            rejection.to_string().contains("no token-kind pair"),
            "message says what is missing: {}",
            rejection
        );
    }
}
