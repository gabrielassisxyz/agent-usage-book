//! Deriving a quota window's burn rate, and its cap-freeze, from the window's
//! observation series.
//!
//! The rate itself is never stored: it is a derivation over the used fraction
//! and the elapsed fraction, computed when a report is assembled. What the cap
//! freeze needs that a single observation cannot give is *when* the window first
//! reached its cap in the current reset cycle; [`frozen_burn_rate`] takes that
//! series and returns the rate as it stood at that instant, so a capped window
//! shows the pace it capped at rather than a number that decays toward `1.00x`
//! as the clock keeps running.

use crate::domain::burn_rate::BurnRate;
use crate::domain::quota::QuotaUsed;
use crate::domain::time::UtcTimestamp;
use crate::domain::window::{NominalWindowDuration, WindowResetState};

/// The parts-per-million value a window's quota-used reaches at its cap.
const CAP_PPM: u32 = 1_000_000;

/// The fraction of a window that has elapsed at `now`.
///
/// The window ends at its `Known` reset instant and began one nominal duration
/// earlier. A `NotStarted` window has no elapsed fraction, and neither does one
/// whose nominal duration is zero. The result is clamped to `[0.0, 1.0]`: a
/// reading taken at or after the stated reset belongs to the completed window
/// and reports as fully elapsed.
pub fn elapsed_fraction(
    reset: WindowResetState,
    nominal: NominalWindowDuration,
    now: UtcTimestamp,
) -> Option<f64> {
    let reset_at = reset.instant()?;
    let nominal_nanos = nominal.as_nanos();
    if nominal_nanos == 0 {
        return None;
    }
    let window_start = reset_at.unix_nanos() - nominal_nanos as i64;
    let elapsed_nanos = now.unix_nanos() - window_start;
    let fraction = elapsed_nanos as f64 / nominal_nanos as f64;
    Some(fraction.clamp(0.0, 1.0))
}

/// The live burn rate of a window from its single current observation.
///
/// `None` for a `NotStarted` window, for a sub-1%-elapsed window that is not
/// capped, and — deliberately — for a window already at its cap: a capped
/// window's rate is the one frozen when it capped ([`frozen_burn_rate`]), which
/// needs the observation series, and recomputing it at `now` would decay it
/// toward `1.00x`. `aub status` reads only the projection's latest observation,
/// so for a capped window it reports no rate rather than a wrong one.
pub fn live_burn_rate(
    used_ppm: QuotaUsed,
    reset: WindowResetState,
    nominal: NominalWindowDuration,
    now: UtcTimestamp,
) -> Option<BurnRate> {
    let ppm = used_ppm.as_ppm().get();
    if ppm >= CAP_PPM {
        return None;
    }
    BurnRate::from_window(ppm, elapsed_fraction(reset, nominal, now))
}

/// One observation of a window: when it was received and the quota it reported
/// used. The unit the freeze walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowObservation {
    pub received_at: UtcTimestamp,
    pub used_ppm: QuotaUsed,
}

