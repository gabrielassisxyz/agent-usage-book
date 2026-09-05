//! Typed report models for `aub calibrate show|history|compare|activate`
//! (`aub-c0b.9`, PLAN.md 23.3, 27, 50).
//!
//! The four subcommands replace the idea of a singleton coefficient command:
//! `show` renders the active calibration together with everything needed to
//! judge it, `history` lists every calibration with its lifecycle events,
//! `compare` reports the difference between a candidate and the active record,
//! and `activate` records the explicit activation. No report here carries a
//! bare coefficient: every fitted value travels with its residual and its
//! uncertainty interval, so a renderer cannot print one without the other.
//!
//! The models carry plain data (strings and micro-unit integers), never store
//! rows: the CLI translates `store::calibration` records into these shapes,
//! and presentation renders them. That keeps the `presentation must not import
//! store or calibration` boundary intact.
//!
//! May not depend on:
//! - presentation
//! - terminal-formatting crates
//! - provider adapters

use crate::report::models::ReportMetadata;

/// One token kind's coverage in the cost model a calibration references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateCostModelKindCoverage {
    /// The stable snake-case label (`input`, `output`, `cache_read`, `cache_write`).
    pub kind_label: String,
    /// True when the cost model carries a term for this kind.
    pub modeled: bool,
}

/// The per-kind coverage of the cost model a calibration references, plus the
/// unknown-kind statement. `unknown_kinds` is empty when the calibration
/// workload carried no unknown token components; the renderer prints `none`
/// for the empty case so presence and absence are both visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateCostModelCoverage {
    pub cost_model_id: String,
    /// False when the referenced cost model row is absent from the ledger.
    pub cost_model_found: bool,
    pub kinds: Vec<CalibrateCostModelKindCoverage>,
    pub unknown_kinds: Vec<String>,
}

/// One activation or supersession event on a calibration, as history shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateLifecycleEventView {
    /// `activation` or `supersession`.
    pub kind_label: String,
    pub event_at_nanos: i64,
    pub actor: String,
    pub activation_policy_version: String,
    /// The calibration this event superseded, when the event is a supersession.
    pub supersedes: Option<String>,
}

/// A single calibration as `show` renders it: the active coefficient together
/// with the residual, the uncertainty, the cost-model version and its
/// token-kind coverage, the plan tier, the fit date, the method, the evidence
/// experiment, the input hash, the fitter version and the health state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateShowEntry {
    pub calibration_id: String,
    pub provider: String,
    pub window_semantic_key: String,
    pub plan_tier: String,
    pub cost_model: CalibrateCostModelCoverage,
    pub fitted_micros_per_point: i64,
    pub equivalent_full_window_capacity_micros: i64,
    pub fit_residual_micros: i64,
    pub out_of_sample_residual_micros: Option<i64>,
    pub uncertainty_low_micros_per_point: i64,
    pub uncertainty_high_micros_per_point: i64,
    pub statistical_method: String,
    pub statistical_parameters: String,
    pub validation_method: String,
    pub validation_version: String,
    pub evidence_experiment_ids: Vec<String>,
    pub inputs_digest_hex: String,
    pub inputs_count: usize,
    pub fitting_evidence_digest_hex: String,
    pub validation_evidence_digest_hex: String,
    pub fit_timestamp_nanos: i64,
    pub activation_policy_version: String,
    pub aub_version: String,
    pub source_revision: String,
    pub health_label: String,
    /// True when this entry is the active calibration for its scope at the
    /// report's knowledge time.
    pub is_active: bool,
}

/// The `calibrate show` report: every currently active calibration, one entry
/// per scope. Empty when no scope has an active calibration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateShowReport {
    pub metadata: ReportMetadata,
    pub entries: Vec<CalibrateShowEntry>,
}

/// One row of `calibrate history`: a calibration with its health state and its
/// activation and supersession events in event order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateHistoryEntry {
    pub calibration_id: String,
    pub provider: String,
    pub window_semantic_key: String,
    pub plan_tier: String,
    pub fitted_micros_per_point: i64,
    pub fit_residual_micros: i64,
    pub uncertainty_low_micros_per_point: i64,
    pub uncertainty_high_micros_per_point: i64,
    pub fit_timestamp_nanos: i64,
    pub health_label: String,
    pub events: Vec<CalibrateLifecycleEventView>,
}

