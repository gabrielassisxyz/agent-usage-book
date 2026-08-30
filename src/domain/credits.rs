//! Credits: this project's own accounting unit, and the two coefficients that convert
//! into and out of it.
//!
//! A usage vector becomes credits through a cost model (`CreditsPerToken`), and credits
//! become quota-window percentage points through a calibration
//! (`CreditsPerPercentagePoint`). Those are two physically distinct unknowns, fit from
//! two different kinds of evidence, and this module refuses to collapse them into one
//! regression: `CreditsPerToken` and `CreditsPerPercentagePoint` have the same
//! underlying arithmetic and completely different meanings, and a codebase that
//! represents both as `f64` eventually multiplies by the wrong one. Keeping them as
//! distinct types with no `From`/`Into` between them, and no operator between wrong
//! pairs, makes that refusal mechanical instead of a matter of discipline.
//!
//! All three types are stored as scaled integers (`i64` micro-units), matching
//! `Money`'s convention: no binary floating point at the persistence boundary, so
//! aggregation order cannot move the last digit. Regression internals elsewhere may use
//! floating point; a fitted result is converted into one of these typed quantities
//! before it leaves the calibration module.
//!
//! None of the three implements `Default`: a default credit or coefficient would be a
//! silent, meaningless zero standing in for "nobody set this yet." None implements a
//! free-standing `Display` either; rendering takes a presentation helper with explicit
//! context, the same rule `Money` follows.
//!
//! Production construction of the two coefficient types is `pub(crate)`, restricting it
//! to this crate and out of reach of a compile-fail fixture, which always compiles as a
//! separate external crate. Rust's visibility system has no way to name "exactly the
//! `calibration` and `store` modules and no other" from here: `pub(in path)` requires
//! `path` to be an ancestor of this module, and `calibration`/`store`/`domain` are
//! flat siblings under the crate root, so `pub(crate)` is the tightest boundary this
//! module can declare on its own. The convention that only `calibration` and `store`
//! actually call these constructors is not yet compiler-enforced *within* the crate;
//! `aub-rif.12` ("private witnesses") is where that gap is meant to close, mechanically
//! rather than by discipline, once it lands.

use super::quota::PercentagePoints;

/// Rounds `numerator / denominator` to the nearest integer, ties away from zero. The
/// same policy `Money` uses, applied at the one place division happens in this module:
/// `CreditsPerToken::times_tokens`'s division by one million.
fn round_div(numerator: i128, denominator: i128) -> i128 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder.abs() * 2 >= denominator {
        if numerator >= 0 {
            quotient + 1
        } else {
            quotient - 1
        }
    } else {
        quotient
    }
}

/// This project's own accounting unit, stored as integer micro-credits (1/1_000_000 of
/// one credit) so aggregation is exact regardless of how many terms were summed or in
/// what order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Credits {
    micros: i64,
}

impl Credits {
    /// Constructs from an integer number of micro-credits.
    pub const fn from_micros(micros: i64) -> Self {
        Self { micros }
    }

    /// The amount in micro-credits.
    pub const fn micros(self) -> i64 {
        self.micros
    }
}

impl std::ops::Add for Credits {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::from_micros(self.micros + rhs.micros)
    }
}

/// Credits per token, amortized over one million tokens: the coefficient a cost model
/// fits and a usage vector is priced through.
///
/// Stored as integer micro-credits per one million tokens, so a rate of 0.000015
/// credits per token is `15` micros per million. Amortizing over a million matches
/// `MoneyPerMillionTokens`'s convention and keeps realistic per-token rates
/// representable as a whole number of micros rather than truncating to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreditsPerToken {
    micros_per_million_tokens: i64,
}

impl CreditsPerToken {
    /// Constructs from an integer number of micro-credits per one million tokens.
    ///
    /// `pub(crate)`: see the module documentation on why this cannot be restricted more
    /// narrowly than the crate boundary from here, and on what still closes the gap.
    #[allow(dead_code)] // no consumer yet: the cost-model epic (aub-ai3) wires this in.
    pub(crate) const fn from_micros_per_million_tokens(micros_per_million_tokens: i64) -> Self {
        Self {
            micros_per_million_tokens,
        }
    }

    /// The rate in micro-credits per one million tokens.
    pub const fn micros_per_million_tokens(self) -> i64 {
        self.micros_per_million_tokens
    }
}

impl std::ops::Mul<u64> for CreditsPerToken {
    type Output = Credits;

    /// Multiplies by a raw token count, yielding an exact `Credits` amount. The token
    /// count is a raw `u64`: a cost model sums the per-kind counts a `UsageVector`
    /// carries and passes the total here, the same shape `MoneyPerMillionTokens` takes.
    /// The division by one million is the single place this type's rounding policy is
    /// applied.
    fn mul(self, tokens: u64) -> Credits {
        // i128 intermediate so the multiplication cannot overflow before the division;
        // realistic rates and token counts fit comfortably in i64 afterwards.
        let numerator = i128::from(self.micros_per_million_tokens) * i128::from(tokens);
        let micros = round_div(numerator, 1_000_000);
        Credits::from_micros(micros as i64)
    }
}