/// The burn rate frozen at the instant a window first reached its cap in the
/// current reset cycle, paired with that instant.
///
/// `series` is the window's observations in received order. Only observations
/// whose `received_at` is strictly after `cycle_start` count, so an observation
/// left over from the previous cycle (still reporting the old cap) is not
/// mistaken for this cycle's freeze. The rate is computed from the elapsed
/// fraction *at the freeze instant*, using `reset` (this cycle's reset) and
/// `nominal`. `None` when no observation in the cycle reached the cap.
pub fn frozen_burn_rate(
    series: &[WindowObservation],
    cycle_start: UtcTimestamp,
    reset: WindowResetState,
    nominal: NominalWindowDuration,
) -> Option<(UtcTimestamp, BurnRate)> {
    let freeze = series
        .iter()
        .filter(|observation| observation.received_at.unix_nanos() > cycle_start.unix_nanos())
        .find(|observation| observation.used_ppm.as_ppm().get() >= CAP_PPM)?;
    let elapsed = elapsed_fraction(reset, nominal, freeze.received_at);
    let rate = BurnRate::from_window(freeze.used_ppm.as_ppm().get(), elapsed)?;
    Some((freeze.received_at, rate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quota::QuotaFractionPpm;

    const HOUR_NANOS: i64 = 3_600_000_000_000;

    fn used(ppm: i32) -> QuotaUsed {
        QuotaUsed::new(QuotaFractionPpm::new(ppm).unwrap())
    }

    fn reset_at(nanos: i64) -> WindowResetState {
        WindowResetState::Known(UtcTimestamp::from_unix_nanos(nanos))
    }

    fn nominal_hours(hours: i64) -> NominalWindowDuration {
        NominalWindowDuration::from_nanos((hours * HOUR_NANOS) as u64)
    }

    #[test]
    fn elapsed_fraction_is_the_share_of_the_window_behind_now() {
        // A 10-hour window resetting at t=10h started at t=0; at t=2h, 20% elapsed.
        let fraction = elapsed_fraction(
            reset_at(10 * HOUR_NANOS),
            nominal_hours(10),
            UtcTimestamp::from_unix_nanos(2 * HOUR_NANOS),
        )
        .unwrap();
        assert!((fraction - 0.2).abs() < 1e-9);
    }

    #[test]
    fn elapsed_fraction_clamps_and_has_no_value_for_a_not_started_window() {
        // After the reset instant: fully elapsed, not more.
        let past = elapsed_fraction(
            reset_at(10 * HOUR_NANOS),
            nominal_hours(10),
            UtcTimestamp::from_unix_nanos(99 * HOUR_NANOS),
        )
        .unwrap();
        assert_eq!(past, 1.0);
        assert!(
            elapsed_fraction(
                WindowResetState::NotStarted,
                nominal_hours(10),
                UtcTimestamp::from_unix_nanos(0),
            )
            .is_none()
        );
    }

    #[test]
    fn live_rate_is_used_over_elapsed_and_two_x_at_forty_percent_over_twenty() {
        // 40% used at 20% elapsed -> 2.00x.
        let rate = live_burn_rate(
            used(400_000),
            reset_at(10 * HOUR_NANOS),
            nominal_hours(10),
            UtcTimestamp::from_unix_nanos(2 * HOUR_NANOS),
        )
        .unwrap();
        assert_eq!(rate.to_string(), "2.00x");
    }

    #[test]
    fn live_rate_is_absent_for_a_capped_window() {
        // The live path never reports a decaying number for a capped window.
        assert!(
            live_burn_rate(
                used(1_000_000),
                reset_at(10 * HOUR_NANOS),
                nominal_hours(10),
                UtcTimestamp::from_unix_nanos(9 * HOUR_NANOS),
            )
            .is_none()
        );
    }

    #[test]
    fn frozen_rate_is_the_rate_at_the_cap_instant_not_at_now() {
        // Window resets at t=10h, nominal 10h, so it started at t=0.
        // Series: 60% at t=3h, 100% first seen at t=5h (50% elapsed), still
        // 100% at t=9h. The freeze is t=5h and the rate is 1.0 / 0.5 = 2.00x,
        // not the 1.0 / 0.9 = 1.11x a recompute at t=9h would give.
        let series = [
            WindowObservation {
                received_at: UtcTimestamp::from_unix_nanos(3 * HOUR_NANOS),
                used_ppm: used(600_000),
            },
            WindowObservation {
                received_at: UtcTimestamp::from_unix_nanos(5 * HOUR_NANOS),
                used_ppm: used(1_000_000),
            },
            WindowObservation {
                received_at: UtcTimestamp::from_unix_nanos(9 * HOUR_NANOS),
                used_ppm: used(1_000_000),
            },
        ];
        let (capped_at, rate) = frozen_burn_rate(
            &series,
            UtcTimestamp::from_unix_nanos(0),
            reset_at(10 * HOUR_NANOS),
            nominal_hours(10),
        )
        .unwrap();
        assert_eq!(capped_at, UtcTimestamp::from_unix_nanos(5 * HOUR_NANOS));
        assert_eq!(rate.to_string(), "2.00x");
    }

    #[test]
    fn frozen_rate_ignores_a_cap_from_the_previous_cycle_until_the_new_cycle_caps() {
        // A cap at t=4h belongs to the previous cycle. The new cycle starts at
        // t=6h (its reset instant is t=16h, nominal 10h). Until the new cycle's
        // own observations reach the cap there is no freeze; once one does at
        // t=11h (50% elapsed of the new window) the freeze is t=11h.
        let previous_cycle_cap = WindowObservation {
            received_at: UtcTimestamp::from_unix_nanos(4 * HOUR_NANOS),
            used_ppm: used(1_000_000),
        };
        let new_cycle_start = UtcTimestamp::from_unix_nanos(6 * HOUR_NANOS);
        let new_reset = reset_at(16 * HOUR_NANOS);

        let before_new_cap = [previous_cycle_cap];
        assert!(
            frozen_burn_rate(
                &before_new_cap,
                new_cycle_start,
                new_reset,
                nominal_hours(10)
            )
            .is_none()
        );

        let after_new_cap = [
            previous_cycle_cap,
            WindowObservation {
                received_at: UtcTimestamp::from_unix_nanos(11 * HOUR_NANOS),
                used_ppm: used(1_000_000),
            },
        ];
        let (capped_at, rate) = frozen_burn_rate(
            &after_new_cap,
            new_cycle_start,
            new_reset,
            nominal_hours(10),
        )
        .unwrap();
        assert_eq!(capped_at, UtcTimestamp::from_unix_nanos(11 * HOUR_NANOS));
        assert_eq!(rate.to_string(), "2.00x");
    }
}
