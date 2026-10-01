//! A per-account selection signal for status JSON (`aub-vkv7`).
//!
//! May not depend on:
//! - calibration fitting or persistence
//! - advice, status, or presentation
//!
//! An orchestrator picking the account for a task needs one comparable signal
//! per account. The ledger already holds every input (percent used per window,
//! burn rate, reset instant, nominal duration, freshness) and publishes no
//! derived signal, so every consumer re-derives its own ranking from the raw
//! windows. This module computes the one number instead, plus the runway and
//! confidence that qualify it.
//!
//! The formula is quota-axi's `spendPriority` (its README, "Per-scope selection
//! signal"): percentage points of paid allowance projected to reach reset
//! unused. Positive means spending here reclaims allowance that would otherwise
//! be forfeited; negative means spending here burns allowance the cycle still
//! needs.

use super::burn_rate::BurnRate;
use super::freshness::Freshness;
use super::quota::{QuotaRemaining, QuotaUsed};
use super::time::UtcTimestamp;
use super::window::{MeterWindow, NominalWindowDuration, WindowResetState};

/// Parts per million at which a window is capped: 0% remaining.
const CAP_PPM: u32 = 1_000_000;

/// The scalar is clamped to this symmetric bound, so a window read instants
/// before its reset cannot dominate the ranking with a near-infinite gap.
const SPEND_PRIORITY_CLAMP: f64 = 100.0;

/// Share of the limiting window's cycle elapsed below which confidence reads
/// early: the burn behind the scalar is still sampling jitter there.
const EARLY_CONFIDENCE_ELAPSED: f64 = 0.2;

const NANOS_PER_SECOND: f64 = 1_000_000_000.0;

/// The four facts the selection formula reads from one window.
///
/// Built from a [`MeterWindow`] plus the report-time burn through
/// [`SelectionWindow::from_meter`]; the status JSON caller builds it
/// field-wise from the same stored inputs because a `MeterWindow` also carries
/// provider facts (activity, severity) the formula never reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionWindow {
    used_ppm: u32,
    reset: WindowResetState,
    nominal: NominalWindowDuration,
    burn: Option<BurnRate>,
}

impl SelectionWindow {
    pub fn new(
        used_ppm: u32,
        reset: WindowResetState,
        nominal: NominalWindowDuration,
        burn: Option<BurnRate>,
    ) -> Self {
        Self {
            used_ppm,
            reset,
            nominal,
            burn,
        }
    }

    /// The formula's view of a domain window: its stored usage, reset state
    /// and nominal duration, plus the burn rate the report derived for it
    /// (`None` when the window never started, capped, or elapsed too little
    /// to divide by).
    pub fn from_meter(window: &MeterWindow, burn: Option<BurnRate>) -> Self {
        Self::new(
            window.quota_used().as_ppm().get(),
            window.reset_state(),
            window.nominal_duration(),
            burn,
        )
    }

    /// An untriggered window carries no reset instant and no usage. It is
    /// fully available, not a gap, and the selection skips it rather than
    /// measuring it.
    fn is_untriggered(self) -> bool {
        self.reset.instant().is_none() && self.used_ppm == 0
    }

    /// Quota consumed as a [`QuotaUsed`] level, the typed form of the stored
    /// integer. Exists so the percent-remaining arithmetic below reads
    /// against the domain vocabulary instead of a bare integer.
    fn used(self) -> QuotaUsed {
        QuotaUsed::new(
            super::quota::QuotaFractionPpm::new(self.used_ppm.min(CAP_PPM) as i32)
                .expect("clamped to the quota fraction range"),
        )
    }
}

/// How far the account's quota reaches at its current burn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runway {
    /// Every measurable window reaches its reset before burning out.
    ThroughReset,
    /// At least one window burns out before its reset; `usable_runway_secs`
    /// carries the earliest exhaustion.
    ProjectedExhaustion,
    /// At least one bounding window has 0% remaining.
    ExhaustedNow,
    /// At least one bounding window has no usable burn or no reset, so the
    /// minimum over windows is unknowable.
    Unknown,
}

impl Runway {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ThroughReset => "through_reset",
            Self::ProjectedExhaustion => "projected_exhaustion",
            Self::ExhaustedNow => "exhausted_now",
            Self::Unknown => "unknown",
        }
    }
}

