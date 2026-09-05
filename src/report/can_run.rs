//! The can-run advisory report (`aub-cab.4`).
//!
//! Evaluates whether a proposed task can run against current quota from calibrated
//! credit headroom, joining:
//! 1. Fresh/cached meter observation windows
//! 2. Window calibration health and uncertainty
//! 3. Historical task distribution and attribution coverage
//! 4. Cost model token class pricing
//!
//! # Refusal semantics
//!
//! [`compose_can_run_report`] is where the join happens and where the seven refusal
//! conditions PLAN.md 26.6 names are checked: stale meter, authentication required,
//! any constraining window without an applicable current calibration, a cost model
//! missing a token class, a plan tier mismatch, too few historical tasks, and mostly
//! unattributable task records. All seven are checked in one pass and every
//! applicable one is reported in the same invocation, rather than the first one
//! found. The distribution ([`crate::advice::historical_distribution`]), headroom
//! ([`crate::advice::headroom`]) and limiting-window/verdict selection
//! ([`crate::advice::verdict`]) logic themselves belong to earlier beads and are
//! never reimplemented here: this module only decides whether their preconditions
//! hold and, when they do, reshapes their output into this report's own shape.
//!
//! Calibration health is checked directly against [`CalibrationHealth`] rather than
//! through [`crate::advice::headroom::WindowHeadroom`]'s generic `Unknown` variant,
//! so a `ReviewDue`, `Suspect`, `Superseded`, `Inapplicable` or `Provisional`
//! calibration produces a refusal reason naming that exact state rather than a
//! generic "no applicable current calibration".

use std::collections::BTreeMap;

use crate::advice::headroom::{CalibratedWindowConstraint, WindowHeadroom, window_credit_headroom};
use crate::advice::historical_distribution::{
    AttributionCoverage, DistributionVerdict, ExclusionCounts,
};
use crate::advice::verdict::{
    AmpleMarginMultiple, CanRunAssessment, CanRunHeadroomBound, CanRunMissingFact,
    CanRunVerdictConfig, CanRunVerdictLabel, LabelledVerdictBasis, TaskReferenceInput,
    assess_can_run,
};
use crate::domain::credits::Credits;
use crate::domain::failure::FailureClass;
use crate::domain::freshness::StaleReason;
use crate::domain::interval::Interval;
use crate::domain::time::{MonotonicDuration, UtcTimestamp};
use crate::domain::window::{MeterWindow, ModelId, WindowSemanticKey};
use crate::report::models::ReportMetadata;
use crate::report::provenance::ProvenanceGraph;

/// One window's headroom and status in the advisory report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunWindowReport {
    pub semantic_key: WindowSemanticKey,
    pub remaining_fraction_ppm: u32,
    pub calibration_id: String,
    pub headroom: Interval<Credits>,
    pub resets_at: Option<UtcTimestamp>,
}

/// Historical exact task evidence summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunTaskEvidence {
    pub sample_count: usize,
    pub median: Credits,
    pub central_range: Interval<Credits>,
    pub upper_reference: Credits,
}

/// Calibration status and token coverage summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunCalibrationSummary {
    pub description: String,
    pub token_kind_coverage_complete: bool,
}

/// One window's margin comparison against historical central range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunWindowComparison {
    pub semantic_key: WindowSemanticKey,
    pub margin: Interval<Credits>,
}

/// Comparison of historical upper reference against limiting window headroom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunUpperReferenceComparison {
    pub limiting_window: WindowSemanticKey,
    pub exceeds: bool,
    pub diff: Interval<Credits>,
}

/// A ready, quantitative can-run assessment.
#[derive(Debug, Clone, PartialEq)]
pub struct CanRunReadyReport {
    pub observed_age: Option<MonotonicDuration>,
    pub windows: Vec<CanRunWindowReport>,
    pub lowest_percentage_window: WindowSemanticKey,
    pub limiting_window: WindowSemanticKey,
    pub windows_differ: bool,
    pub task_evidence: CanRunTaskEvidence,
    pub calibration_summary: CanRunCalibrationSummary,
    pub comparisons: Vec<CanRunWindowComparison>,
    pub upper_reference_comparison: Option<CanRunUpperReferenceComparison>,
    pub assessment: CanRunVerdictLabel,
    pub label_basis: Option<LabelledVerdictBasis>,
}

/// Detailed evidence when attribution coverage floor is breached (`aub-cab.7`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributionRefusalEvidence {
    pub numerator_micros: i64,
    pub denominator_micros: i64,
    pub observed_fraction_ppm: u32,
    pub required_floor_ppm: u32,
    pub selection_window: String,
    pub group: String,
    pub unknown_token_components: usize,
    pub unknown_account_attribution: usize,
    pub incomplete_segmentation: usize,
    pub estimated_tokens: usize,
}

/// A refusal to answer quantitatively with explanations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanRunRefusalReport {
    pub verdict: CanRunVerdictLabel,
    pub missing: Vec<CanRunMissingFact>,
    pub attribution_quality: Option<AttributionRefusalEvidence>,
}

/// The outcome of the can-run evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum CanRunOutcome {
    Ready(CanRunReadyReport),
    Refused(CanRunRefusalReport),
}

