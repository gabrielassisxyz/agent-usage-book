//! The calibration health state machine.
//!
//! The temptation with an ageing calibration is to widen its uncertainty band.
//! The design refuses that unless an actual statistical model supports the
//! widening: an interval invented because a number is old is not evidence about
//! anything, and a decision made from it is a decision made from a guess with
//! error bars. So health is a *state*, not a widened interval, and nothing here
//! ever returns or mutates an uncertainty interval.
//!
//! A calibration leaves [`CalibrationHealth::Current`] for a specific, checkable
//! reason (PLAN.md 23.9):
//!
//! - the plan tier or the meter/billing semantics it was fitted against no
//!   longer match the environment ([`Inapplicable`]);
//! - a supersession lifecycle event, or the supersession of the cost model it
//!   references, retired it ([`Superseded`]);
//! - it was never activated ([`Provisional`]);
//! - passive validation produced a statistically significant drift finding
//!   ([`Suspect`]);
//! - the configured review horizon passed ([`ReviewDue`]).
//!
//! An adapter *implementation* upgrade never invalidates a calibration; only a
//! change to a semantic identifier does (PLAN.md 7.7). That guarantee is
//! structural here: [`compute_health`] takes semantic identifiers and takes no
//! adapter or endpoint-schema version at all, so an implementation upgrade
//! cannot reach it.
//!
//! [`Inapplicable`]: CalibrationHealth::Inapplicable
//! [`Superseded`]: CalibrationHealth::Superseded
//! [`Provisional`]: CalibrationHealth::Provisional
//! [`Suspect`]: CalibrationHealth::Suspect
//! [`ReviewDue`]: CalibrationHealth::ReviewDue

use std::fmt;

use crate::domain::ids::{BillingSemanticsId, MeterSemanticsId};
use crate::domain::time::UtcTimestamp;
use crate::store::calibration::PlanTier;

/// The health of one calibration. Six states, ordered here from least to most
/// usable; only [`Current`](Self::Current) powers a routing recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalibrationHealth {
    /// Fitted but never activated. It may be displayed; it is not authoritative.
    Provisional,
    /// Activated, applicable, no drift finding, within its review horizon.
    Current,
    /// The configured review horizon passed. Still displayable, no longer a
    /// silent routing input.
    ReviewDue,
    /// Passive validation produced a statistically significant drift finding.
    Suspect,
    /// A supersession event, or a supersession of the referenced cost model,
    /// retired it.
    Superseded,
    /// The plan tier or the meter/billing semantics no longer match the
    /// environment: the calibration describes a physical situation that no
    /// longer holds.
    Inapplicable,
}

impl CalibrationHealth {
    /// A stable lower-case label, so `calibrate show` can name a non-current
    /// calibration's state rather than hiding it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Current => "current",
            Self::ReviewDue => "review_due",
            Self::Suspect => "suspect",
            Self::Superseded => "superseded",
            Self::Inapplicable => "inapplicable",
        }
    }
}

/// Where a calibration sits in its activation lifecycle, derived from the
/// ordered `calibration_lifecycle_event` rows (`CalibrationEventKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// No activation event: still a candidate.
    NeverActivated,
    /// Activated and not since superseded.
    Active,
    /// A supersession event followed activation.
    Superseded,
}

/// A statistically significant drift finding from passive validation. Carries
/// the identity of the finding, never a magnitude to widen a band by: its
/// existence is what moves the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignificantDrift {
    /// The validation run or finding this came from, for the operator to chase.
    pub finding_id: String,
}

/// What the environment's semantics are *now*, to compare against what the
/// calibration was fitted against. Deliberately no adapter or endpoint-schema
/// version: applicability is decided against physical and billing semantics
/// only (PLAN.md 7.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicabilityContext {
    pub plan_tier: PlanTier,
    pub meter_semantics_id: MeterSemanticsId,
    pub billing_semantics_id: BillingSemanticsId,
}

/// The applicability-relevant identity of one calibration: the plan tier and
/// the meter and billing semantic identifiers it was fitted against. The caller
/// extracts these from a stored `WindowCalibration`, so the health logic stays a
/// pure function of typed facts and does not depend on the calibration record's
/// storage shape. (`PlanTier` is defined alongside the calibration tables in
/// `src/store/calibration.rs`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationFacts {
    pub plan_tier: PlanTier,
    pub meter_semantics_id: MeterSemanticsId,
    pub billing_semantics_id: BillingSemanticsId,
}

