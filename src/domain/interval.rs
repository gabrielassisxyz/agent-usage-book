//! A closed interval over an ordered domain quantity, with arithmetic that
//! propagates uncertainty rather than collapsing it.
//!
//! Intervals appear wherever a scalar would be a lie: a calibration
//! coefficient with its uncertainty, a quantized provider reading, an
//! empirical range of historical task consumption, and the credit headroom
//! that `can-run` compares against. The rule this type enforces is that
//! widening cannot narrow: no operation silently returns a midpoint.

use std::fmt;
use std::ops::{Add, Mul, Sub};
use std::str::FromStr;

/// A domain quantity: a newtype over a numeric primitive that carries a unit
/// and is ordered. Bare primitives do not implement this trait, so
/// `Interval<f64>` does not compile.
pub trait DomainQuantity:
    Ord + Copy + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self>
{
    /// The unit this quantity is measured in, e.g. `"credits"`.
    fn unit() -> &'static str;

    /// The primitive value, for serialization.
    fn to_f64(self) -> f64;

    /// Reconstructs the quantity from a primitive value, for deserialization.
    fn from_f64(value: f64) -> Self;
}

/// A closed interval `[lower, upper]` over an ordered domain quantity.
///
/// Both endpoints are retained and exposed; there is deliberately no midpoint
/// accessor, because a midpoint is the scalar this type exists to avoid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval<T: DomainQuantity> {
    lower: T,
    upper: T,
}

/// Why an interval could not be constructed or deserialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalError {
    /// The lower endpoint is above the upper endpoint.
    LowerAboveUpper,
    /// The serialized form is malformed.
    Malformed,
    /// The serialized unit does not match the element type's unit.
    UnitMismatch,
}

impl<T: DomainQuantity> Interval<T> {
    /// Constructs an interval, rejecting a lower endpoint above its upper.
    pub fn new(lower: T, upper: T) -> Result<Self, IntervalError> {
        if lower > upper {
            return Err(IntervalError::LowerAboveUpper);
        }
        Ok(Self { lower, upper })
    }

    /// The lower endpoint.
    pub fn lower(&self) -> T {
        self.lower
    }

    /// The upper endpoint.
    pub fn upper(&self) -> T {
        self.upper
    }

    /// The width of the interval: `upper - lower`.
    pub fn width(&self) -> T {
        self.upper - self.lower
    }
}

/// Interval addition: `[a, b] + [c, d] = [a + c, b + d]`.
impl<T: DomainQuantity> Add for Interval<T> {
    type Output = Interval<T>;

    fn add(self, rhs: Self) -> Self::Output {
        Interval {
            lower: self.lower + rhs.lower,
            upper: self.upper + rhs.upper,
        }
    }
}

/// Interval subtraction: `[a, b] - [c, d] = [a - d, b - c]`.
impl<T: DomainQuantity> Sub for Interval<T> {
    type Output = Interval<T>;

    fn sub(self, rhs: Self) -> Self::Output {
        Interval {
            lower: self.lower - rhs.upper,
            upper: self.upper - rhs.lower,
        }
    }
}

/// Interval multiplication: the enclosing interval over all four endpoint
/// products, correct for every sign combination of the endpoints.
impl<T: DomainQuantity> Mul for Interval<T> {
    type Output = Interval<T>;

    fn mul(self, rhs: Self) -> Self::Output {
        let products = [
            self.lower * rhs.lower,
            self.lower * rhs.upper,
            self.upper * rhs.lower,
            self.upper * rhs.upper,
        ];
        let lower = *products.iter().min().expect("four products");
        let upper = *products.iter().max().expect("four products");
        Interval { lower, upper }
    }
}

/// Scalar multiplication: `s * [a, b]`. The width is `abs(s) * width`, so a
/// scalar with magnitude below one may shrink the interval.
impl<T: DomainQuantity> Mul<T> for Interval<T> {
    type Output = Interval<T>;

    fn mul(self, rhs: T) -> Self::Output {
        let a = self.lower * rhs;
        let b = self.upper * rhs;
        if a <= b {
            Interval { lower: a, upper: b }
        } else {
            Interval { lower: b, upper: a }
        }
    }
}

/// Serialization retains both endpoints and the unit, so a consumer never
/// reads an admissible range as a single measurement.
impl<T: DomainQuantity> fmt::Display for Interval<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}, {}] {}",
            self.lower.to_f64(),
            self.upper.to_f64(),
            T::unit()
        )
    }
}

/// Deserialization of the form produced by [`Display`], rejecting a unit that
/// does not match the element type.
impl<T: DomainQuantity> FromStr for Interval<T> {
    type Err = IntervalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let rest = s.strip_prefix('[').ok_or(IntervalError::Malformed)?;
        let (lower_str, rest) = rest.split_once(',').ok_or(IntervalError::Malformed)?;
        let (upper_str, unit) = rest.split_once(']').ok_or(IntervalError::Malformed)?;
        let lower: f64 = lower_str
            .trim()
            .parse()
            .map_err(|_| IntervalError::Malformed)?;
        let upper: f64 = upper_str
            .trim()
            .parse()
            .map_err(|_| IntervalError::Malformed)?;
        let unit = unit.trim();
        if unit != T::unit() {
            return Err(IntervalError::UnitMismatch);
        }
        Self::new(T::from_f64(lower), T::from_f64(upper))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// A test-only domain quantity over `f64`, so the arithmetic can be
    /// exercised before the real quantities exist.
    #[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
    struct TestQuantity(f64);