/// The top-level report for `aub can-run`.
#[derive(Debug, Clone, PartialEq)]
pub struct CanRunReport {
    pub metadata: ReportMetadata,
    pub task_kind: String,
    pub account: String,
    pub model: String,
    pub outcome: CanRunOutcome,
    pub provenance: ProvenanceGraph,
}

impl CanRunReport {
    pub fn new(
        metadata: ReportMetadata,
        task_kind: impl Into<String>,
        account: impl Into<String>,
        model: impl Into<String>,
        outcome: CanRunOutcome,
        provenance: ProvenanceGraph,
    ) -> Self {
        Self {
            metadata,
            task_kind: task_kind.into(),
            account: account.into(),
            model: model.into(),
            outcome,
            provenance,
        }
    }
}

/// The readiness of the meter reading the join was given, reduced to exactly the
/// facts a refusal decision needs. `windows` is the full set of provider windows
/// known at the observation, not yet filtered to the ones constraining the
/// requested model: that filtering happens inside [`compose_can_run_report`] so
/// the "no constraining window at all" refusal can name the model it looked for.
#[derive(Debug, Clone, PartialEq)]
pub enum CanRunMeterReadiness {
    Fresh {
        windows: Vec<MeterWindow>,
        observed_age: Option<MonotonicDuration>,
    },
    Stale {
        reason: StaleReason,
    },
    AuthRequired,
}

/// One constraining window's calibration lookup result: a display id together
/// with its typed health and uncertainty. Absent entirely (`None` in the
/// composing map) means no calibration record exists for the window at all,
/// which the join distinguishes from a record whose health is not current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCalibrationLookup {
    pub calibration_id: String,
    pub constraint: CalibratedWindowConstraint,
}

/// A configured account plan tier that does not match the plan tier the active
/// calibration was fitted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTierMismatch {
    pub account_plan_tier: String,
    pub calibration_plan_tier: String,
}

/// Every input the join needs, gathered by the caller from the meter, the
/// calibration store, the cost model, the plan tier configuration and the
/// historical task distribution (`aub-cab.1`). This function performs no I/O:
/// every refusal is a pure decision over these already-fetched typed facts.
#[derive(Clone)]
pub struct CanRunJoinInputs {
    pub metadata: ReportMetadata,
    pub task_kind: String,
    pub account: String,
    pub model: ModelId,
    pub meter: CanRunMeterReadiness,
    pub window_calibrations: BTreeMap<WindowSemanticKey, WindowCalibrationLookup>,
    pub cost_model_missing_token_classes: Vec<String>,
    pub plan_tier_mismatch: Option<PlanTierMismatch>,
    pub task: TaskReferenceInput,
    pub attribution: AttributionCoverage,
    pub attribution_exclusions: ExclusionCounts,
    pub attribution_selection_window: String,
    pub attribution_group: String,
    /// `aub-jsq`'s multiple (2026-08-25), read from `crate::config`, never a literal.
    pub ample_margin_multiple: AmpleMarginMultiple,
    /// `aub-jsq`'s bound (2026-08-25), read from `crate::config`, never a literal.
    pub headroom_bound: CanRunHeadroomBound,
    pub provenance: ProvenanceGraph,
}

fn missing_fact(subject: impl Into<String>, reason: impl Into<String>) -> CanRunMissingFact {
    CanRunMissingFact {
        subject: subject.into(),
        reason: reason.into(),
    }
}

/// Names a stale reading's cause for a missing-fact reason. Deliberately local
/// to this module rather than reusing a presentation helper: `report` may not
/// depend on `presentation` (the dependency runs the other way), and this text
/// is a refusal reason, not a rendered surface.
fn stale_reason_text(reason: StaleReason) -> String {
    match reason {
        StaleReason::AgeExceeded => {
            "the last successful observation is older than the freshness policy allows".to_string()
        }
        StaleReason::NoSuccessfulObservation => {
            "no successful observation has ever been recorded".to_string()
        }
        StaleReason::SourceUnreachable(class) => format!(
            "provider usage source was unreachable ({})",
            failure_class_text(class)
        ),
        StaleReason::MalformedProviderResponse => {
            "the provider's response could not be parsed".to_string()
        }
        StaleReason::RateLimited => "the provider rate-limited the collection attempt".to_string(),
        StaleReason::SamplingGap => {
            "no collection attempt has been recorded since the last good observation".to_string()
        }
        StaleReason::ClockAnomaly => {
            "the observation timestamp is inconsistent with the local clock".to_string()
        }
        StaleReason::CollectorInterrupted => {
            "the collection attempt started but never recorded a result".to_string()
        }
        StaleReason::CredentialChangedUnverified => {
            "the credential changed and has not yet been verified by a successful observation"
                .to_string()
        }
    }
}

