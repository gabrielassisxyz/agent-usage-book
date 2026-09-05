//! Conversion from qualified subscription credits to calibrated window movement.
//!
//! A conversion is valid only when the caller supplies the immutable
//! [`WindowCalibration`] witness and the current semantic context agrees with
//! that witness. The result keeps both percentage-point endpoints: a point
//! coefficient is useful for arithmetic, but printing it as a point estimate
//! would hide calibration uncertainty.

use std::collections::BTreeSet;

use crate::domain::credits::Credits;
use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
use crate::domain::interval::Interval;
use crate::domain::provenance::CostModelId;
use crate::domain::quota::PercentagePoints;
use crate::domain::window::WindowSemanticKey;
use crate::evidence::{Derivation, EstimatorId, EvidenceQuality, Provenance, RequiredFact};
use crate::report::{WindowEquivalentDerivation, WindowEquivalentValue};
use crate::store::calibration::{PlanTier, WindowCalibration};
use crate::store::cost_model::ProviderKey;

use super::health::CalibrationHealth;

/// The current facts against which one stored calibration is checked.
///
/// Account identity is part of the report stratum even though the calibration
/// record itself is scoped by provider, plan and window. A missing account is
/// therefore an explicit refusal rather than permission to combine accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowConversionContext {
    pub account: Option<String>,
    pub provider: ProviderKey,
    pub plan_tier: PlanTier,
    pub window_semantic_key: WindowSemanticKey,
    pub meter_semantics_id: MeterSemanticsId,
    pub billing_semantics_id: BillingSemanticsId,
    pub cost_model_id: Option<CostModelId>,
}

impl WindowConversionContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        account: Option<String>,
        provider: ProviderKey,
        plan_tier: PlanTier,
        window_semantic_key: WindowSemanticKey,
        meter_semantics_id: MeterSemanticsId,
        billing_semantics_id: BillingSemanticsId,
        cost_model_id: Option<CostModelId>,
    ) -> Self {
        Self {
            account,
            provider,
            plan_tier,
            window_semantic_key,
            meter_semantics_id,
            billing_semantics_id,
            cost_model_id,
        }
    }
}

/// Converts one qualified credit derivation into percentage-point movement.
///
/// `calibration` is deliberately an explicit parameter: callers cannot obtain a
/// conversion by reaching for a global coefficient or by passing only its raw
/// numeric fields. `health` is evaluated by the repository-facing caller from
/// the same current context; every state other than `Current` is refused.
pub fn convert(
    credits: &Derivation<Credits>,
    calibration: &WindowCalibration,
    context: &WindowConversionContext,
    health: CalibrationHealth,
) -> WindowEquivalentDerivation {
    let provenance = conversion_provenance(credits.provenance(), calibration);
    let mismatches = applicability_mismatches(calibration, context, health);
    if !mismatches.is_empty() {
        return unavailable(mismatches, provenance);
    }

    let Derivation::Available(qualified) = credits else {
        let mut missing = credits.missing().cloned().unwrap_or_default();
        missing.insert(RequiredFact::new("qualified credits"));
        return unavailable(missing, provenance);
    };

    let (credits_value, coverage, credit_quality, _) = qualified.clone().into_parts();
    let Some((credit_lower, credit_upper)) = credit_bounds(credits_value, &credit_quality) else {
        return unavailable(
            [RequiredFact::new("non-negative qualified credits interval")],
            provenance,
        );
    };
    let coefficient = calibration.uncertainty();
    let coefficient_lower = i128::from(coefficient.lower().micros_per_point());
    let coefficient_upper = i128::from(coefficient.upper().micros_per_point());
    if coefficient_lower <= 0 || coefficient_upper <= 0 {
        return unavailable(
            [RequiredFact::new("positive calibration coefficient")],
            provenance,
        );
    }

    // Movement is credits divided by credits-per-native-point. The lower endpoint
    // uses the largest coefficient and the upper endpoint the smallest coefficient;
    // floor/ceiling preserve every integer percentage-point value the evidence admits.
    let lower_raw = divide_floor(credit_lower, coefficient_upper);
    let upper_raw = divide_ceil(credit_upper, coefficient_lower);
    if lower_raw < i128::from(PercentagePoints::MIN)
        || upper_raw > i128::from(PercentagePoints::MAX)
    {
        return unavailable(
            [RequiredFact::new(
                "percentage-point interval within representable range",
            )],
            provenance,
        );
    }
    let lower = PercentagePoints::new(lower_raw as i32).expect("range checked above");
    let upper = PercentagePoints::new(upper_raw as i32).expect("range checked above");
    let interval = Interval::new(lower, upper).expect("inverse bounds are ordered");
    let quality = lift_quality(&credit_quality, interval, calibration);

    WindowEquivalentDerivation::Available(WindowEquivalentValue {
        interval,
        calibration_id: calibration.id().clone(),
        coverage,
        quality,
        provenance,
    })
}

