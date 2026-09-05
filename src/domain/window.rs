//! Provider quota-window semantics and selection.
//!
//! May not depend on:
//! - calibration fitting or persistence
//! - advice, status, or presentation
//!
//! A provider can impose several independently calibrated constraints on one model.
//! Display selects the lowest remaining fraction; workload advice selects the smallest
//! calibrated credit headroom. Keeping both selections here prevents a percentage from
//! silently standing in for capacity.

use std::collections::BTreeMap;

use super::{
    credits::{Credits, CreditsPerPercentagePoint},
    quota::{PercentagePoints, QuotaFractionPpm, QuotaRemaining, QuotaUsed},
    time::UtcTimestamp,
};

/// A stable provider-defined key for one quota constraint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowSemanticKey(String);

impl WindowSemanticKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A provider model identifier used to match model-scoped quota constraints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The scope kind a provider reports for a quota constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowScopeKind {
    AccountWide,
    ModelSpecific,
}

/// Which models a quota constraint can constrain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowScope {
    AccountWide,
    ModelSpecific(ModelId),
}

impl WindowScope {
    pub fn kind(&self) -> WindowScopeKind {
        match self {
            Self::AccountWide => WindowScopeKind::AccountWide,
            Self::ModelSpecific(_) => WindowScopeKind::ModelSpecific,
        }
    }

    pub fn scoped_model(&self) -> Option<&ModelId> {
        match self {
            Self::AccountWide => None,
            Self::ModelSpecific(model) => Some(model),
        }
    }

    pub fn constrains(&self, model: &ModelId) -> bool {
        match self {
            Self::AccountWide => true,
            Self::ModelSpecific(scoped_model) => scoped_model == model,
        }
    }
}

/// Smallest provider-reported increment of quota usage, in parts per million.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportedResolution(QuotaFractionPpm);

impl ReportedResolution {
    /// Constructs a non-zero resolution. A zero-width measurement has no resolution
    /// semantics and would make quantization claims meaningless.
    pub fn new(ppm: QuotaFractionPpm) -> Option<Self> {
        (ppm.get() != 0).then_some(Self(ppm))
    }

    pub fn as_ppm(self) -> QuotaFractionPpm {
        self.0
    }
}

/// How a provider maps an underlying value onto its reported resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizationSemantics {
    Exact,
    RoundedToNearest,
    RoundedDown,
    RoundedUp,
    Unknown,
}

/// Nominal length of a provider quota window, in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NominalWindowDuration(u64);

impl NominalWindowDuration {
    pub fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub fn as_nanos(self) -> u64 {
        self.0
    }
}

/// The provider-reported reset state of a quota window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowResetState {
    /// A known reset instant reported by the provider.
    Known(UtcTimestamp),
    /// The window has not started yet (idle window with no activity since last reset).
    NotStarted,
}

impl WindowResetState {
    pub fn instant(self) -> Option<UtcTimestamp> {
        match self {
            Self::Known(ts) => Some(ts),
            Self::NotStarted => None,
        }
    }

    pub fn is_not_started(self) -> bool {
        matches!(self, Self::NotStarted)
    }

    pub fn is_known(self) -> bool {
        matches!(self, Self::Known(_))
    }
}

impl From<UtcTimestamp> for WindowResetState {
    fn from(ts: UtcTimestamp) -> Self {
        Self::Known(ts)
    }
}

/// A provider-reported severity label for a quota constraint.
///
/// The provider owns the vocabulary, so the domain preserves the label as a
/// typed value instead of guessing an enum that would turn a future provider
/// label into an unmeasured window.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowSeverity(String);

impl WindowSeverity {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn unknown() -> Self {
        Self::new("unknown")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_unknown(&self) -> bool {
        self.0 == "unknown"
    }
}

/// A normalized provider quota constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeterWindow {
    semantic_key: WindowSemanticKey,
    scope: WindowScope,
    quota_used: QuotaUsed,
    reported_resolution: ReportedResolution,
    quantization: QuantizationSemantics,
    resets_at: WindowResetState,
    nominal_duration: NominalWindowDuration,
    is_active: bool,
    severity: WindowSeverity,
}

