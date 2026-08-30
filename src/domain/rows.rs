//! The row count: how many records a query or an export actually produced.
//!
//! A row count is a quantity like any other here, and it is one this project has a
//! specific reason to qualify rather than print bare. An export that wrote nine
//! hundred rows because ingestion was incomplete looks exactly like one that wrote
//! nine hundred rows because nine hundred exist, and the difference is the whole
//! subject of this codebase. Being a [`DomainQuantity`] is what lets the count live
//! inside `Qualified`, which carries the coverage and the evidence quality that tell
//! those two cases apart.
//!
//! No `Default` and no `Display`: a defaulted row count would be an unmeasured zero
//! wearing the shape of a measurement, and rendering belongs to presentation with an
//! explicit context.

use std::ops::{Add, Mul, Sub};

use crate::domain::interval::DomainQuantity;

/// A count of rows, in rows.
///
/// Saturating arithmetic, matching the token counts: a count is unsigned, and the
/// alternative to saturation is a panic or a wrap, neither of which is a better
/// answer for a number that is about to be reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowCount(u64);

impl RowCount {
    /// Constructs from a raw count. No range is rejected: an unsigned integer's own
    /// range is this type's range.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw count this newtype wraps.
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl Add for RowCount {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl Sub for RowCount {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

impl Mul for RowCount {
    type Output = Self;

    fn mul(self, other: Self) -> Self {
        Self(self.0.saturating_mul(other.0))
    }
}

impl DomainQuantity for RowCount {
    fn unit() -> &'static str {
        "rows"
    }

    fn to_f64(self) -> f64 {
        self.0 as f64
    }

    fn from_f64(value: f64) -> Self {
        Self(value.max(0.0).min(u64::MAX as f64) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_is_rows_and_the_value_round_trips() {
        assert_eq!(RowCount::unit(), "rows");
        assert_eq!(RowCount::from_f64(RowCount::new(42).to_f64()).value(), 42);
    }

    #[test]
    fn a_negative_primitive_reconstructs_as_zero_rather_than_wrapping() {
        assert_eq!(RowCount::from_f64(-1.0).value(), 0);
    }

    #[test]
    fn arithmetic_saturates_at_both_ends() {
        assert_eq!(
            (RowCount::new(u64::MAX) + RowCount::new(1)).value(),
            u64::MAX
        );
        assert_eq!((RowCount::new(1) - RowCount::new(2)).value(), 0);
    }
}