fn applicability_mismatches(
    calibration: &WindowCalibration,
    context: &WindowConversionContext,
    health: CalibrationHealth,
) -> BTreeSet<RequiredFact> {
    let mut missing = BTreeSet::new();
    if context
        .account
        .as_deref()
        .is_none_or(|account| account.is_empty() || account == crate::report::UNKNOWN_ACCOUNT_LABEL)
    {
        missing.insert(RequiredFact::new("account attribution"));
    }
    if calibration.provider() != &context.provider {
        missing.insert(RequiredFact::new("provider matches calibration"));
    }
    if calibration.plan_tier() != &context.plan_tier {
        missing.insert(RequiredFact::new("plan tier matches calibration"));
    }
    if calibration.window_semantic_key() != &context.window_semantic_key {
        missing.insert(RequiredFact::new("window semantic key matches calibration"));
    }
    if calibration.meter_semantics_id() != &context.meter_semantics_id {
        missing.insert(RequiredFact::new("meter semantics match calibration"));
    }
    if calibration.billing_semantics_id() != &context.billing_semantics_id {
        missing.insert(RequiredFact::new("billing semantics match calibration"));
    }
    if context.cost_model_id.as_ref() != Some(calibration.cost_model_id()) {
        missing.insert(RequiredFact::new("cost model matches calibration"));
    }
    if health != CalibrationHealth::Current {
        missing.insert(RequiredFact::new(format!(
            "calibration health: {}",
            health.label()
        )));
    }
    missing
}

fn credit_bounds(value: Credits, quality: &EvidenceQuality<Credits>) -> Option<(i128, i128)> {
    let value = i128::from(value.micros());
    let (quality_lower, quality_upper) = quality
        .uncertainty()
        .map(|interval| {
            (
                i128::from(interval.lower().micros()),
                i128::from(interval.upper().micros()),
            )
        })
        .unwrap_or((value, value));
    let lower = value.min(quality_lower);
    let upper = value.max(quality_upper);
    (lower >= 0 && upper >= lower).then_some((lower, upper))
}

fn divide_floor(numerator: i128, denominator: i128) -> i128 {
    numerator / denominator
}

fn divide_ceil(numerator: i128, denominator: i128) -> i128 {
    if numerator == 0 {
        0
    } else {
        (numerator + denominator - 1) / denominator
    }
}

fn lift_quality(
    credit_quality: &EvidenceQuality<Credits>,
    interval: Interval<PercentagePoints>,
    calibration: &WindowCalibration,
) -> EvidenceQuality<PercentagePoints> {
    let mut methods = credit_quality.methods().clone();
    methods.insert(EstimatorId::new(format!(
        "window-calibration:{}",
        calibration.id().as_str()
    )));
    match credit_quality {
        EvidenceQuality::Measured | EvidenceQuality::Estimated { .. } => {
            EvidenceQuality::Estimated {
                methods,
                uncertainty: Some(interval),
            }
        }
        EvidenceQuality::Mixed { .. } => EvidenceQuality::Mixed {
            methods,
            uncertainty: Some(interval),
        },
    }
}