impl MeterWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        semantic_key: WindowSemanticKey,
        scope: WindowScope,
        quota_used: QuotaUsed,
        reported_resolution: ReportedResolution,
        quantization: QuantizationSemantics,
        resets_at: impl Into<WindowResetState>,
        nominal_duration: NominalWindowDuration,
    ) -> Self {
        let resets_at = resets_at.into();
        let is_active = resets_at.is_known();
        Self::new_with_facts(
            semantic_key,
            scope,
            quota_used,
            reported_resolution,
            quantization,
            resets_at,
            nominal_duration,
            is_active,
            WindowSeverity::unknown(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_facts(
        semantic_key: WindowSemanticKey,
        scope: WindowScope,
        quota_used: QuotaUsed,
        reported_resolution: ReportedResolution,
        quantization: QuantizationSemantics,
        resets_at: impl Into<WindowResetState>,
        nominal_duration: NominalWindowDuration,
        is_active: bool,
        severity: WindowSeverity,
    ) -> Self {
        Self {
            semantic_key,
            scope,
            quota_used,
            reported_resolution,
            quantization,
            resets_at: resets_at.into(),
            nominal_duration,
            is_active,
            severity,
        }
    }

    pub fn semantic_key(&self) -> &WindowSemanticKey {
        &self.semantic_key
    }

    pub fn scope(&self) -> &WindowScope {
        &self.scope
    }

    pub fn quota_used(&self) -> QuotaUsed {
        self.quota_used
    }

    pub fn reported_resolution(&self) -> ReportedResolution {
        self.reported_resolution
    }

    pub fn quantization(&self) -> QuantizationSemantics {
        self.quantization
    }

    pub fn reset_state(&self) -> WindowResetState {
        self.resets_at
    }

    pub fn resets_at(&self) -> Option<UtcTimestamp> {
        self.resets_at.instant()
    }

    pub fn nominal_duration(&self) -> NominalWindowDuration {
        self.nominal_duration
    }

    /// Whether the provider marked this constraint active in the response.
    ///
    /// This is a provider fact, not the display-selection rule. In particular,
    /// [`lowest_remaining_fraction_window`] still considers every applicable
    /// window whose reset state is current.
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    pub fn severity(&self) -> &WindowSeverity {
        &self.severity
    }

    /// True before the provider's stated reset instant. At that instant this reading
    /// belongs to the completed window and must not be treated as current.
    /// Returns false for not-started windows where no active window is in progress.
    pub fn is_active_at(&self, now: UtcTimestamp) -> bool {
        match self.resets_at {
            WindowResetState::Known(reset) => now < reset,
            WindowResetState::NotStarted => false,
        }
    }

    pub fn remaining_fraction(&self) -> QuotaRemaining {
        self.quota_used.complement()
    }

    pub fn constrains(&self, model: &ModelId) -> bool {
        self.scope.constrains(model)
    }

    pub fn remaining_percentage_points(&self) -> PercentagePoints {
        self.remaining_fraction().as_ppm() - zero_fraction()
    }
}

/// The display selection: applicable window with least remaining fraction.
///
/// Calibration intentionally is not an input here. A caller deciding whether work fits
/// must use [`limiting_credit_headroom_window`] instead.
pub fn lowest_remaining_fraction_window<'a>(
    windows: &'a [MeterWindow],
    model: &ModelId,
) -> Option<&'a MeterWindow> {
    windows
        .iter()
        .filter(|window| window.constrains(model))
        .min_by_key(|window| {
            if window.reset_state().is_not_started() {
                1_000_000
            } else {
                window.remaining_fraction().as_ppm().get()
            }
        })
}

/// Result of selecting the workload constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditHeadroomSelection<'a> {
    Known {
        window: &'a MeterWindow,
        headroom: Credits,
    },
    Unknown {
        uncalibrated_window: &'a MeterWindow,
    },
}

impl<'a> CreditHeadroomSelection<'a> {
    pub fn window(self) -> &'a MeterWindow {
        match self {
            Self::Known { window, .. }
            | Self::Unknown {
                uncalibrated_window: window,
            } => window,
        }
    }
}

/// The workload-advice selection: applicable window with least calibrated credits.
///
/// Every applicable window must have its own calibration. Missing calibration returns
/// [`CreditHeadroomSelection::Unknown`] rather than silently discarding a constraint.
pub fn limiting_credit_headroom_window<'a>(
    windows: &'a [MeterWindow],
    model: &ModelId,
    calibrations: &BTreeMap<WindowSemanticKey, CreditsPerPercentagePoint>,
) -> Option<CreditHeadroomSelection<'a>> {
    let mut limiting: Option<(&MeterWindow, Credits)> = None;

    for window in windows.iter().filter(|window| window.constrains(model)) {
        let Some(calibration) = calibrations.get(window.semantic_key()) else {
            return Some(CreditHeadroomSelection::Unknown {
                uncalibrated_window: window,
            });
        };
        let headroom = *calibration * window.remaining_percentage_points();
        if limiting.is_none_or(|(_, current)| headroom.micros() < current.micros()) {
            limiting = Some((window, headroom));
        }
    }

    limiting.map(|(window, headroom)| CreditHeadroomSelection::Known { window, headroom })
}

