//! The burn rate of a quota window: quota consumed divided by the fraction of
//! the window that has elapsed.
//!
//! `1.00x` means the window is on pace to reach its cap exactly at the reset,
//! `2.00x` means twice that pace, `0.39x` means the window is coasting. A window
//! that has already reached its cap keeps the rate it had at the instant it
//! capped rather than a number that decays toward `1.00x` as the clock keeps
//! running; that freeze is the report layer's job, computed from the observation
//! series (`crate::report::burn_rate`), and this type only holds the ratio.
//!
//! The ratio is two fractions divided, so it carries no unit. Unlike the Phase 0
//! quantities it needs no coverage, freshness or precision context to be read,
//! which is why it carries its own `Display` (`Nx`) instead of routing through a
//! presentation helper. `docs/domain-quantity-inventory.md` records that
//! deviation. The representation is fixed point in millionths of `x`, the same
//! parts-per-million idiom `QuotaFractionPpm` and `PercentagePoints` use, so the
//! type is `Eq` and `Ord` and slots into a report model without forcing a float.

use std::fmt;

/// A window burn rate: a finite, non-negative ratio of quota-used fraction to
/// elapsed-window fraction, stored as millionths of `x`.
///
/// Private representation with a checked constructor: a negative, non-finite or
/// absurdly large candidate never becomes a `BurnRate`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BurnRate(u64);

impl BurnRate {
    /// Millionths of `x` per whole `x`.
    const MICRO_PER_X: u64 = 1_000_000;

    /// The parts-per-million value a window's `quota_used` reaches at its cap.
    const CAP_PPM: u32 = 1_000_000;

    /// The largest ratio [`Self::new`] accepts, in whole `x`. A burn rate beyond
    /// this is a computation bug, not a fast window.
    const MAX_X: f64 = 1_000_000.0;

    /// The smallest elapsed fraction that yields a rate for a window that is not
    /// yet capped. Below this the divisor is so small that the rate is dominated
    /// by sampling jitter rather than by consumption, the same guard
    /// `bin/quota-bars` applies. A capped window is exempt: its rate at the cap
    /// instant is meaningful however little of the window had elapsed.
    pub const MIN_ELAPSED_FRACTION: f64 = 0.01;

    /// Millionths of `x` below which a non-zero rate renders as `<0.01x` rather
    /// than as `0.00x`. `0.005x` exactly (`5_000`) is above it and renders as
    /// `0.01x`.
    const NEAR_ZERO_MICRO: u64 = 5_000;

    /// Constructs a rate, rejecting a negative, non-finite or absurdly large
    /// candidate.
    pub fn new(value: f64) -> Option<Self> {
        if !value.is_finite() || !(0.0..=Self::MAX_X).contains(&value) {
            return None;
        }
        Some(Self((value * Self::MICRO_PER_X as f64).round() as u64))
    }

    /// The ratio as a floating-point value.
    pub fn get(self) -> f64 {
        self.0 as f64 / Self::MICRO_PER_X as f64
    }

    /// The ratio in millionths of `x`, the exact stored value.
    pub fn micro_x(self) -> u64 {
        self.0
    }

    /// An exact decimal string of the ratio, for a JSON value: `"2.000000"`.
    pub fn as_decimal_string(self) -> String {
        format!(
            "{}.{:06}",
            self.0 / Self::MICRO_PER_X,
            self.0 % Self::MICRO_PER_X
        )
    }

    /// The rate of a window from its used parts-per-million and the fraction of
    /// the window that has elapsed.
    ///
    /// `elapsed_fraction` is `None` when the window has no elapsed fraction to
    /// speak of: a `NotStarted` reset state, where the report layer passes
    /// `None` and this returns `None`. When it is `Some`, a fraction below
    /// [`Self::MIN_ELAPSED_FRACTION`] yields `None` unless the window is already
    /// at its cap.
    pub fn from_window(used_ppm: u32, elapsed_fraction: Option<f64>) -> Option<Self> {
        let elapsed = elapsed_fraction?;
        let capped = used_ppm >= Self::CAP_PPM;
        if !capped && elapsed < Self::MIN_ELAPSED_FRACTION {
            return None;
        }
        if elapsed <= 0.0 {
            return None;
        }
        let used_fraction = f64::from(used_ppm) / f64::from(Self::CAP_PPM);
        Self::new(used_fraction / elapsed)
    }
}