/// Everything [`compute_health`] needs beyond the wall clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthInputs<'a> {
    pub calibration: &'a CalibrationFacts,
    pub context: &'a ApplicabilityContext,
    pub lifecycle: LifecycleState,
    /// The referenced cost model has a supersession recorded against it.
    pub cost_model_superseded: bool,
    /// A statistically significant drift finding, if passive validation made one.
    pub drift: Option<&'a SignificantDrift>,
    /// The instant the review horizon passes, if one is configured. Computed by
    /// the caller from the fit time and the configured horizon.
    pub review_due_at: Option<UtcTimestamp>,
}

/// The health of a calibration given the environment and the evidence.
///
/// The checks run in a fixed order of precedence, because more than one can hold
/// at once and the most fundamental fact should win: an inapplicable calibration
/// is inapplicable whatever its age, and a superseded one is retired whether or
/// not the tier also drifted. The order is
/// `Inapplicable > Superseded > Provisional > Suspect > ReviewDue > Current`.
///
/// Every transition away from `Current` is triggered by a named condition rather
/// than by elapsed time alone; the one time condition, the review horizon, is
/// explicit in `review_due_at`.
pub fn compute_health(inputs: &HealthInputs<'_>, now: UtcTimestamp) -> CalibrationHealth {
    if !applies(inputs.calibration, inputs.context) {
        return CalibrationHealth::Inapplicable;
    }
    if inputs.lifecycle == LifecycleState::Superseded || inputs.cost_model_superseded {
        return CalibrationHealth::Superseded;
    }
    if inputs.lifecycle == LifecycleState::NeverActivated {
        return CalibrationHealth::Provisional;
    }
    if inputs.drift.is_some() {
        return CalibrationHealth::Suspect;
    }
    if let Some(due) = inputs.review_due_at
        && now.unix_nanos() >= due.unix_nanos()
    {
        return CalibrationHealth::ReviewDue;
    }
    CalibrationHealth::Current
}

/// Whether the calibration's fitted scope still matches the environment. A
/// difference in the plan tier or in either semantic identifier makes it
/// inapplicable; nothing else here is an applicability question.
fn applies(calibration: &CalibrationFacts, context: &ApplicabilityContext) -> bool {
    calibration.plan_tier == context.plan_tier
        && calibration.meter_semantics_id == context.meter_semantics_id
        && calibration.billing_semantics_id == context.billing_semantics_id
}

/// A calibration that is not a `Current` applicable one, so a quantitative
/// consumer refuses a recommendation and says which state it found (PLAN.md
/// 23.9, 26.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotCurrentApplicable {
    pub health: CalibrationHealth,
}

impl fmt::Display for NotCurrentApplicable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "calibration is {} rather than current-applicable; it does not power a routing recommendation",
            self.health.label()
        )
    }
}

impl std::error::Error for NotCurrentApplicable {}

