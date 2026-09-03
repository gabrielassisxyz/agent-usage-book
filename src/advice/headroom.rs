//! Headroom conversion: converts quota windows constraining a model into credit headroom intervals.
//!
//! May not depend on:
//! - constructing calibration constants
//! - presentation
//!
//! Windows are calibrated independently, so remaining percentage is not a common
//! unit across them: comparing percentages across windows compares quantities that
//! are not the same kind of thing.
//!
//! This module converts every constraining provider window into a credit-headroom
//! interval using that window's own applicable current calibration, propagating
//! calibration uncertainty through interval multiplication.
//!
//! An uncalibrated window or one whose calibration is not current produces an
//! explicit unknown evaluation for that window rather than dropping out of the
//! enumeration.

use std::collections::BTreeMap;

use crate::domain::credits::{Credits, CreditsPerPercentagePoint};
use crate::domain::interval::Interval;
use crate::domain::window::{MeterWindow, ModelId, WindowSemanticKey};
use crate::store::calibration::{CoefficientUncertainty, WindowCalibration};

/// The health of one calibration as evaluated by the calibration health state machine.
///
/// Only [`Current`](Self::Current) allows a calibration to power an advisory calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalibrationHealth {
    /// Fitted but never activated.
    Provisional,
    /// Activated, applicable, no drift finding, within its review horizon.
    Current,
    /// The configured review horizon passed.
    ReviewDue,
    /// Passive validation produced a statistically significant drift finding.
    Suspect,
    /// A supersession event or cost model supersession retired it.
    Superseded,
    /// Plan tier or meter/billing semantics no longer match the environment.
    Inapplicable,
}

impl CalibrationHealth {
    /// True only when the calibration is current and applicable.
    pub fn is_current(self) -> bool {
        matches!(self, Self::Current)
    }

    /// Lower-case label describing the health state.
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

/// A calibration record or constraint input supplied for headroom conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibratedWindowConstraint {
    uncertainty: CoefficientUncertainty,
    health: CalibrationHealth,
}

impl CalibratedWindowConstraint {
    /// Constructs a constraint with explicit uncertainty and health.
    pub fn new(uncertainty: CoefficientUncertainty, health: CalibrationHealth) -> Self {
        Self {
            uncertainty,
            health,
        }
    }

    /// Constructs a current constraint with explicit uncertainty bounds.
    pub fn current(uncertainty: CoefficientUncertainty) -> Self {
        Self {
            uncertainty,
            health: CalibrationHealth::Current,
        }
    }

    /// Constructs a current constraint with a single point coefficient (zero-width uncertainty).
    pub fn current_point(fitted: CreditsPerPercentagePoint) -> Self {
        let uncertainty = CoefficientUncertainty::new(fitted, fitted)
            .expect("identical lower and upper bounds are valid");
        Self {
            uncertainty,
            health: CalibrationHealth::Current,
        }
    }

    /// Constructs a constraint from a stored [`WindowCalibration`] witness and its evaluated health.
    pub fn from_calibration(calibration: &WindowCalibration, health: CalibrationHealth) -> Self {
        Self {
            uncertainty: calibration.uncertainty(),
            health,
        }
    }

    pub fn uncertainty(&self) -> CoefficientUncertainty {
        self.uncertainty
    }

    pub fn health(&self) -> CalibrationHealth {
        self.health
    }

    pub fn is_current(&self) -> bool {
        self.health.is_current()
    }
}

/// The result of converting one constraining provider window into credit headroom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowHeadroom<'a> {
    /// The window has an applicable current calibration and produced a bounded credit headroom interval.
    Known {
        window: &'a MeterWindow,
        headroom: Interval<Credits>,
    },
    /// The window lacks an applicable current calibration (missing, uncalibrated, or non-current health).
    Unknown { window: &'a MeterWindow },
}