/// How much of the limiting window's cycle the burn behind the scalar stands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Less than 20% of the limiting window's cycle has elapsed.
    Early,
    /// At least 20% has elapsed, or no window has started to time.
    Established,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Early => "early",
            Self::Established => "established",
        }
    }
}

/// The per-account selection signal: one scalar a consumer ranks on, plus the
/// runway and confidence that qualify it.
///
/// `spend_priority` is percentage points of paid allowance per one percent of
/// cycle time, cycle-weighted across the measurable windows and clamped to
/// `[-100, 100]`. It is `None` (absent in JSON, never zero) when no window is
/// measurable, and when the account is exhausted: an exhausted account must
/// not carry a number that invites spending. `usable_runway_secs` is present
/// exactly when the runway is finite: zero at exhaustion, the earliest
/// exhaustion in seconds under projection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selection {
    pub spend_priority: Option<f64>,
    pub runway: Runway,
    pub usable_runway_secs: Option<u64>,
    pub confidence: Confidence,
}

/// Computes the selection signal over an account's windows at the report
/// instant.
///
/// `freshness` is taken so the caller passes the reading's state explicitly
/// and ignored on purpose: a stale account keeps its numbers, and the caller
/// decides how to present a stale selection. Untriggered windows (no reset,
/// no usage) are skipped as fully available. Precedence over the account is
/// exhausted first (a fact needing no rate), then unknown (one unmeasurable
/// bounding window hides the minimum over all of them), then projected
/// exhaustion, then through reset.
pub fn compute_selection(
    windows: &[SelectionWindow],
    _freshness: &Freshness<QuotaRemaining>,
    now: UtcTimestamp,
) -> Selection {
    let confidence = confidence(windows, now);
    let bounding: Vec<&SelectionWindow> = windows
        .iter()
        .filter(|window| !window.is_untriggered())
        .collect();

    if bounding.iter().any(|window| window.used_ppm >= CAP_PPM) {
        return Selection {
            spend_priority: None,
            runway: Runway::ExhaustedNow,
            usable_runway_secs: Some(0),
            confidence,
        };
    }

    let mut unmeasurable = false;
    let mut gaps: Vec<(f64, f64)> = Vec::new();
    let mut earliest_exhaustion_secs: Option<f64> = None;
    for window in bounding {
        let Some(measured) = measure(window, now) else {
            unmeasurable = true;
            continue;
        };
        if measured.exhaustion_secs < measured.remaining_secs
            && earliest_exhaustion_secs.is_none_or(|earliest| measured.exhaustion_secs < earliest)
        {
            earliest_exhaustion_secs = Some(measured.exhaustion_secs);
        }
        gaps.push((measured.gap, measured.weight_secs));
    }

    if unmeasurable {
        return Selection {
            spend_priority: average_gap(&gaps),
            runway: Runway::Unknown,
            usable_runway_secs: None,
            confidence,
        };
    }
    if let Some(secs) = earliest_exhaustion_secs {
        return Selection {
            spend_priority: average_gap(&gaps),
            runway: Runway::ProjectedExhaustion,
            usable_runway_secs: Some(secs.round() as u64),
            confidence,
        };
    }
    Selection {
        spend_priority: average_gap(&gaps),
        runway: Runway::ThroughReset,
        usable_runway_secs: None,
        confidence,
    }
}

/// One measurable window's contribution: its gap, its cycle weight, and its
/// exhaustion and remaining times in seconds.
struct Measured {
    gap: f64,
    weight_secs: f64,
    exhaustion_secs: f64,
    remaining_secs: f64,
}

/// Measures one bounding window, or `None` when it cannot be timed or has no
/// burn: no reset instant with nonzero usage, a zero nominal duration, no
/// burn rate, or a cycle that already ended (which counts as reaching its
/// reset, handled by the caller skipping it rather than by manufacturing a
/// gap over zero time left).
fn measure(window: &SelectionWindow, now: UtcTimestamp) -> Option<Measured> {
    let reset_at = window.reset.instant()?;
    let nominal_nanos = window.nominal.as_nanos();
    if nominal_nanos == 0 {
        return None;
    }
    let burn = window.burn?;
    let remaining_nanos = reset_at.unix_nanos() - now.unix_nanos();
    if remaining_nanos <= 0 {
        return None;
    }
    let nominal_secs = nominal_nanos as f64 / NANOS_PER_SECOND;
    let remaining_secs = remaining_nanos as f64 / NANOS_PER_SECOND;
    let remaining_pct = remaining_secs / nominal_secs * 100.0;
    let percent_remaining = 100.0 - window.used().as_ppm().get() as f64 / 10_000.0;
    let burn_multiple = burn.get();
    let surplus = percent_remaining - burn_multiple * remaining_pct;
    Some(Measured {
        gap: surplus / remaining_pct,
        weight_secs: nominal_secs,
        exhaustion_secs: if burn_multiple == 0.0 {
            f64::INFINITY
        } else {
            percent_remaining / 100.0 / burn_multiple * nominal_secs
        },
        remaining_secs,
    })
}