fn zero_fraction() -> QuotaFractionPpm {
    QuotaFractionPpm::new(0).expect("zero is in quota fraction range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn window(key: &str, scope: WindowScope, used_ppm: i32, reset_nanos: i64) -> MeterWindow {
        MeterWindow::new(
            WindowSemanticKey::new(key),
            scope,
            QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::RoundedToNearest,
            UtcTimestamp::from_unix_nanos(reset_nanos),
            NominalWindowDuration::from_nanos(1_000_000_000),
        )
    }

    fn calibration(
        key: &str,
        micros_per_point: i64,
    ) -> (WindowSemanticKey, CreditsPerPercentagePoint) {
        (
            WindowSemanticKey::new(key),
            CreditsPerPercentagePoint::from_micros_per_point(micros_per_point),
        )
    }

    #[test]
    fn display_and_advice_select_different_windows_when_calibrations_diverge() {
        let model = ModelId::new("model-a");
        let windows = [
            window("account", WindowScope::AccountWide, 800_000, 100),
            window(
                "model",
                WindowScope::ModelSpecific(model.clone()),
                600_000,
                100,
            ),
        ];
        let calibrations = BTreeMap::from([calibration("account", 100), calibration("model", 10)]);

        assert_eq!(
            lowest_remaining_fraction_window(&windows, &model)
                .unwrap()
                .semantic_key()
                .as_str(),
            "account"
        );
        assert_eq!(
            limiting_credit_headroom_window(&windows, &model, &calibrations)
                .unwrap()
                .window()
                .semantic_key()
                .as_str(),
            "model"
        );
    }

    #[test]
    fn display_selection_does_not_use_provider_activity_as_a_filter() {
        let model = ModelId::new("model-a");
        let inactive_tight_window = MeterWindow::new_with_facts(
            WindowSemanticKey::new("inactive-tight"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(900_000).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::Exact,
            UtcTimestamp::from_unix_nanos(100),
            NominalWindowDuration::from_nanos(1_000),
            false,
            WindowSeverity::new("critical"),
        );
        let windows = [inactive_tight_window];
        let selected = lowest_remaining_fraction_window(&windows, &model)
            .expect("an applicable inactive provider window remains a measured constraint");
        assert_eq!(selected.semantic_key().as_str(), "inactive-tight");
        assert!(!selected.is_active());
    }

    #[test]
    fn unrelated_model_window_does_not_constrain_selected_model() {
        let selected = ModelId::new("model-a");
        let windows = [
            window("account", WindowScope::AccountWide, 400_000, 100),
            window(
                "other",
                WindowScope::ModelSpecific(ModelId::new("model-b")),
                990_000,
                100,
            ),
        ];

        assert_eq!(
            lowest_remaining_fraction_window(&windows, &selected)
                .unwrap()
                .semantic_key()
                .as_str(),
            "account"
        );
    }

    #[test]
    fn reset_boundary_excludes_the_completed_window_at_its_reset_instant() {
        let window = window("account", WindowScope::AccountWide, 400_000, 100);

        assert!(window.is_active_at(UtcTimestamp::from_unix_nanos(99)));
        assert!(!window.is_active_at(UtcTimestamp::from_unix_nanos(100)));
        assert!(!window.is_active_at(UtcTimestamp::from_unix_nanos(101)));
    }

    #[test]
    fn missing_applicable_calibration_is_unknown_not_a_skipped_constraint() {
        let model = ModelId::new("model-a");
        let windows = [
            window("account", WindowScope::AccountWide, 400_000, 100),
            window(
                "model",
                WindowScope::ModelSpecific(model.clone()),
                900_000,
                100,
            ),
        ];
        let calibrations = BTreeMap::from([calibration("account", 100)]);

        assert!(matches!(
            limiting_credit_headroom_window(&windows, &model, &calibrations),
            Some(CreditHeadroomSelection::Unknown {
                uncalibrated_window
            }) if uncalibrated_window.semantic_key().as_str() == "model"
        ));
    }

    proptest::proptest! {
        #[test]
        fn prop_adding_a_tighter_window_never_raises_selected_headroom(
            base_remaining_ppm in 0u32..=1_000_000u32,
            tighter_remaining_ppm in 0u32..=1_000_000u32,
            base_micros in 1i64..1_000_000i64,
            tighter_micros in 1i64..1_000_000i64,
        ) {
            let model = ModelId::new("model-a");
            let base = window(
                "base",
                WindowScope::AccountWide,
                (1_000_000 - base_remaining_ppm).try_into().unwrap(),
                100,
            );
            let tighter = window(
                "tighter",
                WindowScope::ModelSpecific(model.clone()),
                (1_000_000 - tighter_remaining_ppm).try_into().unwrap(),
                100,
            );
            let calibrations = BTreeMap::from([
                (
                    crate::domain::window::WindowSemanticKey::new("base"),
                    CreditsPerPercentagePoint::from_micros_per_point(base_micros),
                ),
                (
                    crate::domain::window::WindowSemanticKey::new("tighter"),
                    CreditsPerPercentagePoint::from_micros_per_point(tighter_micros),
                ),
            ]);
            let before_windows = [base.clone()];
            let after_windows = [base, tighter];
            let before =
                limiting_credit_headroom_window(&before_windows, &model, &calibrations).unwrap();
            let after =
                limiting_credit_headroom_window(&after_windows, &model, &calibrations).unwrap();

            let CreditHeadroomSelection::Known {
                headroom: before_headroom, ..
            } = before
            else {
                prop_assert!(false, "base calibration is present");
                return Ok(());
            };
            let CreditHeadroomSelection::Known {
                headroom: after_headroom, ..
            } = after
            else {
                prop_assert!(false, "both calibrations are present");
                return Ok(());
            };

            prop_assert!(
                after_headroom.micros() <= before_headroom.micros(),
                "adding another constraint raised headroom from {:?} to {:?}",
                before_headroom,
                after_headroom
            );
        }
    }

    /// Retained hand-picked regression: walks stepping fractions over fixed calibrations.
    #[test]
    fn adding_a_tighter_window_never_raises_selected_headroom_hand_picked() {
        let model = ModelId::new("model-a");
        for remaining_ppm in (100_000..=900_000).step_by(100_000) {
            let base = window("base", WindowScope::AccountWide, 500_000, 100);
            let tighter = window(
                "tighter",
                WindowScope::ModelSpecific(model.clone()),
                1_000_000 - remaining_ppm,
                100,
            );
            let calibrations =
                BTreeMap::from([calibration("base", 100), calibration("tighter", 1)]);
            let before_windows = [base.clone()];
            let after_windows = [base, tighter];
            let before =
                limiting_credit_headroom_window(&before_windows, &model, &calibrations).unwrap();
            let after =
                limiting_credit_headroom_window(&after_windows, &model, &calibrations).unwrap();
            let CreditHeadroomSelection::Known {
                headroom: before, ..
            } = before
            else {
                panic!("base calibration is present");
            };
            let CreditHeadroomSelection::Known {
                headroom: after, ..
            } = after
            else {
                panic!("both calibrations are present");
            };
            assert!(after.micros() <= before.micros());
        }
    }

    #[test]
    fn lowest_remaining_fraction_window_treats_not_started_as_zero_used() {
        let model = ModelId::new("claude-sonnet");
        let idle_five_hour = MeterWindow::new(
            WindowSemanticKey::new("five_hour"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(0).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::Exact,
            WindowResetState::NotStarted,
            NominalWindowDuration::from_nanos(5 * 3600 * 1_000_000_000),
        );
        let weekly_used = MeterWindow::new(
            WindowSemanticKey::new("seven_day"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(210_000).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::Exact,
            WindowResetState::Known(UtcTimestamp::from_unix_nanos(100_000)),
            NominalWindowDuration::from_nanos(7 * 24 * 3600 * 1_000_000_000),
        );

        let windows = [idle_five_hour.clone(), weekly_used.clone()];
        let selected = lowest_remaining_fraction_window(&windows, &model).unwrap();
        assert_eq!(selected.semantic_key().as_str(), "seven_day");

        // When only the not-started window is present, it is selected with 100% remaining.
        let only_idle = [idle_five_hour];
        let selected_idle = lowest_remaining_fraction_window(&only_idle, &model).unwrap();
        assert_eq!(selected_idle.semantic_key().as_str(), "five_hour");
    }

    #[test]
    fn not_started_window_is_never_active() {
        let idle_window = MeterWindow::new(
            WindowSemanticKey::new("five_hour"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(0).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::Exact,
            WindowResetState::NotStarted,
            NominalWindowDuration::from_nanos(5 * 3600 * 1_000_000_000),
        );

        assert!(!idle_window.is_active_at(UtcTimestamp::from_unix_nanos(0)));
        assert!(!idle_window.is_active_at(UtcTimestamp::from_unix_nanos(100_000)));
        assert_eq!(idle_window.resets_at(), None);
        assert_eq!(idle_window.reset_state(), WindowResetState::NotStarted);
    }
}