/// The `calibrate history` report: every calibration result in the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateHistoryReport {
    pub metadata: ReportMetadata,
    pub entries: Vec<CalibrateHistoryEntry>,
}

/// The `calibrate compare` report: the difference between a candidate and the
/// active record, plus the candidate's activation status stated plainly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateCompareReport {
    pub metadata: ReportMetadata,
    pub candidate_id: String,
    pub active_id: String,
    pub candidate_fitted_micros_per_point: i64,
    pub active_fitted_micros_per_point: i64,
    /// Signed difference in basis points (hundredths of a percent), rounded to
    /// the nearest basis point: 380 means the candidate is 3.8 percent above
    /// the active record.
    pub difference_bps: i64,
    pub candidate_fit_residual_micros: i64,
    pub candidate_uncertainty_low_micros_per_point: i64,
    pub candidate_uncertainty_high_micros_per_point: i64,
    pub active_fit_residual_micros: i64,
    pub active_uncertainty_low_micros_per_point: i64,
    pub active_uncertainty_high_micros_per_point: i64,
    pub candidate_health_label: String,
    /// True when the candidate is the active calibration for its scope.
    pub candidate_is_active: bool,
}

/// Computes the signed difference in basis points between a candidate and the
/// active fitted coefficient: `(candidate - active) * 10_000 / active`,
/// rounded to the nearest basis point. Returns zero when the active
/// coefficient is zero rather than dividing by zero; a zero fitted
/// coefficient carries no meaningful percentage difference.
pub fn calibrate_difference_bps(candidate_micros: i64, active_micros: i64) -> i64 {
    if active_micros == 0 {
        return 0;
    }
    let numerator = i128::from(candidate_micros - active_micros) * 10_000;
    let denominator = i128::from(active_micros);
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder.abs() * 2 >= denominator.abs() {
        if (numerator >= 0) == (denominator > 0) {
            (quotient + 1) as i64
        } else {
            (quotient - 1) as i64
        }
    } else {
        quotient as i64
    }
}

/// Formats a basis-point difference as a percentage with one decimal place,
/// for example 380 as `3.8%` and -45 as `-0.5%` (sign on the whole figure).
pub fn format_calibrate_difference_percent(difference_bps: i64) -> String {
    let sign = if difference_bps < 0 { "-" } else { "" };
    let magnitude = difference_bps.abs();
    format!("{sign}{}.{}%", magnitude / 100, (magnitude % 100) / 10)
}

/// The `calibrate activate` report: the explicit activation just recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrateActivateReport {
    pub metadata: ReportMetadata,
    pub calibration_id: String,
    /// The calibration this activation superseded, if the scope already had
    /// an active calibration.
    pub supersedes: Option<String>,
    pub actor: String,
    pub activation_policy_version: String,
    pub event_at_nanos: i64,
    pub fitting_evidence_digest_hex: String,
    pub validation_evidence_digest_hex: String,
    pub fitted_micros_per_point: i64,
    pub fit_residual_micros: i64,
    pub uncertainty_low_micros_per_point: i64,
    pub uncertainty_high_micros_per_point: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difference_bps_matches_the_design_example() {
        assert_eq!(format_calibrate_difference_percent(380), "3.8%");
        assert_eq!(format_calibrate_difference_percent(-380), "-3.8%");
        assert_eq!(format_calibrate_difference_percent(0), "0.0%");
        assert_eq!(format_calibrate_difference_percent(5), "0.0%");
        assert_eq!(format_calibrate_difference_percent(1_000), "10.0%");
    }

    #[test]
    fn difference_bps_computes_candidate_minus_active_over_active() {
        assert_eq!(calibrate_difference_bps(1_038_000, 1_000_000), 380);
        assert_eq!(calibrate_difference_bps(1_000_000, 1_000_000), 0);
        assert_eq!(calibrate_difference_bps(900_000, 1_000_000), -1_000);
        assert_eq!(calibrate_difference_bps(5, 0), 0);
    }
}