/// Two decimals and an `x` suffix (`0.88x`), rounding half up. A rate in the
/// open interval `(0, 0.005)` renders as `<0.01x` so a real but tiny burn is
/// never shown as `0.00x`; a rate of exactly zero renders as `0.00x`.
impl fmt::Display for BurnRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 > 0 && self.0 < Self::NEAR_ZERO_MICRO {
            return write!(f, "<0.01x");
        }
        let hundredths = (self.0 + 5_000) / 10_000;
        write!(f, "{}.{:02}x", hundredths / 100, hundredths % 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random sample of the elapsed-fraction range, so
    /// the property test runs over many values without a property-testing
    /// dependency, the convention `src/domain/quota.rs` established for this
    /// crate. Values land in `[0, 1]`; the caller rescales to `[0.01, 1.0]`.
    fn sample_fractions() -> impl Iterator<Item = f64> {
        let mut state = 0x9e37_79b9u32;
        std::iter::from_fn(move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Some(f64::from(state % 1_000_001) / 1_000_000.0)
        })
    }

    #[test]
    fn construction_rejects_negative_non_finite_and_absurd() {
        assert!(BurnRate::new(0.0).is_some());
        assert!(BurnRate::new(4.06).is_some());
        assert!(BurnRate::new(-0.001).is_none());
        assert!(BurnRate::new(f64::NAN).is_none());
        assert!(BurnRate::new(f64::INFINITY).is_none());
        assert!(BurnRate::new(2_000_000.0).is_none());
    }

    #[test]
    fn display_rounds_at_the_four_boundaries() {
        assert_eq!(BurnRate::new(0.0).unwrap().to_string(), "0.00x");
        assert_eq!(BurnRate::new(0.004).unwrap().to_string(), "<0.01x");
        assert_eq!(BurnRate::new(0.005).unwrap().to_string(), "0.01x");
        assert_eq!(BurnRate::new(1.0).unwrap().to_string(), "1.00x");
    }

    #[test]
    fn display_two_decimals_and_x() {
        assert_eq!(BurnRate::new(0.88).unwrap().to_string(), "0.88x");
        assert_eq!(BurnRate::new(2.0).unwrap().to_string(), "2.00x");
        assert_eq!(BurnRate::new(4.061).unwrap().to_string(), "4.06x");
    }

    #[test]
    fn from_window_applies_the_elapsed_guard_unless_capped() {
        // 40% used at 20% elapsed is 2.00x.
        let rate = BurnRate::from_window(400_000, Some(0.2)).unwrap();
        assert_eq!(rate.to_string(), "2.00x");

        // Below 1% elapsed and not capped: no rate.
        assert!(BurnRate::from_window(400_000, Some(0.004)).is_none());

        // Below 1% elapsed but capped: the rate at that instant still stands.
        assert!(BurnRate::from_window(1_000_000, Some(0.004)).is_some());
    }

    #[test]
    fn from_window_has_no_rate_for_a_not_started_window() {
        assert!(BurnRate::from_window(0, None).is_none());
        assert!(BurnRate::from_window(500_000, None).is_none());
    }

    #[test]
    fn decimal_string_is_exact_fixed_point() {
        assert_eq!(BurnRate::new(2.0).unwrap().as_decimal_string(), "2.000000");
        assert_eq!(
            BurnRate::new(0.004).unwrap().as_decimal_string(),
            "0.004000"
        );
    }

    #[test]
    fn from_window_is_finite_non_negative_and_monotonic_over_the_sample() {
        for sample in sample_fractions().take(64) {
            let elapsed = 0.01 + sample * 0.99; // [0.01, 1.0]
            let mut previous: Option<u64> = None;
            for used_ppm in (0..=1_000_000).step_by(25_000) {
                let rate = BurnRate::from_window(used_ppm, Some(elapsed))
                    .expect("a fraction in [0.01, 1.0] always yields a rate");
                assert!(rate.get().is_finite() && rate.get() >= 0.0);
                if let Some(previous) = previous {
                    assert!(
                        rate.micro_x() >= previous,
                        "rate must not decrease as used_ppm grows at a fixed elapsed fraction"
                    );
                }
                previous = Some(rate.micro_x());
            }
        }
    }
}