/// Credits per percentage point of quota movement: the coefficient a window calibration
/// fits between observed credit spend and observed quota movement.
///
/// Stored as integer micro-credits per one [`PercentagePoints`] unit (one part per
/// million of quota movement, `PercentagePoints`' own native scale). Unlike
/// `CreditsPerToken`, multiplication here is exact integer multiplication with no
/// division: `PercentagePoints`' entire range is `[-1_000_000, 1_000_000]`, an order of
/// magnitude too small for a "per million" amortized basis to mean anything, so this
/// type is scaled directly against `PercentagePoints`' own unit instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreditsPerPercentagePoint {
    micros_per_point: i64,
}

impl CreditsPerPercentagePoint {
    /// Constructs from an integer number of micro-credits per one `PercentagePoints`
    /// unit.
    ///
    /// `pub(crate)`: see the module documentation on why this cannot be restricted more
    /// narrowly than the crate boundary from here, and on what still closes the gap.
    #[allow(dead_code)] // no consumer yet: the calibration epic (aub-c0b) wires this in.
    pub(crate) const fn from_micros_per_point(micros_per_point: i64) -> Self {
        Self { micros_per_point }
    }

    /// The rate in micro-credits per one `PercentagePoints` unit.
    pub const fn micros_per_point(self) -> i64 {
        self.micros_per_point
    }
}

impl std::ops::Mul<PercentagePoints> for CreditsPerPercentagePoint {
    type Output = Credits;

    /// Multiplies by a percentage-point delta, yielding an exact `Credits` amount.
    /// Exact integer multiplication: see the type's documentation for why no division
    /// or rounding applies here, unlike `CreditsPerToken::mul`.
    fn mul(self, points: PercentagePoints) -> Credits {
        Credits::from_micros(self.micros_per_point * i64::from(points.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random generator, the same construction `money.rs` uses,
    /// so property-style tests run over many values without a property-testing
    /// dependency.
    fn xorshift(seed: u64) -> impl FnMut() -> u64 {
        let mut state = seed;
        move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn credits_addition_is_exact_integer_arithmetic() {
        let a = Credits::from_micros(1_000_000);
        let b = Credits::from_micros(2_500_000);
        assert_eq!((a + b).micros(), 3_500_000);
    }

    #[test]
    fn credits_per_token_rounds_half_away_from_zero_at_the_boundary() {
        // One micro per million tokens, so the half-micro boundary sits at 500_000
        // tokens, exactly Money's own boundary test shape.
        let rate = CreditsPerToken::from_micros_per_million_tokens(1);
        assert_eq!(
            (rate * 499_999).micros(),
            0,
            "rounds down below the half boundary"
        );
        assert_eq!((rate * 500_000).micros(), 1, "rounds half away from zero");
        assert_eq!(
            (rate * 500_001).micros(),
            1,
            "rounds up past the half boundary"
        );
    }

    #[test]
    fn credits_per_token_rounds_negative_half_away_from_zero() {
        let rate = CreditsPerToken::from_micros_per_million_tokens(-1);
        assert_eq!(
            (rate * 500_000).micros(),
            -1,
            "negative half rounds away from zero"
        );
        assert_eq!(
            (rate * 499_999).micros(),
            0,
            "rounds toward zero below the boundary"
        );
    }

    #[test]
    fn credits_per_token_whole_rate_times_a_million_tokens_is_exact() {
        let rate = CreditsPerToken::from_micros_per_million_tokens(15_000_000);
        assert_eq!((rate * 1_000_000).micros(), 15_000_000);
        assert_eq!((rate * 500_000).micros(), 7_500_000);
    }

    #[test]
    fn credits_per_percentage_point_multiplication_is_exact_with_no_rounding() {
        let rate = CreditsPerPercentagePoint::from_micros_per_point(3);
        let points = PercentagePoints::new(250_000).unwrap();
        assert_eq!((rate * points).micros(), 750_000);

        let negative_points = PercentagePoints::new(-250_000).unwrap();
        assert_eq!((rate * negative_points).micros(), -750_000);
    }

    proptest::proptest! {
        #[test]
        fn prop_scaling_a_token_rate_is_linear_and_stays_in_credits(
            rate_micros in -1_000_000i64..=1_000_000i64,
            count in 0u64..10_000_000u64,
            factor in 1u64..=10u64,
        ) {
            let rate = CreditsPerToken::from_micros_per_million_tokens(rate_micros);
            let once: Credits = rate * (count * factor);
            let composed = Credits::from_micros((rate * count).micros() * factor as i64);

            let diff = (once.micros() - composed.micros()).abs();
            prop_assert!(
                diff <= factor as i64,
                "scaling drift {diff} exceeds the {factor} independent-roundings bound"
            );
        }
    }

    /// Retained hand-picked regression: walks deterministic pseudo-random samples.
    #[test]
    fn scaling_a_token_rate_is_linear_and_stays_in_credits_hand_picked() {
        let mut next = xorshift(0x2545_F491_4F6C_DD1D);
        for _ in 0..200 {
            let rate = CreditsPerToken::from_micros_per_million_tokens(
                (next() % 2_000_001) as i64 - 1_000_000,
            );
            let count = next() % 10_000_000;
            let factor = 1 + (next() % 5);

            // The result type is `Credits` by construction (`Mul::Output = Credits`);
            // what this loop actually exercises is that the VALUE tracks linearly.
            let once: Credits = rate * (count * factor);
            let composed = Credits::from_micros((rate * count).micros() * factor as i64);

            let diff = (once.micros() - composed.micros()).abs();
            assert!(
                diff <= factor as i64,
                "scaling drift {diff} exceeds the {factor} independent-roundings bound"
            );
        }
    }
}