/// The decision a later advisory consumer (`aub-cab.4`) makes before routing:
/// only a `Current` calibration is authoritative. `Provisional`, `ReviewDue`,
/// `Suspect`, `Superseded` and `Inapplicable` are all refused, so an ageing or
/// mismatched calibration never silently drives a recommendation.
pub fn require_current_applicable(health: CalibrationHealth) -> Result<(), NotCurrentApplicable> {
    match health {
        CalibrationHealth::Current => Ok(()),
        CalibrationHealth::Provisional
        | CalibrationHealth::ReviewDue
        | CalibrationHealth::Suspect
        | CalibrationHealth::Superseded
        | CalibrationHealth::Inapplicable => Err(NotCurrentApplicable { health }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> CalibrationFacts {
        CalibrationFacts {
            plan_tier: PlanTier::new("pro-5h"),
            meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
            billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
        }
    }

    fn matching_context() -> ApplicabilityContext {
        ApplicabilityContext {
            plan_tier: PlanTier::new("pro-5h"),
            meter_semantics_id: MeterSemanticsId::new("account-5h-v2"),
            billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v4"),
        }
    }

    fn healthy_inputs<'a>(
        facts: &'a CalibrationFacts,
        context: &'a ApplicabilityContext,
    ) -> HealthInputs<'a> {
        HealthInputs {
            calibration: facts,
            context,
            lifecycle: LifecycleState::Active,
            cost_model_superseded: false,
            drift: None,
            review_due_at: None,
        }
    }

    fn now_ts() -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(1_000_000)
    }

    #[test]
    fn an_activated_applicable_calibration_with_no_findings_is_current() {
        let (facts, context) = (facts(), matching_context());
        assert_eq!(
            compute_health(&healthy_inputs(&facts, &context), now_ts()),
            CalibrationHealth::Current
        );
    }

    #[test]
    fn a_plan_tier_change_makes_it_inapplicable() {
        let facts = facts();
        let context = ApplicabilityContext {
            plan_tier: PlanTier::new("max-5h"),
            ..matching_context()
        };
        assert_eq!(
            compute_health(&healthy_inputs(&facts, &context), now_ts()),
            CalibrationHealth::Inapplicable
        );
    }

    #[test]
    fn a_meter_or_billing_semantics_change_makes_it_inapplicable() {
        let facts = facts();
        for context in [
            ApplicabilityContext {
                meter_semantics_id: MeterSemanticsId::new("account-5h-v3"),
                ..matching_context()
            },
            ApplicabilityContext {
                billing_semantics_id: BillingSemanticsId::new("model-x-subscription-v5"),
                ..matching_context()
            },
        ] {
            assert_eq!(
                compute_health(&healthy_inputs(&facts, &context), now_ts()),
                CalibrationHealth::Inapplicable
            );
        }
    }

    #[test]
    fn a_cost_model_supersession_makes_it_superseded() {
        let (facts, context) = (facts(), matching_context());
        let inputs = HealthInputs {
            cost_model_superseded: true,
            ..healthy_inputs(&facts, &context)
        };
        assert_eq!(
            compute_health(&inputs, now_ts()),
            CalibrationHealth::Superseded
        );
    }

    #[test]
    fn a_supersession_event_makes_it_superseded() {
        let (facts, context) = (facts(), matching_context());
        let inputs = HealthInputs {
            lifecycle: LifecycleState::Superseded,
            ..healthy_inputs(&facts, &context)
        };
        assert_eq!(
            compute_health(&inputs, now_ts()),
            CalibrationHealth::Superseded
        );
    }

    #[test]
    fn a_calibration_that_was_never_activated_is_provisional() {
        let (facts, context) = (facts(), matching_context());
        let inputs = HealthInputs {
            lifecycle: LifecycleState::NeverActivated,
            ..healthy_inputs(&facts, &context)
        };
        assert_eq!(
            compute_health(&inputs, now_ts()),
            CalibrationHealth::Provisional
        );
    }

    #[test]
    fn a_significant_drift_finding_makes_it_suspect() {
        let (facts, context) = (facts(), matching_context());
        let drift = SignificantDrift {
            finding_id: "validation-2026-09".into(),
        };
        let inputs = HealthInputs {
            drift: Some(&drift),
            ..healthy_inputs(&facts, &context)
        };
        assert_eq!(
            compute_health(&inputs, now_ts()),
            CalibrationHealth::Suspect
        );
    }

    #[test]
    fn a_passed_review_horizon_makes_it_review_due_and_a_future_one_does_not() {
        let (facts, context) = (facts(), matching_context());

        let passed = HealthInputs {
            review_due_at: Some(UtcTimestamp::from_unix_nanos(now_ts().unix_nanos() - 1)),
            ..healthy_inputs(&facts, &context)
        };
        assert_eq!(
            compute_health(&passed, now_ts()),
            CalibrationHealth::ReviewDue
        );

        let future = HealthInputs {
            review_due_at: Some(UtcTimestamp::from_unix_nanos(now_ts().unix_nanos() + 1)),
            ..healthy_inputs(&facts, &context)
        };
        assert_eq!(
            compute_health(&future, now_ts()),
            CalibrationHealth::Current
        );
    }

    /// The negative that keeps the semantic-identifier rule honest: an adapter
    /// implementation upgrade changes no input this function can see, because it
    /// takes no adapter or endpoint-schema version. Modelled as the semantic
    /// identifiers staying identical while the world moves on: the state does
    /// not move. An implementation that keyed applicability on a version number
    /// could not be written against this signature.
    #[test]
    fn an_adapter_implementation_upgrade_does_not_move_the_state() {
        let (facts, context) = (facts(), matching_context());
        let before = compute_health(&healthy_inputs(&facts, &context), now_ts());

        // The adapter is rewritten; the physical and billing semantics it
        // reports are unchanged, so the only inputs that exist are unchanged.
        let after = compute_health(&healthy_inputs(&facts, &context), now_ts());

        assert_eq!(before, after);
        assert_eq!(before, CalibrationHealth::Current);
    }

    #[test]
    fn precedence_inapplicable_beats_superseded_beats_provisional_beats_suspect_beats_review_due() {
        let facts = facts();
        let mismatch = ApplicabilityContext {
            plan_tier: PlanTier::new("other"),
            ..matching_context()
        };
        let drift = SignificantDrift {
            finding_id: "d".into(),
        };
        let everything_wrong = HealthInputs {
            calibration: &facts,
            context: &mismatch,
            lifecycle: LifecycleState::Superseded,
            cost_model_superseded: true,
            drift: Some(&drift),
            review_due_at: Some(UtcTimestamp::from_unix_nanos(0)),
        };
        assert_eq!(
            compute_health(&everything_wrong, now_ts()),
            CalibrationHealth::Inapplicable
        );

        let context = matching_context();
        let superseded_and_more = HealthInputs {
            context: &context,
            ..everything_wrong
        };
        assert_eq!(
            compute_health(&superseded_and_more, now_ts()),
            CalibrationHealth::Superseded
        );

        let provisional_and_more = HealthInputs {
            lifecycle: LifecycleState::NeverActivated,
            cost_model_superseded: false,
            ..superseded_and_more
        };
        assert_eq!(
            compute_health(&provisional_and_more, now_ts()),
            CalibrationHealth::Provisional
        );

        let suspect_and_review_due = HealthInputs {
            lifecycle: LifecycleState::Active,
            ..provisional_and_more
        };
        assert_eq!(
            compute_health(&suspect_and_review_due, now_ts()),
            CalibrationHealth::Suspect
        );
    }

    #[test]
    fn require_current_applicable_accepts_only_current() {
        assert!(require_current_applicable(CalibrationHealth::Current).is_ok());
        for refused in [
            CalibrationHealth::Provisional,
            CalibrationHealth::ReviewDue,
            CalibrationHealth::Suspect,
            CalibrationHealth::Superseded,
            CalibrationHealth::Inapplicable,
        ] {
            let err = require_current_applicable(refused).unwrap_err();
            assert_eq!(err.health, refused);
            assert!(err.to_string().contains(refused.label()));
        }
    }

    #[test]
    fn the_six_state_labels_are_distinct() {
        let labels = [
            CalibrationHealth::Provisional,
            CalibrationHealth::Current,
            CalibrationHealth::ReviewDue,
            CalibrationHealth::Suspect,
            CalibrationHealth::Superseded,
            CalibrationHealth::Inapplicable,
        ]
        .map(CalibrationHealth::label);
        let unique: std::collections::BTreeSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), 6);
    }

    /// Ageing produces a state, never a changed interval: `compute_health` has
    /// no way to return or mutate an uncertainty interval, and over a range of
    /// review horizons it only ever crosses from `Current` to `ReviewDue`.
    #[test]
    fn ageing_only_ever_transitions_the_state_and_never_touches_an_interval() {
        let (facts, context) = (facts(), matching_context());
        for offset_ns in [-3_000i64, -1, 0, 1, 5_000] {
            let due = UtcTimestamp::from_unix_nanos(now_ts().unix_nanos() + offset_ns);
            let inputs = HealthInputs {
                review_due_at: Some(due),
                ..healthy_inputs(&facts, &context)
            };
            let health = compute_health(&inputs, now_ts());
            let expected = if now_ts().unix_nanos() >= due.unix_nanos() {
                CalibrationHealth::ReviewDue
            } else {
                CalibrationHealth::Current
            };
            assert_eq!(health, expected, "offset {offset_ns}");
        }
    }
}