impl<'a> WindowHeadroom<'a> {
    /// The constraining window this evaluation was computed for.
    pub fn window(&self) -> &'a MeterWindow {
        match self {
            Self::Known { window, .. } | Self::Unknown { window } => window,
        }
    }

    /// The computed headroom interval, if known.
    pub fn headroom(&self) -> Option<Interval<Credits>> {
        match self {
            Self::Known { headroom, .. } => Some(*headroom),
            Self::Unknown { .. } => None,
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }
}

/// Converts a single provider window into a credit headroom interval using its calibration.
///
/// If calibration is absent or not current, produces [`WindowHeadroom::Unknown`].
/// Calibration uncertainty propagates into the headroom interval via interval arithmetic.
pub fn window_credit_headroom<'a>(
    window: &'a MeterWindow,
    calibration: Option<&CalibratedWindowConstraint>,
) -> WindowHeadroom<'a> {
    let Some(cal) = calibration else {
        return WindowHeadroom::Unknown { window };
    };

    if !cal.is_current() {
        return WindowHeadroom::Unknown { window };
    }

    let remaining_points = window.remaining_percentage_points();
    let unc = cal.uncertainty();
    let p1 = unc.lower() * remaining_points;
    let p2 = unc.upper() * remaining_points;
    let lower = p1.min(p2);
    let upper = p1.max(p2);
    let headroom = Interval::new(lower, upper).expect("lower <= upper by construction");

    WindowHeadroom::Known { window, headroom }
}

