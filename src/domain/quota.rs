//! Quota fractions in parts per million, with distinct level and delta types.
//!
//! A percentage level and a percentage-point delta are different quantities: a level is
//! an unsigned fraction of a quota in `[0, 1_000_000]`, and a delta is a signed
//! difference in `[-1_000_000, 1_000_000]`. `QuotaUsed` and `QuotaRemaining` are
//! distinct despite sharing a display unit, because they are complements and mixing
//! them inverts a decision without changing the shape of anything.

use std::ops::Sub;

/// A quota fraction stored in parts per million: 0 is 0% and 1_000_000 is 100%.
///
/// This is the level type. It deliberately has no `Add` implementation, so adding two
/// levels is a compile error rather than a runtime one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QuotaFractionPpm(u32);

impl QuotaFractionPpm {
    /// The largest representable fraction, 100%.
    pub const MAX: u32 = 1_000_000;

    /// Constructs a fraction, rejecting values outside `[0, 1_000_000]`.
    ///
    /// The parameter is signed so a negative candidate is rejected here rather than
    /// being unrepresentable at the call site.
    pub fn new(value: i32) -> Option<Self> {
        (0..=Self::MAX as i32)
            .contains(&value)
            .then_some(Self(value as u32))
    }

    /// The raw parts-per-million value.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Sub for QuotaFractionPpm {
    type Output = PercentagePoints;

    fn sub(self, rhs: Self) -> Self::Output {
        // Both operands are in [0, 1_000_000], so the difference is always in
        // [-1_000_000, 1_000_000] and therefore always representable.
        PercentagePoints(self.0 as i32 - rhs.0 as i32)
    }
}

/// The used portion of a quota, in parts per million.
///
/// Distinct from [`QuotaRemaining`] even though both share a display unit. The only
/// route between them is [`QuotaUsed::complement`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QuotaUsed(QuotaFractionPpm);

impl QuotaUsed {
    pub fn new(ppm: QuotaFractionPpm) -> Self {
        Self(ppm)
    }

    pub fn as_ppm(self) -> QuotaFractionPpm {
        self.0
    }

    /// The remaining fraction: `1_000_000 - used`.
    pub fn complement(self) -> QuotaRemaining {
        QuotaRemaining(QuotaFractionPpm(QuotaFractionPpm::MAX - self.0.get()))
    }
}

/// The remaining portion of a quota, in parts per million.
///
/// Distinct from [`QuotaUsed`]; see that type's documentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QuotaRemaining(QuotaFractionPpm);

impl QuotaRemaining {
    pub fn new(ppm: QuotaFractionPpm) -> Self {
        Self(ppm)
    }

    pub fn as_ppm(self) -> QuotaFractionPpm {
        self.0
    }

    /// The used fraction: `1_000_000 - remaining`.
    pub fn complement(self) -> QuotaUsed {
        QuotaUsed(QuotaFractionPpm(QuotaFractionPpm::MAX - self.0.get()))
    }
}

/// A percentage-point delta, signed fixed point in parts per million.
///
/// The range is `[-1_000_000, 1_000_000]`: the difference between any two levels in
/// `[0, 1_000_000]` is always representable here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PercentagePoints(i32);

impl PercentagePoints {
    pub const MIN: i32 = -1_000_000;
    pub const MAX: i32 = 1_000_000;

    pub fn new(value: i32) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&value)
            .then_some(Self(value))
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random sample of the level range, so the property tests
    /// run over many values without a property-testing dependency.
    fn sample_levels() -> impl Iterator<Item = u32> {
        let mut state = 0x9e37_79b9u32;
        std::iter::from_fn(move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Some(state % (QuotaFractionPpm::MAX + 1))
        })
    }

    #[test]
    fn construction_accepts_the_boundaries_and_rejects_outside() {
        assert_eq!(QuotaFractionPpm::new(0).map(QuotaFractionPpm::get), Some(0));
        assert_eq!(
            QuotaFractionPpm::new(1_000_000).map(QuotaFractionPpm::get),
            Some(1_000_000)
        );
        assert_eq!(QuotaFractionPpm::new(-1), None);
        assert_eq!(QuotaFractionPpm::new(1_000_001), None);
    }

    #[test]
    fn percentage_points_accepts_its_boundaries_and_rejects_outside() {
        assert_eq!(
            PercentagePoints::new(-1_000_000).map(PercentagePoints::get),
            Some(-1_000_000)
        );
        assert_eq!(
            PercentagePoints::new(1_000_000).map(PercentagePoints::get),
            Some(1_000_000)
        );
        assert_eq!(PercentagePoints::new(-1_000_001), None);
        assert_eq!(PercentagePoints::new(1_000_001), None);
    }

    #[test]
    fn subtracting_levels_yields_a_delta() {
        let zero = QuotaFractionPpm::new(0).unwrap();
        let full = QuotaFractionPpm::new(1_000_000).unwrap();
        // 0% - 100% == -100pp.
        assert_eq!(zero - full, PercentagePoints::new(-1_000_000).unwrap());
        // 100% - 0% == +100pp.
        assert_eq!(full - zero, PercentagePoints::new(1_000_000).unwrap());
    }

    #[test]
    fn subtraction_is_antisymmetric() {
        let a = QuotaFractionPpm::new(250_000).unwrap();
        let b = QuotaFractionPpm::new(750_000).unwrap();
        assert_eq!((a - b).get(), -(b - a).get());
    }

    #[test]
    fn complement_is_the_named_route_between_used_and_remaining() {
        let used = QuotaUsed::new(QuotaFractionPpm::new(300_000).unwrap());
        let remaining = used.complement();
        assert_eq!(remaining.as_ppm().get(), 700_000);
        assert_eq!(remaining.complement(), used);
    }

    #[test]
    fn complement_applied_twice_is_the_identity() {
        for ppm in 0..=QuotaFractionPpm::MAX {
            let used = QuotaUsed::new(QuotaFractionPpm::new(ppm as i32).unwrap());
            assert_eq!(used.complement().complement(), used);
        }
    }

    #[test]
    fn level_minus_level_is_always_a_representable_delta() {
        let levels: Vec<QuotaFractionPpm> = sample_levels()
            .take(200)
            .map(|ppm| QuotaFractionPpm::new(ppm as i32).unwrap())
            .collect();
        for &a in &levels {
            for &b in &levels {
                let delta = a - b;
                assert!(
                    (PercentagePoints::MIN..=PercentagePoints::MAX).contains(&delta.get()),
                    "delta out of range: {a:?} - {b:?}"
                );
            }
        }
    }
}