fn failure_class_text(class: FailureClass) -> String {
    match class {
        FailureClass::DnsFailure => "dns failure".to_string(),
        FailureClass::ConnectTimeout => "connect timeout".to_string(),
        FailureClass::ReadTimeout => "read timeout".to_string(),
        FailureClass::TotalBudgetExpired => "total budget expired".to_string(),
        FailureClass::HttpStatus(status) => format!("http status {status:?}"),
        FailureClass::RateLimited { .. } => "rate limited".to_string(),
        FailureClass::MalformedBody => "malformed body".to_string(),
        FailureClass::MissingRequiredField => "missing required field".to_string(),
    }
}

/// Joins the four inputs (PLAN.md 26.1) into a can-run report, refusing a
/// quantitative answer when any of the seven prerequisites (PLAN.md 26.6) is
/// missing and naming every one that is missing in the same invocation, rather
/// than the first one found.
pub fn compose_can_run_report(inputs: CanRunJoinInputs) -> CanRunReport {
    let mut missing: Vec<CanRunMissingFact> = Vec::new();

    // 1 & 2: stale meter, authentication required.
    let known_windows: Vec<(&MeterWindow, &WindowCalibrationLookup)> = match &inputs.meter {
        CanRunMeterReadiness::Stale { reason } => {
            missing.push(missing_fact("meter", stale_reason_text(*reason)));
            Vec::new()
        }
        CanRunMeterReadiness::AuthRequired => {
            missing.push(missing_fact(
                "meter",
                format!(
                    "authentication is required for account '{}'",
                    inputs.account
                ),
            ));
            Vec::new()
        }
        CanRunMeterReadiness::Fresh { windows, .. } => {
            let constraining: Vec<&MeterWindow> = windows
                .iter()
                .filter(|window| window.constrains(&inputs.model))
                .collect();
            if constraining.is_empty() {
                missing.push(missing_fact(
                    "constraining_windows",
                    format!(
                        "no provider windows constrain the selected model '{}'",
                        inputs.model.as_str()
                    ),
                ));
                Vec::new()
            } else {
                let mut known = Vec::new();
                // 3: any constraining window has no applicable calibration, checked
                // through the shared typed health decision rather than through the
                // generic Unknown headroom evaluation, so a `ReviewDue`, `Suspect`,
                // `Superseded`, `Inapplicable` or `Provisional` calibration produces
                // a refusal naming that exact state.
                for window in constraining {
                    match inputs.window_calibrations.get(window.semantic_key()) {
                        None => {
                            missing.push(missing_fact(
                                window.semantic_key().as_str(),
                                "no calibration is recorded for this window",
                            ));
                        }
                        Some(lookup) if !lookup.constraint.is_current() => {
                            missing.push(missing_fact(
                                window.semantic_key().as_str(),
                                format!(
                                    "calibration #{} health is {}, not current",
                                    lookup.calibration_id,
                                    lookup.constraint.health().label()
                                ),
                            ));
                        }
                        Some(lookup) => known.push((window, lookup)),
                    }
                }
                known
            }
        }
    };

    // 4: cost model lacks one token class.
    if !inputs.cost_model_missing_token_classes.is_empty() {
        missing.push(missing_fact(
            "cost_model",
            format!(
                "cost model is missing coverage for token class(es): {}",
                inputs.cost_model_missing_token_classes.join(", ")
            ),
        ));
    }

    // 5: plan tier does not match calibration.
    if let Some(mismatch) = &inputs.plan_tier_mismatch {
        missing.push(missing_fact(
            "plan_tier",
            format!(
                "account plan tier '{}' does not match calibration plan tier '{}'",
                mismatch.account_plan_tier, mismatch.calibration_plan_tier
            ),
        ));
    }

    // 6: too few historical tasks for the configured range policy.
    if let DistributionVerdict::InsufficientEvidence { min_samples } = &inputs.task.verdict {
        missing.push(missing_fact(
            "historical_task_evidence",
            format!(
                "fewer than {min_samples} eligible completed sample(s) (got {})",
                inputs.task.sample_count
            ),
        ));
    }

    // 7: task records are mostly unattributable.
    let attribution_evidence = if inputs.attribution.is_below_floor() {
        let evidence = AttributionRefusalEvidence {
            numerator_micros: inputs.attribution.fraction.numerator() as i64,
            denominator_micros: inputs.attribution.fraction.denominator() as i64,
            observed_fraction_ppm: inputs.attribution.fraction.ppm().unwrap_or(0),
            required_floor_ppm: inputs.attribution.floor.ppm(),
            selection_window: inputs.attribution_selection_window.clone(),
            group: inputs.attribution_group.clone(),
            unknown_token_components: inputs.attribution_exclusions.unknown_token_components,
            unknown_account_attribution: inputs.attribution_exclusions.unknown_account_attribution,
            incomplete_segmentation: inputs.attribution_exclusions.incomplete_segmentation,
            estimated_tokens: inputs.attribution_exclusions.estimated_tokens,
        };
        missing.push(missing_fact(
            "attribution_quality",
            "attribution_coverage_below_floor",
        ));
        Some(evidence)
    } else {
        None
    };

    let outcome = if !missing.is_empty() {
        // A refusal driven only by thin history keeps the more specific
        // INSUFFICIENT_EVIDENCE token (PLAN.md 26.5's vocabulary); every other
        // shape, including a mix with thin history, is UNKNOWN: current quota or
        // calibration cannot be justified.
        let only_thin_history =
            missing.len() == 1 && missing[0].subject == "historical_task_evidence";
        let verdict = if only_thin_history {
            CanRunVerdictLabel::InsufficientEvidence
        } else {
            CanRunVerdictLabel::Unknown
        };
        CanRunOutcome::Refused(CanRunRefusalReport {
            verdict,
            missing,
            attribution_quality: attribution_evidence,
        })
    } else {
        let (median, central_range, upper_reference) = match inputs.task.verdict {
            DistributionVerdict::Distribution {
                median,
                central_range,
                upper_reference,
                ..
            } => (median, central_range, upper_reference),
            DistributionVerdict::InsufficientEvidence { .. } => {
                unreachable!(
                    "insufficient historical evidence was already turned into a missing fact above"
                )
            }
        };

        let evaluated: Vec<WindowHeadroom<'_>> = known_windows
            .iter()
            .map(|(window, lookup)| window_credit_headroom(window, Some(&lookup.constraint)))
            .collect();

        let config = CanRunVerdictConfig {
            labels_enabled: true,
            ample_margin_multiple: inputs.ample_margin_multiple,
            headroom_bound: inputs.headroom_bound,
        };

        match assess_can_run(&evaluated, &inputs.task, &config) {
            CanRunAssessment::Ready(ready) => {
                let calibration_id_of = |key: &WindowSemanticKey| -> String {
                    known_windows
                        .iter()
                        .find(|(window, _)| window.semantic_key() == key)
                        .map(|(_, lookup)| lookup.calibration_id.clone())
                        .unwrap_or_default()
                };

                let windows: Vec<CanRunWindowReport> = ready
                    .per_window
                    .iter()
                    .map(|assessment| CanRunWindowReport {
                        semantic_key: assessment.window.semantic_key().clone(),
                        remaining_fraction_ppm: assessment
                            .window
                            .remaining_fraction()
                            .as_ppm()
                            .get(),
                        calibration_id: calibration_id_of(assessment.window.semantic_key()),
                        headroom: assessment.headroom,
                        resets_at: assessment.window.resets_at(),
                    })
                    .collect();

                let comparisons: Vec<CanRunWindowComparison> = ready
                    .per_window
                    .iter()
                    .map(|assessment| CanRunWindowComparison {
                        semantic_key: assessment.window.semantic_key().clone(),
                        margin: assessment.margin,
                    })
                    .collect();

                let limiting_headroom = ready
                    .per_window
                    .iter()
                    .find(|assessment| {
                        assessment.window.semantic_key() == ready.limiting_window.semantic_key()
                    })
                    .map(|assessment| assessment.headroom)
                    .expect("limiting window is one of per_window's entries");

                let exceeds = upper_reference.micros() > limiting_headroom.lower().micros();
                let diff = if exceeds {
                    Interval::new(
                        upper_reference - limiting_headroom.upper(),
                        upper_reference - limiting_headroom.lower(),
                    )
                    .expect("upper_reference - upper <= upper_reference - lower")
                } else {
                    Interval::new(
                        limiting_headroom.lower() - upper_reference,
                        limiting_headroom.upper() - upper_reference,
                    )
                    .expect("headroom is itself a valid interval, shifted by a constant")
                };

                let calibration_ids: Vec<&str> = windows
                    .iter()
                    .map(|window| window.calibration_id.as_str())
                    .collect();
                let description = match calibration_ids.as_slice() {
                    [single] => format!("#{single}, current"),
                    [first, second] => format!("#{first} and #{second}, both current"),
                    many => format!(
                        "{}, all current",
                        many.iter()
                            .map(|id| format!("#{id}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };

                let label_basis = ready
                    .label_basis
                    .expect("labels_enabled is always true in this join's verdict config");

                let ready_report = CanRunReadyReport {
                    observed_age: match &inputs.meter {
                        CanRunMeterReadiness::Fresh { observed_age, .. } => *observed_age,
                        CanRunMeterReadiness::Stale { .. } | CanRunMeterReadiness::AuthRequired => {
                            unreachable!(
                                "a stale or auth-required meter reading was already turned into a missing fact above"
                            )
                        }
                    },
                    windows,
                    lowest_percentage_window: ready.lowest_percentage_window.semantic_key().clone(),
                    limiting_window: ready.limiting_window.semantic_key().clone(),
                    windows_differ: ready.windows_differ,
                    task_evidence: CanRunTaskEvidence {
                        sample_count: ready.sample_count,
                        median,
                        central_range,
                        upper_reference,
                    },
                    calibration_summary: CanRunCalibrationSummary {
                        description,
                        token_kind_coverage_complete: true,
                    },
                    comparisons,
                    upper_reference_comparison: Some(CanRunUpperReferenceComparison {
                        limiting_window: ready.limiting_window.semantic_key().clone(),
                        exceeds,
                        diff,
                    }),
                    assessment: label_basis.label,
                    label_basis: Some(label_basis),
                };
                CanRunOutcome::Ready(ready_report)
            }
            CanRunAssessment::InsufficientEvidence(refusal) => {
                CanRunOutcome::Refused(CanRunRefusalReport {
                    verdict: CanRunVerdictLabel::InsufficientEvidence,
                    missing: refusal.missing,
                    attribution_quality: None,
                })
            }
            CanRunAssessment::Unknown(refusal) => CanRunOutcome::Refused(CanRunRefusalReport {
                verdict: CanRunVerdictLabel::Unknown,
                missing: refusal.missing,
                attribution_quality: None,
            }),
        }
    };

    CanRunReport::new(
        inputs.metadata,
        inputs.task_kind,
        inputs.account,
        inputs.model.as_str().to_string(),
        outcome,
        inputs.provenance,
    )
}

#[cfg(test)]
mod compose_tests {
    use super::*;
    use crate::advice::headroom::CalibrationHealth;
    use crate::advice::historical_distribution::{Percentile, QuantileMethod};
    use crate::attribution::quality::{AttributionFraction, AttributionQualityFloor};
    use crate::domain::credits::{Credits, CreditsPerPercentagePoint};
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::UtcTimestamp;
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    };
    use crate::report::models::LedgerGeneration;
    use crate::store::calibration::CoefficientUncertainty;

    fn credits(whole: i64) -> Credits {
        Credits::from_micros(whole * 1_000_000)
    }

    fn metadata() -> ReportMetadata {
        ReportMetadata::new(
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            LedgerGeneration::new(1),
            None,
        )
    }

    fn make_window(key: &str, scope: WindowScope, used_ppm: i32, resets_at: i64) -> MeterWindow {
        MeterWindow::new(
            WindowSemanticKey::new(key),
            scope,
            QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::RoundedToNearest,
            UtcTimestamp::from_unix_nanos(resets_at),
            NominalWindowDuration::from_nanos(1_000_000_000),
        )
    }

    fn interval_calibration(id: &str, low: i64, high: i64) -> WindowCalibrationLookup {
        let unc = CoefficientUncertainty::new(
            CreditsPerPercentagePoint::from_micros_per_point(low),
            CreditsPerPercentagePoint::from_micros_per_point(high),
        )
        .expect("valid uncertainty");
        WindowCalibrationLookup {
            calibration_id: id.to_string(),
            constraint: CalibratedWindowConstraint::current(unc),
        }
    }

    fn sufficient_task() -> TaskReferenceInput {
        TaskReferenceInput {
            verdict: DistributionVerdict::Distribution {
                median: credits(640),
                central_range: Interval::new(credits(550), credits(940)).unwrap(),
                central_low_percentile: Percentile::new(25).unwrap(),
                central_high_percentile: Percentile::new(75).unwrap(),
                upper_reference: credits(1520),
                upper_percentile: Percentile::new(90).unwrap(),
                quantile_method: QuantileMethod::NearestRank,
            },
            sample_count: 23,
        }
    }

    fn full_attribution_coverage() -> AttributionCoverage {
        AttributionCoverage {
            fraction: AttributionFraction::new(95, 100),
            floor: AttributionQualityFloor::new(0.80).unwrap(),
        }
    }

    /// A worked example self-consistent under exact integer arithmetic: PLAN.md
    /// section 51's own illustrative figures (52.0% remaining -> 1,040-1,180
    /// credits) are not mutually exactly divisible under this project's
    /// fixed-point calibration multiplication (`docs/PLAN.md` 26.3), so this
    /// fixture uses different, exact numbers while reproducing the same
    /// structure: two constraining windows with different limiting and
    /// lowest-percentage identities, a MARGINAL assessment, and a p90 reference
    /// that exceeds the limiting window's headroom.
    fn worked_example_inputs() -> CanRunJoinInputs {
        let model = ModelId::new("model-x");
        let account_window = make_window(
            "account:5h",
            WindowScope::AccountWide,
            600_000,
            2_000_000_000,
        );
        let weekly_window = make_window(
            "model-x:weekly",
            WindowScope::ModelSpecific(model.clone()),
            500_000,
            3_000_000_000,
        );
        let windows = vec![account_window, weekly_window];

        let mut calibrations = BTreeMap::new();
        calibrations.insert(
            WindowSemanticKey::new("account:5h"),
            interval_calibration("17", 8_000, 9_000),
        );
        calibrations.insert(
            WindowSemanticKey::new("model-x:weekly"),
            interval_calibration("22", 2_000, 2_200),
        );

        CanRunJoinInputs {
            metadata: metadata(),
            task_kind: "refactor-module".to_string(),
            account: "work-primary".to_string(),
            model,
            meter: CanRunMeterReadiness::Fresh {
                windows,
                observed_age: Some(MonotonicDuration::from_seconds(41)),
            },
            window_calibrations: calibrations,
            cost_model_missing_token_classes: Vec::new(),
            plan_tier_mismatch: None,
            task: sufficient_task(),
            attribution: full_attribution_coverage(),
            attribution_exclusions: ExclusionCounts::default(),
            attribution_selection_window: "2026-08-01..2026-09-01".to_string(),
            attribution_group: "refactor-module".to_string(),
            ample_margin_multiple: AmpleMarginMultiple::new(2.0).unwrap(),
            headroom_bound: CanRunHeadroomBound::Low,
            provenance: ProvenanceGraph::default(),
        }
    }

    fn ready_of(report: &CanRunReport) -> &CanRunReadyReport {
        match &report.outcome {
            CanRunOutcome::Ready(ready) => ready,
            CanRunOutcome::Refused(refused) => {
                panic!("expected Ready, got Refused: {refused:?}")
            }
        }
    }

    fn refused_of(report: &CanRunReport) -> &CanRunRefusalReport {
        match &report.outcome {
            CanRunOutcome::Refused(refused) => refused,
            CanRunOutcome::Ready(ready) => panic!("expected Refused, got Ready: {ready:?}"),
        }
    }

    /// Golden: the design's full example structure (PLAN.md 51), with exact
    /// numbers this integer pipeline reproduces losslessly. Exercises the
    /// entire join from raw windows and calibrations through to the rendered
    /// text.
    #[test]
    fn golden_worked_example_renders_the_designed_structure() {
        let report = compose_can_run_report(worked_example_inputs());
        let ready = ready_of(&report);

        assert_eq!(ready.limiting_window.as_str(), "model-x:weekly");
        assert_eq!(ready.lowest_percentage_window.as_str(), "account:5h");
        assert!(ready.windows_differ);
        assert_eq!(ready.assessment, CanRunVerdictLabel::Marginal);
        assert_eq!(ready.task_evidence.sample_count, 23);
        assert_eq!(ready.task_evidence.median, credits(640));

        let weekly = ready
            .windows
            .iter()
            .find(|w| w.semantic_key.as_str() == "model-x:weekly")
            .expect("weekly window is reported");
        assert_eq!(
            weekly.headroom,
            Interval::new(credits(1000), credits(1100)).unwrap()
        );
        assert_eq!(weekly.calibration_id, "22");

        let account = ready
            .windows
            .iter()
            .find(|w| w.semantic_key.as_str() == "account:5h")
            .expect("account window is reported");
        assert_eq!(
            account.headroom,
            Interval::new(credits(3200), credits(3600)).unwrap()
        );

        let comparison = ready
            .upper_reference_comparison
            .as_ref()
            .expect("labels are always enabled in this join");
        assert!(comparison.exceeds);
        assert_eq!(
            comparison.diff,
            Interval::new(credits(420), credits(520)).unwrap()
        );

        let text = crate::presentation::render::render_can_run_report(&report);
        assert!(text.contains("can-run: refactor-module"), "{text}");
        assert!(text.contains("account: work-primary"), "{text}");
        assert!(text.contains("model: model-x"), "{text}");
        assert!(text.contains("observed 41s ago"), "{text}");
        assert!(
            text.contains("lowest remaining percentage: account:5h"),
            "{text}"
        );
        assert!(
            text.contains("limiting calibrated window:  model-x:weekly"),
            "{text}"
        );
        assert!(text.contains("historical exact task evidence:"), "{text}");
        assert!(text.contains("n = 23"), "{text}");
        assert!(text.contains("p90 reference exceeds"), "{text}");
        assert!(text.contains("assessment: MARGINAL"), "{text}");
        assert!(text.contains("limiting window: model-x:weekly"), "{text}");
    }

    /// Golden: the stale-meter case renders the unknown form with its reason,
    /// carrying no fabricated numeric interval anywhere (PLAN.md 51's own
    /// stale-meter example).
    #[test]
    fn golden_stale_meter_renders_unknown_form_with_its_reason() {
        let mut inputs = worked_example_inputs();
        inputs.meter = CanRunMeterReadiness::Stale {
            reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.verdict, CanRunVerdictLabel::Unknown);
        assert_eq!(refused.missing.len(), 1);
        assert_eq!(refused.missing[0].subject, "meter");
        assert!(refused.missing[0].reason.contains("unreachable"));

        let text = crate::presentation::render::render_can_run_report(&report);
        assert!(text.contains("assessment: UNKNOWN"), "{text}");
        assert!(text.contains("reason:"), "{text}");
        assert!(!text.contains("headroom"), "{text}");
        assert!(!text.contains("credits"), "{text}");
    }

    /// Unit, refusal 1 of 7: a stale meter refuses. No last-known value ever
    /// reaches the report: `CanRunMeterReadiness::Stale` carries no numeric
    /// payload at all, so "last week's stale meter" (one of the five forbidden
    /// substitutions) cannot leak through even in principle.
    #[test]
    fn refusal_1_stale_meter_refuses_with_no_fabricated_numbers() {
        let mut inputs = worked_example_inputs();
        inputs.meter = CanRunMeterReadiness::Stale {
            reason: StaleReason::AgeExceeded,
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.missing.len(), 1);
        assert_eq!(refused.missing[0].subject, "meter");
        assert!(refused.missing[0].reason.contains("freshness policy"));
    }

    /// Unit, refusal 2 of 7: authentication required refuses and names the
    /// account.
    #[test]
    fn refusal_2_auth_required_refuses_and_names_the_account() {
        let mut inputs = worked_example_inputs();
        inputs.meter = CanRunMeterReadiness::AuthRequired;
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.missing.len(), 1);
        assert_eq!(refused.missing[0].subject, "meter");
        assert!(refused.missing[0].reason.contains("work-primary"));
    }

    /// Unit, refusal 3 of 7: a constraining window whose calibration health is
    /// not current refuses through the shared typed health decision, naming the
    /// exact state, for each of the four states this bead's acceptance
    /// criteria name plus the fifth non-current state the shared enum defines.
    #[test]
    fn refusal_3_non_current_calibration_health_names_the_exact_state() {
        for health in [
            CalibrationHealth::Provisional,
            CalibrationHealth::ReviewDue,
            CalibrationHealth::Suspect,
            CalibrationHealth::Superseded,
            CalibrationHealth::Inapplicable,
        ] {
            let mut inputs = worked_example_inputs();
            let unc = CoefficientUncertainty::new(
                CreditsPerPercentagePoint::from_micros_per_point(2_000),
                CreditsPerPercentagePoint::from_micros_per_point(2_200),
            )
            .unwrap();
            inputs.window_calibrations.insert(
                WindowSemanticKey::new("model-x:weekly"),
                WindowCalibrationLookup {
                    calibration_id: "22".to_string(),
                    constraint: CalibratedWindowConstraint::new(unc, health),
                },
            );
            let report = compose_can_run_report(inputs);
            let refused = refused_of(&report);
            let fact = refused
                .missing
                .iter()
                .find(|fact| fact.subject == "model-x:weekly")
                .unwrap_or_else(|| {
                    panic!(
                        "expected a missing fact for model-x:weekly under {health:?}: {:?}",
                        refused.missing
                    )
                });
            assert!(
                fact.reason.contains(health.label()),
                "reason must name the exact health state {health:?}: {}",
                fact.reason
            );
            assert!(!fact.reason.contains("headroom"));
        }
    }

    /// The same refusal, for a window with no calibration record at all rather
    /// than an unhealthy one.
    #[test]
    fn refusal_3_missing_calibration_record_refuses_distinctly_from_unhealthy() {
        let mut inputs = worked_example_inputs();
        inputs
            .window_calibrations
            .remove(&WindowSemanticKey::new("model-x:weekly"));
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        let fact = refused
            .missing
            .iter()
            .find(|fact| fact.subject == "model-x:weekly")
            .expect("expected a missing fact for the uncalibrated window");
        assert!(fact.reason.contains("no calibration is recorded"));
    }

    /// Unit, refusal 4 of 7: a cost model missing a token class refuses and
    /// names the missing class, never falling back to an API-list-price
    /// conversion (one of the five forbidden substitutions; this join has no
    /// rate-card or list-price type in scope to fall back to at all).
    #[test]
    fn refusal_4_cost_model_missing_token_class_refuses_and_names_it() {
        let mut inputs = worked_example_inputs();
        inputs.cost_model_missing_token_classes = vec!["cache_read".to_string()];
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        let fact = refused
            .missing
            .iter()
            .find(|fact| fact.subject == "cost_model")
            .expect("expected a cost_model missing fact");
        assert!(fact.reason.contains("cache_read"));
    }

    /// Unit, refusal 5 of 7: a plan tier mismatch refuses rather than silently
    /// applying a different plan tier's calibration (one of the five forbidden
    /// substitutions).
    #[test]
    fn refusal_5_plan_tier_mismatch_refuses_rather_than_substituting_calibration() {
        let mut inputs = worked_example_inputs();
        inputs.plan_tier_mismatch = Some(PlanTierMismatch {
            account_plan_tier: "tier-1".to_string(),
            calibration_plan_tier: "tier-2".to_string(),
        });
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        let fact = refused
            .missing
            .iter()
            .find(|fact| fact.subject == "plan_tier")
            .expect("expected a plan_tier missing fact");
        assert!(fact.reason.contains("tier-1"));
        assert!(fact.reason.contains("tier-2"));
    }

    /// Unit, refusal 6 of 7: too few historical tasks refuses as
    /// INSUFFICIENT_EVIDENCE when it is the only missing prerequisite, never
    /// substituting a global average task cost (one of the five forbidden
    /// substitutions: the refused report carries no `task_evidence` field at
    /// all, by the outcome enum's own construction).
    #[test]
    fn refusal_6_too_few_historical_tasks_refuses_as_insufficient_evidence() {
        let mut inputs = worked_example_inputs();
        inputs.task = TaskReferenceInput {
            verdict: DistributionVerdict::InsufficientEvidence { min_samples: 12 },
            sample_count: 3,
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.verdict, CanRunVerdictLabel::InsufficientEvidence);
        assert_eq!(refused.missing.len(), 1);
        assert_eq!(refused.missing[0].subject, "historical_task_evidence");
        assert!(refused.missing[0].reason.contains("12"));
        assert!(refused.missing[0].reason.contains('3'));

        let text = crate::presentation::render::render_can_run_report(&report);
        assert!(!text.contains("median"), "{text}");
        assert!(!text.contains("credits"), "{text}");
    }

    /// Unit, refusal 6 of 7, forbidden substitution: a group whose eligible
    /// samples are all excluded as estimated-token sessions is indistinguishable
    /// from any other too-thin history, and refuses rather than reporting an
    /// estimated-token session as though it were measured.
    #[test]
    fn refusal_6_estimated_token_only_history_refuses_rather_than_reporting_estimates() {
        let mut inputs = worked_example_inputs();
        inputs.task = TaskReferenceInput {
            verdict: DistributionVerdict::InsufficientEvidence { min_samples: 12 },
            sample_count: 0,
        };
        inputs.attribution_exclusions = ExclusionCounts {
            estimated_tokens: 5,
            ..ExclusionCounts::default()
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.verdict, CanRunVerdictLabel::InsufficientEvidence);
    }

    /// Unit, refusal 7 of 7: task records mostly unattributable refuses with
    /// full evidence fields, in human and (elsewhere, contract-tested) JSON
    /// form.
    #[test]
    fn refusal_7_attribution_below_floor_refuses_with_full_evidence() {
        let mut inputs = worked_example_inputs();
        inputs.attribution = AttributionCoverage {
            fraction: AttributionFraction::new(40, 100),
            floor: AttributionQualityFloor::new(0.80).unwrap(),
        };
        inputs.attribution_exclusions = ExclusionCounts {
            unknown_account_attribution: 9,
            ..ExclusionCounts::default()
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        let fact = refused
            .missing
            .iter()
            .find(|fact| fact.subject == "attribution_quality")
            .expect("expected an attribution_quality missing fact");
        assert_eq!(fact.reason, "attribution_coverage_below_floor");
        let evidence = refused
            .attribution_quality
            .as_ref()
            .expect("attribution evidence travels with the refusal");
        assert_eq!(evidence.numerator_micros, 40);
        assert_eq!(evidence.denominator_micros, 100);
        assert_eq!(evidence.unknown_account_attribution, 9);
        assert_eq!(evidence.selection_window, "2026-08-01..2026-09-01");
        assert_eq!(evidence.group, "refactor-module");
    }

    /// Integration: a single invocation lists every applicable refusal at
    /// once, rather than one per run.
    #[test]
    fn integration_a_single_invocation_lists_every_applicable_refusal() {
        let mut inputs = worked_example_inputs();
        inputs.cost_model_missing_token_classes = vec!["cache_write".to_string()];
        inputs.plan_tier_mismatch = Some(PlanTierMismatch {
            account_plan_tier: "tier-1".to_string(),
            calibration_plan_tier: "tier-2".to_string(),
        });
        inputs.attribution = AttributionCoverage {
            fraction: AttributionFraction::new(10, 100),
            floor: AttributionQualityFloor::new(0.80).unwrap(),
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        let subjects: Vec<&str> = refused
            .missing
            .iter()
            .map(|fact| fact.subject.as_str())
            .collect();
        assert!(subjects.contains(&"cost_model"), "{subjects:?}");
        assert!(subjects.contains(&"plan_tier"), "{subjects:?}");
        assert!(subjects.contains(&"attribution_quality"), "{subjects:?}");
        assert_eq!(refused.verdict, CanRunVerdictLabel::Unknown);
    }

    /// Integration: a new constraining window without applicable calibration
    /// refuses quantitative advice, naming that window, while an unrelated
    /// complete calibrated window remains present in the input but cannot hide
    /// the missing one.
    #[test]
    fn integration_new_uncalibrated_window_refuses_and_names_it_precisely() {
        let mut inputs = worked_example_inputs();
        inputs
            .window_calibrations
            .remove(&WindowSemanticKey::new("account:5h"));
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.missing.len(), 1);
        assert_eq!(refused.missing[0].subject, "account:5h");
    }

    /// No constraining window at all for the selected model refuses, naming
    /// the model.
    #[test]
    fn no_constraining_windows_for_the_model_refuses_and_names_the_model() {
        let mut inputs = worked_example_inputs();
        inputs.meter = CanRunMeterReadiness::Fresh {
            windows: vec![make_window(
                "other:5h",
                WindowScope::ModelSpecific(ModelId::new("some-other-model")),
                100_000,
                1_000_000_000,
            )],
            observed_age: Some(MonotonicDuration::from_seconds(1)),
        };
        let report = compose_can_run_report(inputs);
        let refused = refused_of(&report);
        assert_eq!(refused.missing.len(), 1);
        assert_eq!(refused.missing[0].subject, "constraining_windows");
        assert!(refused.missing[0].reason.contains("model-x"));
    }

    /// A reset timestamp may be displayed, but no output ever reasons about
    /// whether the proposed task finishes before or after it (PLAN.md 26.8):
    /// run duration belongs to a separate friction ledger this report never
    /// touches.
    #[test]
    fn a_reset_timestamp_never_carries_a_before_or_after_claim() {
        let report = compose_can_run_report(worked_example_inputs());
        let text = crate::presentation::render::render_can_run_report(&report).to_lowercase();
        assert!(text.contains("resets"), "{text}");
        for forbidden in [
            "before",
            "after",
            "finish",
            "in time",
            "will cross",
            "won't cross",
        ] {
            assert!(!text.contains(forbidden), "found {forbidden:?} in: {text}");
        }
    }
}