/// Enumerates every window constraining the selected model and converts each into credit headroom.
///
/// Includes both account-wide and model-specific windows.
/// Each window uses its own applicable current calibration, never another window's.
/// Missing or non-current calibrations produce explicit [`WindowHeadroom::Unknown`] entries
/// rather than skipping constraints.
pub fn convert_constraining_windows<'a>(
    windows: &'a [MeterWindow],
    model: &ModelId,
    calibrations: &BTreeMap<WindowSemanticKey, CalibratedWindowConstraint>,
) -> Vec<WindowHeadroom<'a>> {
    windows
        .iter()
        .filter(|w| w.constrains(model))
        .map(|w| {
            let cal = calibrations.get(w.semantic_key());
            window_credit_headroom(w, cal)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::{QuotaFractionPpm, QuotaUsed};
    use crate::domain::time::UtcTimestamp;
    use crate::domain::window::{
        NominalWindowDuration, QuantizationSemantics, ReportedResolution, WindowScope,
    };
    use proptest::prelude::*;

    fn make_window(key: &str, scope: WindowScope, used_ppm: i32) -> MeterWindow {
        MeterWindow::new(
            WindowSemanticKey::new(key),
            scope,
            QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::RoundedToNearest,
            UtcTimestamp::from_unix_nanos(1_000_000_000),
            NominalWindowDuration::from_nanos(1_000_000_000),
        )
    }

    fn point_calibration(micros_per_point: i64) -> CalibratedWindowConstraint {
        CalibratedWindowConstraint::current_point(CreditsPerPercentagePoint::from_micros_per_point(
            micros_per_point,
        ))
    }

    fn interval_calibration(low_micros: i64, high_micros: i64) -> CalibratedWindowConstraint {
        let unc = CoefficientUncertainty::new(
            CreditsPerPercentagePoint::from_micros_per_point(low_micros),
            CreditsPerPercentagePoint::from_micros_per_point(high_micros),
        )
        .expect("valid uncertainty");
        CalibratedWindowConstraint::current(unc)
    }

    /// The design's worked example (PLAN.md 7.3, 26.3, 26.4):
    /// Window A: 20 percentage points left at 100 credits per point => 2,000 credits.
    /// Window B: 40 percentage points left at 10 credits per point => 400 credits.
    /// Lower-percentage window (Window A, 20%) holds five times the headroom (2,000 credits).
    #[test]
    fn worked_example_produces_expected_headroom_and_lower_percentage_has_larger_headroom() {
        let model = ModelId::new("model-a");
        // Window A: AccountWide, 800_000 used => 200_000 ppm remaining (20 percentage points).
        let window_a = make_window("window-a", WindowScope::AccountWide, 800_000);
        // Window B: ModelSpecific, 600_000 used => 400_000 ppm remaining (40 percentage points).
        let window_b = make_window(
            "window-b",
            WindowScope::ModelSpecific(model.clone()),
            600_000,
        );

        // 100 credits per point (1 percentage point = 10,000 ppm, so 100 credits / 10,000 ppm = 10,000 micros/point).
        // 10 credits per point (10 credits / 10,000 ppm = 1,000 micros/point).
        let calibrations = BTreeMap::from([
            (
                WindowSemanticKey::new("window-a"),
                point_calibration(10_000),
            ),
            (WindowSemanticKey::new("window-b"), point_calibration(1_000)),
        ]);

        let windows = [window_a.clone(), window_b.clone()];
        let evaluated = convert_constraining_windows(&windows, &model, &calibrations);

        assert_eq!(evaluated.len(), 2);

        let WindowHeadroom::Known {
            window: eval_a_win,
            headroom: headroom_a,
        } = &evaluated[0]
        else {
            panic!("expected window_a to produce Known headroom");
        };
        assert_eq!(eval_a_win.semantic_key().as_str(), "window-a");
        // 20 percentage points * 100 credits/point = 2,000 credits = 2,000_000_000 micros.
        assert_eq!(headroom_a.lower(), Credits::from_micros(2_000_000_000));
        assert_eq!(headroom_a.upper(), Credits::from_micros(2_000_000_000));

        let WindowHeadroom::Known {
            window: eval_b_win,
            headroom: headroom_b,
        } = &evaluated[1]
        else {
            panic!("expected window_b to produce Known headroom");
        };
        assert_eq!(eval_b_win.semantic_key().as_str(), "window-b");
        // 40 percentage points * 10 credits/point = 400 credits = 400_000_000 micros.
        assert_eq!(headroom_b.lower(), Credits::from_micros(400_000_000));
        assert_eq!(headroom_b.upper(), Credits::from_micros(400_000_000));

        // Assert that the lower-percentage window holds the larger headroom.
        let remaining_a = window_a.remaining_fraction().as_ppm().get();
        let remaining_b = window_b.remaining_fraction().as_ppm().get();
        assert!(
            remaining_a < remaining_b,
            "window A remaining ({remaining_a}) must be lower than window B ({remaining_b})"
        );
        assert!(
            headroom_a.lower().micros() > headroom_b.upper().micros(),
            "window A headroom ({:?}) must exceed window B headroom ({:?})",
            headroom_a,
            headroom_b
        );
    }

    /// Assert each window's headroom uses that window's own calibration, by giving two
    /// windows deliberately different coefficients.
    #[test]
    fn each_window_uses_its_own_calibration_and_not_another_windows() {
        let model = ModelId::new("model-a");
        let window_1 = make_window("win-1", WindowScope::AccountWide, 500_000); // 500_000 ppm remaining
        let window_2 = make_window("win-2", WindowScope::ModelSpecific(model.clone()), 500_000); // 500_000 ppm remaining

        let calibrations = BTreeMap::from([
            (WindowSemanticKey::new("win-1"), point_calibration(500)),
            (WindowSemanticKey::new("win-2"), point_calibration(2_000)),
        ]);

        let windows = [window_1.clone(), window_2.clone()];
        let evaluated = convert_constraining_windows(&windows, &model, &calibrations);

        assert_eq!(evaluated.len(), 2);
        let hr_1 = evaluated[0].headroom().unwrap();
        let hr_2 = evaluated[1].headroom().unwrap();

        // 500_000 * 500 = 250_000_000 micros.
        assert_eq!(hr_1.lower(), Credits::from_micros(250_000_000));
        // 500_000 * 2000 = 1_000_000_000 micros.
        assert_eq!(hr_2.lower(), Credits::from_micros(1_000_000_000));

        // Swap calibrations to prove each window binds to its own key.
        let swapped_calibrations = BTreeMap::from([
            (WindowSemanticKey::new("win-1"), point_calibration(2_000)),
            (WindowSemanticKey::new("win-2"), point_calibration(500)),
        ]);
        let swapped_evaluated =
            convert_constraining_windows(&windows, &model, &swapped_calibrations);
        let swapped_hr_1 = swapped_evaluated[0].headroom().unwrap();
        let swapped_hr_2 = swapped_evaluated[1].headroom().unwrap();

        assert_eq!(swapped_hr_1.lower(), Credits::from_micros(1_000_000_000));
        assert_eq!(swapped_hr_2.lower(), Credits::from_micros(250_000_000));
    }

    /// Assert an uncalibrated constraining window produces an explicit unknown rather
    /// than being skipped.
    #[test]
    fn uncalibrated_constraining_window_produces_explicit_unknown_rather_than_skipped() {
        let model = ModelId::new("model-a");
        let window_calibrated = make_window("calibrated", WindowScope::AccountWide, 700_000);
        let window_uncalibrated = make_window(
            "uncalibrated",
            WindowScope::ModelSpecific(model.clone()),
            300_000,
        );

        let calibrations = BTreeMap::from([(
            WindowSemanticKey::new("calibrated"),
            point_calibration(1_000),
        )]);

        let windows = [window_calibrated.clone(), window_uncalibrated.clone()];
        let evaluated = convert_constraining_windows(&windows, &model, &calibrations);

        // Both constraining windows must be present in the output.
        assert_eq!(evaluated.len(), 2);
        assert!(evaluated[0].is_known());
        assert_eq!(evaluated[0].window().semantic_key().as_str(), "calibrated");

        assert!(evaluated[1].is_unknown());
        assert_eq!(
            evaluated[1].window().semantic_key().as_str(),
            "uncalibrated"
        );
        assert!(evaluated[1].headroom().is_none());
    }

    /// Assert a calibration whose health is not current is not used, and its window becomes unknown.
    #[test]
    fn non_current_calibration_is_not_used_and_window_becomes_unknown() {
        let model = ModelId::new("model-a");
        let window = make_window("test-win", WindowScope::AccountWide, 500_000);
        let unc = CoefficientUncertainty::new(
            CreditsPerPercentagePoint::from_micros_per_point(100),
            CreditsPerPercentagePoint::from_micros_per_point(200),
        )
        .unwrap();

        let non_current_states = [
            CalibrationHealth::Provisional,
            CalibrationHealth::ReviewDue,
            CalibrationHealth::Suspect,
            CalibrationHealth::Superseded,
            CalibrationHealth::Inapplicable,
        ];

        for health in non_current_states {
            let constraint = CalibratedWindowConstraint::new(unc, health);
            let calibrations = BTreeMap::from([(WindowSemanticKey::new("test-win"), constraint)]);
            let windows = [window.clone()];
            let evaluated = convert_constraining_windows(&windows, &model, &calibrations);

            assert_eq!(evaluated.len(), 1);
            assert!(
                evaluated[0].is_unknown(),
                "calibration with health {:?} must produce Unknown headroom",
                health
            );
            assert!(evaluated[0].headroom().is_none());
        }

        // Current calibration produces Known.
        let current_constraint = CalibratedWindowConstraint::new(unc, CalibrationHealth::Current);
        let current_calibrations =
            BTreeMap::from([(WindowSemanticKey::new("test-win"), current_constraint)]);
        let windows = [window];
        let current_evaluated =
            convert_constraining_windows(&windows, &model, &current_calibrations);
        assert_eq!(current_evaluated.len(), 1);
        assert!(current_evaluated[0].is_known());
        assert!(current_evaluated[0].headroom().is_some());
    }

    /// Assert every window constraining the selected model is enumerated, including account-wide
    /// and model-specific ones, while other models' windows are excluded.
    #[test]
    fn all_constraining_windows_are_enumerated_including_account_wide_and_model_specific() {
        let selected_model = ModelId::new("model-target");
        let other_model = ModelId::new("model-other");

        let account_win = make_window("acc-1", WindowScope::AccountWide, 100_000);
        let target_win = make_window(
            "target-1",
            WindowScope::ModelSpecific(selected_model.clone()),
            200_000,
        );
        let other_win = make_window("other-1", WindowScope::ModelSpecific(other_model), 300_000);

        let windows = [account_win, target_win, other_win];
        let calibrations = BTreeMap::new(); // uncalibrated, so all constraining will be Unknown

        let evaluated = convert_constraining_windows(&windows, &selected_model, &calibrations);

        // Account-wide and model-target windows are returned; model-other is excluded.
        assert_eq!(evaluated.len(), 2);
        let keys: Vec<&str> = evaluated
            .iter()
            .map(|e| e.window().semantic_key().as_str())
            .collect();
        assert_eq!(keys, vec!["acc-1", "target-1"]);
    }

    /// Unit: pure conversion function testable without a database.
    #[test]
    fn pure_function_conversion_testable_without_database() {
        let model = ModelId::new("claude-sonnet");
        let window = make_window("win-pure", WindowScope::AccountWide, 250_000); // 750_000 ppm remaining
        let constraint = interval_calibration(100, 300);
        let calibrations = BTreeMap::from([(WindowSemanticKey::new("win-pure"), constraint)]);

        let windows = [window];
        let result = convert_constraining_windows(&windows, &model, &calibrations);
        assert_eq!(result.len(), 1);
        let hr = result[0].headroom().unwrap();
        // 750_000 * 100 = 75_000_000 micros.
        assert_eq!(hr.lower(), Credits::from_micros(75_000_000));
        // 750_000 * 300 = 225_000_000 micros.
        assert_eq!(hr.upper(), Credits::from_micros(225_000_000));
        assert_eq!(hr.width(), Credits::from_micros(150_000_000));
    }

    proptest! {
        /// Property: widening a calibration interval can only widen the headroom interval,
        /// over generated calibrations.
        #[test]
        fn prop_widening_calibration_interval_can_only_widen_headroom(
            used_ppm in 0i32..=1_000_000i32,
            base_low in 1i64..100_000i64,
            base_width in 0i64..100_000i64,
            widen_low_delta in 0i64..50_000i64,
            widen_high_delta in 0i64..50_000i64,
        ) {
            let base_high = base_low + base_width;
            let widened_low = (base_low - widen_low_delta).max(0);
            let widened_high = base_high + widen_high_delta;

            let model = ModelId::new("prop-model");
            let window_1 = make_window("prop-win", WindowScope::AccountWide, used_ppm);
            let window_2 = window_1.clone();

            let base_constraint = interval_calibration(base_low, base_high);
            let widened_constraint = interval_calibration(widened_low, widened_high);

            let base_cal = BTreeMap::from([(WindowSemanticKey::new("prop-win"), base_constraint)]);
            let widened_cal = BTreeMap::from([(WindowSemanticKey::new("prop-win"), widened_constraint)]);

            let windows_1 = [window_1];
            let windows_2 = [window_2];
            let base_eval = convert_constraining_windows(&windows_1, &model, &base_cal);
            let widened_eval = convert_constraining_windows(&windows_2, &model, &widened_cal);

            let base_hr = base_eval[0].headroom().unwrap();
            let widened_hr = widened_eval[0].headroom().unwrap();

            // Widened lower endpoint must be <= base lower endpoint.
            prop_assert!(
                widened_hr.lower().micros() <= base_hr.lower().micros(),
                "widened lower ({}) must be <= base lower ({})",
                widened_hr.lower().micros(),
                base_hr.lower().micros()
            );

            // Widened upper endpoint must be >= base upper endpoint.
            prop_assert!(
                widened_hr.upper().micros() >= base_hr.upper().micros(),
                "widened upper ({}) must be >= base upper ({})",
                widened_hr.upper().micros(),
                base_hr.upper().micros()
            );

            // Widened width must be >= base width.
            prop_assert!(
                widened_hr.width().micros() >= base_hr.width().micros(),
                "widened width ({}) must be >= base width ({})",
                widened_hr.width().micros(),
                base_hr.width().micros()
            );
        }
    }
}