/// The cycle-weighted mean gap over the measurable windows, clamped to the
/// symmetric bound. `None` when nothing was measurable, or when the mean is
/// somehow non-finite rather than a number the ledger justifies.
fn average_gap(gaps: &[(f64, f64)]) -> Option<f64> {
    if gaps.is_empty() {
        return None;
    }
    let weighted: f64 = gaps.iter().map(|(gap, weight)| gap * weight).sum();
    let total: f64 = gaps.iter().map(|(_, weight)| weight).sum();
    if total <= 0.0 {
        return None;
    }
    let mean = (weighted / total).clamp(-SPEND_PRIORITY_CLAMP, SPEND_PRIORITY_CLAMP);
    mean.is_finite().then_some(mean)
}

/// Confidence from the limiting window: the started window with the most
/// usage, mirroring the status grid's own limiting rule
/// (`MeterAccount::limiting_status_window`) over the started set. Early below
/// one fifth elapsed, established otherwise, and established when no window
/// has started to time.
fn confidence(windows: &[SelectionWindow], now: UtcTimestamp) -> Confidence {
    let mut limiting: Option<(&SelectionWindow, UtcTimestamp)> = None;
    for window in windows {
        let Some(reset_at) = window.reset.instant() else {
            continue;
        };
        let more_used = limiting.is_none_or(|(current, _)| window.used_ppm > current.used_ppm);
        if more_used {
            limiting = Some((window, reset_at));
        }
    }
    let Some((limit, reset_at)) = limiting else {
        return Confidence::Established;
    };
    let nominal_nanos = limit.nominal.as_nanos();
    if nominal_nanos == 0 {
        return Confidence::Established;
    }
    let remaining_nanos = reset_at.unix_nanos() - now.unix_nanos();
    let elapsed = 1.0 - remaining_nanos as f64 / nominal_nanos as f64;
    if elapsed.clamp(0.0, 1.0) < EARLY_CONFIDENCE_ELAPSED {
        Confidence::Early
    } else {
        Confidence::Established
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::{Observed, StaleReason};
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::{MeasurementBasis, ReceivedAt};
    use crate::domain::window::{
        QuantizationSemantics, ReportedResolution, WindowScope, WindowSemanticKey,
    };

    const HOUR_NANOS: i64 = 3_600_000_000_000;

    fn nominal_hours(hours: i64) -> NominalWindowDuration {
        NominalWindowDuration::from_nanos((hours * HOUR_NANOS) as u64)
    }

    fn known(nanos: i64) -> WindowResetState {
        WindowResetState::Known(UtcTimestamp::from_unix_nanos(nanos))
    }

    fn meter_window(
        used_ppm: i32,
        reset: WindowResetState,
        nominal: NominalWindowDuration,
    ) -> MeterWindow {
        MeterWindow::new(
            WindowSemanticKey::new("five_hour"),
            WindowScope::AccountWide,
            QuotaUsed::new(QuotaFractionPpm::new(used_ppm).unwrap()),
            ReportedResolution::new(QuotaFractionPpm::new(10_000).unwrap()).unwrap(),
            QuantizationSemantics::Exact,
            reset,
            nominal,
        )
    }

    fn window(
        used_ppm: u32,
        reset: WindowResetState,
        nominal_h: i64,
        burn: Option<f64>,
    ) -> SelectionWindow {
        SelectionWindow::new(
            used_ppm,
            reset,
            nominal_hours(nominal_h),
            burn.map(|rate| BurnRate::new(rate).unwrap()),
        )
    }

    fn observed_remaining(ppm: u32) -> Observed<QuotaRemaining> {
        Observed::new(
            QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap()),
            None,
            ReceivedAt::new(UtcTimestamp::from_unix_nanos(1)),
            MeasurementBasis::ProviderObserved,
        )
    }

    fn fresh_reading() -> Freshness<QuotaRemaining> {
        Freshness::Fresh {
            observed: observed_remaining(820_000),
            latest_attempt: AttemptId::new(1),
        }
    }

    fn stale_reading() -> Freshness<QuotaRemaining> {
        Freshness::Stale {
            last_good: Some(observed_remaining(820_000)),
            latest_attempt: AttemptId::new(2),
            reason: StaleReason::AgeExceeded,
        }
    }

    /// One window at 82% remaining, burn 0.25, 30% of its cycle left.
    /// percentRemaining = 100 - 18 = 82; timeRemaining = 30.
    /// S = 82 - 0.25 * 30 = 74.5; gap = 74.5 / 30 = 2.4833, inside 2.47 +- 0.02.
    #[test]
    fn one_window_yields_the_hand_computed_scalar() {
        let nominal = nominal_hours(10);
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 10_800_000_000_000);
        let meter = meter_window(180_000, known(reset_nanos), nominal);
        let burn = BurnRate::new(0.25).unwrap();
        let selection = compute_selection(
            &[SelectionWindow::from_meter(&meter, Some(burn))],
            &fresh_reading(),
            now,
        );

        let priority = selection
            .spend_priority
            .expect("one measurable window yields a scalar");
        assert!(
            (priority - 2.4833).abs() < 0.01,
            "spend_priority {priority} is not the hand-computed 2.4833"
        );
        // Exhaustion at 0.82 / 0.25 = 3.28 cycles outruns the 0.30 left.
        assert_eq!(selection.runway, Runway::ThroughReset);
        assert_eq!(selection.usable_runway_secs, None);
        // 70% of the cycle elapsed: past the 20% early line.
        assert_eq!(selection.confidence, Confidence::Established);
    }

    /// Every window untriggered: no reset, no usage. Fully available, so the
    /// runway reaches every reset and there is nothing to average.
    #[test]
    fn all_untriggered_windows_read_as_through_reset_with_no_scalar() {
        let now = UtcTimestamp::from_unix_nanos(1_000);
        let idle = [
            window(0, WindowResetState::NotStarted, 5, None),
            window(0, WindowResetState::NotStarted, 168, None),
        ];
        let selection = compute_selection(&idle, &fresh_reading(), now);

        assert_eq!(selection.runway, Runway::ThroughReset);
        assert_eq!(selection.spend_priority, None);
        assert_eq!(selection.usable_runway_secs, None);
    }

    /// A capped window carries no live burn on the status path, and needs
    /// none: 0% remaining is exhausted whatever the pace was.
    #[test]
    fn a_window_at_zero_remaining_is_exhausted_now() {
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 10_800_000_000_000);
        let capped = [window(1_000_000, known(reset_nanos), 10, None)];
        let selection = compute_selection(&capped, &fresh_reading(), now);

        assert_eq!(selection.runway, Runway::ExhaustedNow);
        assert_eq!(selection.usable_runway_secs, Some(0));
        assert_eq!(selection.spend_priority, None);
    }

    /// 50% remaining at burn 2.0 over a 10 h cycle exhausts at
    /// (0.50 / 2.0) * 36_000 = 9_000 s, before the reset 18_000 s out.
    /// S = 50 - 2.0 * 50 = -50; gap = -50 / 50 = -1.0.
    #[test]
    fn burn_that_outruns_the_reset_projects_exhaustion_with_usable_seconds() {
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 18_000_000_000_000);
        let racing = [window(500_000, known(reset_nanos), 10, Some(2.0))];
        let selection = compute_selection(&racing, &fresh_reading(), now);

        assert_eq!(selection.runway, Runway::ProjectedExhaustion);
        assert_eq!(selection.usable_runway_secs, Some(9_000));
        let priority = selection
            .spend_priority
            .expect("the racing window is measurable");
        assert!(
            (priority - -1.0).abs() < 1e-9,
            "spend_priority {priority} is not the hand-computed -1.0"
        );
    }

    /// Freshness never degrades the numbers: the same windows give the same
    /// selection fresh or stale, and the caller decides what stale means.
    #[test]
    fn a_stale_account_keeps_its_numbers() {
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 10_800_000_000_000);
        let windows = [window(180_000, known(reset_nanos), 10, Some(0.25))];

        let fresh = compute_selection(&windows, &fresh_reading(), now);
        let stale = compute_selection(&windows, &stale_reading(), now);
        assert_eq!(fresh, stale);
    }

    /// No burn, no scalar, no runway: the minimum over windows is unknowable
    /// when one bounding window cannot be timed. The planted negative beside
    /// it is a window with no reset but real usage, which is unknown for the
    /// same reason rather than silently skipped as untriggered.
    #[test]
    fn a_window_with_unknown_burn_is_unknown_with_no_scalar() {
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 10_800_000_000_000);
        let burning_unknown = [window(180_000, known(reset_nanos), 10, None)];
        let selection = compute_selection(&burning_unknown, &fresh_reading(), now);
        assert_eq!(selection.runway, Runway::Unknown);
        assert_eq!(selection.spend_priority, None);

        let resetless_used = [window(300_000, WindowResetState::NotStarted, 10, Some(0.5))];
        let selection = compute_selection(&resetless_used, &fresh_reading(), now);
        assert_eq!(selection.runway, Runway::Unknown);
        assert_eq!(selection.spend_priority, None);
    }

    /// Two measurable windows average by cycle length: window A (10 h cycle,
    /// gap 2.4833) and window B (30 h cycle: 90% remaining, burn 0.5, half its
    /// cycle left, so S = 90 - 0.5 * 50 = 65 and gap = 1.3).
    /// (2.4833 * 36_000 + 1.3 * 108_000) / 144_000 = 229_800 / 144_000 = 1.5958.
    #[test]
    fn two_windows_weight_gaps_by_cycle_length() {
        let reset_a = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_a - 10_800_000_000_000);
        let reset_b = now.unix_nanos() + 54_000_000_000_000;
        let windows = [
            window(180_000, known(reset_a), 10, Some(0.25)),
            window(100_000, known(reset_b), 30, Some(0.5)),
        ];
        let selection = compute_selection(&windows, &fresh_reading(), now);

        let priority = selection
            .spend_priority
            .expect("two measurable windows yield a scalar");
        assert!(
            (priority - 1.5958).abs() < 0.01,
            "spend_priority {priority} is not the hand-computed 1.5958"
        );
        // A exhausts at 3.28 cycles against 0.30 left, B at 1.80 against 0.50.
        assert_eq!(selection.runway, Runway::ThroughReset);
    }

    /// One unmeasurable bounding window hides the minimum even beside a
    /// window that projects exhaustion on its own; the measurable scalar
    /// still publishes.
    #[test]
    fn an_unmeasurable_window_makes_runway_unknown_beside_a_projected_one() {
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 18_000_000_000_000);
        let windows = [
            window(500_000, known(reset_nanos), 10, Some(2.0)),
            window(180_000, known(reset_nanos), 10, None),
        ];
        let selection = compute_selection(&windows, &fresh_reading(), now);

        assert_eq!(selection.runway, Runway::Unknown);
        assert_eq!(selection.usable_runway_secs, None);
        let priority = selection
            .spend_priority
            .expect("the measurable window still publishes");
        assert!(
            (priority - -1.0).abs() < 1e-9,
            "spend_priority {priority} is not the measurable window's -1.0"
        );
    }

    /// 100% remaining at zero burn with 0.001% of the cycle left gaps at
    /// 100 / 0.001 = 100_000, clamped to 100. Zero burn never exhausts, so
    /// the runway still reaches the reset.
    #[test]
    fn an_enormous_gap_clamps_to_the_symmetric_bound() {
        let now = UtcTimestamp::from_unix_nanos(1_000_000_000_000);
        let reset_nanos = now.unix_nanos() + 360_000_000;
        let coasting = [window(0, known(reset_nanos), 10, Some(0.0))];
        let selection = compute_selection(&coasting, &fresh_reading(), now);

        assert_eq!(selection.spend_priority, Some(100.0));
        assert_eq!(selection.runway, Runway::ThroughReset);
    }

    /// 10% of the limiting window's cycle elapsed reads early; the
    /// hand-computed case above at 70% reads established.
    #[test]
    fn less_than_a_fifth_elapsed_is_early() {
        let reset_nanos = 36_000_000_000_000;
        let now = UtcTimestamp::from_unix_nanos(reset_nanos - 32_400_000_000_000);
        let young = [window(100_000, known(reset_nanos), 10, Some(0.5))];
        let selection = compute_selection(&young, &fresh_reading(), now);

        assert_eq!(selection.confidence, Confidence::Early);
    }
}