fn conversion_provenance(
    credits_provenance: &Provenance,
    calibration: &WindowCalibration,
) -> Provenance {
    let mut sources = credits_provenance.sources().clone();
    sources.insert(format!("window-calibration:{}", calibration.id().as_str()));
    sources.insert(format!("provider:{}", calibration.provider().as_str()));
    Provenance::new(sources)
}

fn unavailable(
    missing: impl IntoIterator<Item = RequiredFact>,
    provenance: Provenance,
) -> WindowEquivalentDerivation {
    WindowEquivalentDerivation::unavailable(missing, provenance)
        .expect("every conversion refusal names at least one missing fact")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::health::CalibrationHealth;
    use crate::domain::provenance::WindowCalibrationId;
    use crate::domain::time::UtcTimestamp;
    use crate::evidence::{CoverageCompleteness, Qualified};
    use crate::store::calibration::CalibrationScope;

    fn calibration_for(id: &str, plan_tier: &str, micros_per_point: i64) -> WindowCalibration {
        let scope = CalibrationScope {
            provider: ProviderKey::new("anthropic"),
            plan_tier: PlanTier::new(plan_tier),
            window_semantic_key: WindowSemanticKey::new("five_hour"),
        };
        let meter = MeterSemanticsId::new("meter-v1");
        let billing = BillingSemanticsId::new("billing-v1");
        let validity = crate::store::cost_model::ValidityInterval::new(
            UtcTimestamp::from_unix_nanos(0),
            UtcTimestamp::from_unix_nanos(i64::MAX),
        )
        .unwrap();
        crate::store::calibration::minimal_fixture(
            id,
            &scope,
            &meter,
            &billing,
            &CostModelId::new("cost-v1"),
            crate::domain::credits::CreditsPerPercentagePoint::from_micros_per_point(
                micros_per_point,
            ),
            validity,
            UtcTimestamp::from_unix_nanos(1),
        )
    }

    fn calibration() -> WindowCalibration {
        calibration_for("calibration-v1", "pro", 100_000)
    }

    fn context() -> WindowConversionContext {
        WindowConversionContext::new(
            Some("work".to_string()),
            ProviderKey::new("anthropic"),
            PlanTier::new("pro"),
            WindowSemanticKey::new("five_hour"),
            MeterSemanticsId::new("meter-v1"),
            BillingSemanticsId::new("billing-v1"),
            Some(CostModelId::new("cost-v1")),
        )
    }

    fn credits(micros: i64) -> Derivation<Credits> {
        Derivation::Available(Qualified::new(
            Credits::from_micros(micros),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
            Provenance::new(["cost-model:cost-v1".to_string()]),
        ))
    }

    fn missing(result: &WindowEquivalentDerivation) -> Vec<String> {
        result
            .missing()
            .unwrap()
            .iter()
            .map(|fact| fact.as_str().to_string())
            .collect()
    }

    #[test]
    fn converts_qualified_credits_to_a_bounded_percentage_point_interval() {
        let result = convert(
            &credits(10_000_000),
            &calibration(),
            &context(),
            CalibrationHealth::Current,
        );
        let WindowEquivalentDerivation::Available(value) = result else {
            panic!("matching calibration should convert")
        };
        assert_eq!(value.interval.lower().get(), 100);
        assert_eq!(value.interval.upper().get(), 100);
        assert_eq!(
            value.calibration_id,
            WindowCalibrationId::new("calibration-v1")
        );
        assert_eq!(value.coverage, CoverageCompleteness::Complete);
        assert!(matches!(value.quality, EvidenceQuality::Estimated { .. }));
        assert!(
            value
                .provenance
                .sources()
                .contains("window-calibration:calibration-v1")
        );
    }

    #[test]
    fn every_applicability_dimension_is_checked_independently() {
        let cases = [
            (
                "account attribution",
                WindowConversionContext {
                    account: None,
                    ..context()
                },
            ),
            (
                "account attribution",
                WindowConversionContext {
                    account: Some(crate::report::UNKNOWN_ACCOUNT_LABEL.to_string()),
                    ..context()
                },
            ),
            (
                "provider matches calibration",
                WindowConversionContext {
                    provider: ProviderKey::new("openai"),
                    ..context()
                },
            ),
            (
                "plan tier matches calibration",
                WindowConversionContext {
                    plan_tier: PlanTier::new("max"),
                    ..context()
                },
            ),
            (
                "window semantic key matches calibration",
                WindowConversionContext {
                    window_semantic_key: WindowSemanticKey::new("seven_day"),
                    ..context()
                },
            ),
            (
                "meter semantics match calibration",
                WindowConversionContext {
                    meter_semantics_id: MeterSemanticsId::new("meter-v2"),
                    ..context()
                },
            ),
            (
                "billing semantics match calibration",
                WindowConversionContext {
                    billing_semantics_id: BillingSemanticsId::new("billing-v2"),
                    ..context()
                },
            ),
        ];
        for (expected, mismatched) in cases {
            let result = convert(
                &credits(10_000_000),
                &calibration(),
                &mismatched,
                CalibrationHealth::Current,
            );
            assert!(
                missing(&result).iter().any(|fact| fact == expected),
                "{expected}"
            );
        }
    }

    #[test]
    fn non_current_health_states_refuse_without_erasing_credit_evidence() {
        for health in [
            CalibrationHealth::Provisional,
            CalibrationHealth::ReviewDue,
            CalibrationHealth::Suspect,
            CalibrationHealth::Superseded,
            CalibrationHealth::Inapplicable,
        ] {
            let result = convert(&credits(10_000_000), &calibration(), &context(), health);
            let facts = missing(&result);
            assert!(
                facts
                    .iter()
                    .any(|fact| fact == &format!("calibration health: {}", health.label()))
            );
            assert!(result.provenance().sources().contains("cost-model:cost-v1"));
        }
    }

    #[test]
    fn unavailable_credits_keep_their_reason_in_the_window_refusal() {
        let credits = Derivation::unavailable(
            [RequiredFact::new("unknown component: tool_use_tokens")],
            Provenance::new(["cost-model:cost-v1".to_string()]),
        )
        .unwrap();
        let result = convert(
            &credits,
            &calibration(),
            &context(),
            CalibrationHealth::Current,
        );
        let facts = missing(&result);
        assert!(
            facts
                .iter()
                .any(|fact| fact == "unknown component: tool_use_tokens")
        );
        assert!(facts.iter().any(|fact| fact == "qualified credits"));
    }

    #[test]
    fn mixed_account_and_plan_strata_keep_distinct_calibration_results() {
        let work = convert(
            &credits(10_000_000),
            &calibration_for("calibration-work-pro", "pro", 100_000),
            &context(),
            CalibrationHealth::Current,
        );
        let research = convert(
            &credits(10_000_000),
            &calibration_for("calibration-research-max", "max", 200_000),
            &WindowConversionContext {
                account: Some("research".to_string()),
                plan_tier: PlanTier::new("max"),
                ..context()
            },
            CalibrationHealth::Current,
        );

        let WindowEquivalentDerivation::Available(work) = work else {
            panic!("work stratum should use its applicable calibration")
        };
        let WindowEquivalentDerivation::Available(research) = research else {
            panic!("research stratum should use its applicable calibration")
        };
        assert_ne!(work.calibration_id, research.calibration_id);
        assert_eq!(work.interval.lower().get(), 100);
        assert_eq!(research.interval.lower().get(), 50);
        assert_ne!(work.interval, research.interval);
    }
}