    impl Ord for TestQuantity {
        fn cmp(&self, other: &Self) -> Ordering {
            self.0.partial_cmp(&other.0).expect("no NaN in tests")
        }
    }

    impl Eq for TestQuantity {}

    impl Add for TestQuantity {
        type Output = Self;
        fn add(self, rhs: Self) -> Self {
            Self(self.0 + rhs.0)
        }
    }

    impl Sub for TestQuantity {
        type Output = Self;
        fn sub(self, rhs: Self) -> Self {
            Self(self.0 - rhs.0)
        }
    }

    impl Mul for TestQuantity {
        type Output = Self;
        fn mul(self, rhs: Self) -> Self {
            Self(self.0 * rhs.0)
        }
    }

    impl DomainQuantity for TestQuantity {
        fn unit() -> &'static str {
            "test"
        }

        fn to_f64(self) -> f64 {
            self.0
        }

        fn from_f64(value: f64) -> Self {
            Self(value)
        }
    }

    fn q(value: f64) -> TestQuantity {
        TestQuantity(value)
    }

    fn interval(lower: f64, upper: f64) -> Interval<TestQuantity> {
        Interval::new(q(lower), q(upper)).expect("valid interval")
    }

    /// A hand-computed multiplication case: `[a, b] * [c, d]` encloses all
    /// four endpoint products.
    fn assert_mul(a: f64, b: f64, c: f64, d: f64, expected_lower: f64, expected_upper: f64) {
        let result = interval(a, b) * interval(c, d);
        assert_eq!(result.lower(), q(expected_lower));
        assert_eq!(result.upper(), q(expected_upper));
    }

    #[test]
    fn multiplication_covers_all_four_sign_combinations() {
        // Both positive.
        assert_mul(1.0, 2.0, 3.0, 4.0, 3.0, 8.0);
        // Both negative.
        assert_mul(-2.0, -1.0, -4.0, -3.0, 3.0, 8.0);
        // Left straddles zero, right positive.
        assert_mul(-1.0, 2.0, 3.0, 4.0, -4.0, 8.0);
        // Left positive, right straddles zero.
        assert_mul(1.0, 2.0, -4.0, 3.0, -8.0, 6.0);
        // Both straddle zero.
        assert_mul(-1.0, 2.0, -4.0, 3.0, -8.0, 6.0);
    }

    #[test]
    fn construction_rejects_lower_above_upper() {
        assert_eq!(
            Interval::new(q(2.0), q(1.0)),
            Err(IntervalError::LowerAboveUpper)
        );
        assert!(Interval::new(q(1.0), q(1.0)).is_ok());
    }

    #[test]
    fn no_midpoint_accessor_exists() {
        // The type exposes lower and upper and nothing that collapses them.
        let i = interval(1.0, 3.0);
        assert_eq!(i.lower(), q(1.0));
        assert_eq!(i.upper(), q(3.0));
        assert_eq!(i.width(), q(2.0));
    }

    /// Enclosure under inclusion: if `A` is contained in `B`, then applying
    /// the same operation with the same operand keeps the result of `A`
    /// contained in the result of `B`.
    #[test]
    fn operations_preserve_enclosure_under_inclusion() {
        // A is contained in B: B.lower <= A.lower and A.upper <= B.upper.
        let a = interval(2.0, 3.0);
        let b = interval(1.0, 4.0);
        let c = interval(5.0, 6.0);

        let add_a = a + c;
        let add_b = b + c;
        assert!(add_a.lower() >= add_b.lower());
        assert!(add_a.upper() <= add_b.upper());

        let sub_a = a - c;
        let sub_b = b - c;
        assert!(sub_a.lower() >= sub_b.lower());
        assert!(sub_a.upper() <= sub_b.upper());

        let mul_a = a * c;
        let mul_b = b * c;
        assert!(mul_a.lower() >= mul_b.lower());
        assert!(mul_a.upper() <= mul_b.upper());

        let scalar = q(2.0);
        let smul_a = a * scalar;
        let smul_b = b * scalar;
        assert!(smul_a.lower() >= smul_b.lower());
        assert!(smul_a.upper() <= smul_b.upper());
    }

    #[test]
    fn scalar_multiplication_width_is_abs_scalar_times_width() {
        let i = interval(2.0, 5.0);
        let doubled = i * q(2.0);
        assert_eq!(doubled.width(), q(6.0));
        let halved = i * q(0.5);
        assert_eq!(halved.width(), q(1.5));
        let negated = i * q(-1.0);
        assert_eq!(negated.lower(), q(-5.0));
        assert_eq!(negated.upper(), q(-2.0));
    }

    #[test]
    fn serialization_round_trip_preserves_endpoints_and_unit() {
        let i = interval(1.5, 2.5);
        let text = i.to_string();
        assert_eq!(text, "[1.5, 2.5] test");
        let parsed: Interval<TestQuantity> = text.parse().expect("round trip");
        assert_eq!(parsed, i);
    }

    #[test]
    fn deserialization_rejects_a_unit_mismatch() {
        let result: Result<Interval<TestQuantity>, _> = "[1.5, 2.5] credits".parse();
        assert_eq!(result, Err(IntervalError::UnitMismatch));
    }
}
