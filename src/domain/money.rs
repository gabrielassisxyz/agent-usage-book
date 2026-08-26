//! Money: exact monetary amounts with an explicit currency.
//!
//! The monetary axis is the one most likely to be read as authoritative by somebody
//! skimming, so it carries the narrowest possible meaning: an API list-price
//! equivalent, never "cost". A subscription token and an API-priced token are
//! quantities in two different economic systems, and no generic `Cost` type represents
//! both.
//!
//! Amounts are stored as integer micros (1/1_000_000 of the major currency unit, a
//! microdollar or a microeuro), so arithmetic is exact: no binary floating point is
//! involved, and aggregation order cannot move the last digit.
//!
//! Currency is a type parameter, not a field: `Money<Usd>` and `Money<Eur>` are
//! different types, and adding them does not compile. The cost of carrying the
//! currency now is one type parameter; the cost of adding it after totals exist is
//! auditing every summation.
//!
//! Rounding policy: the only place a fractional micro can arise is the
//! [`MoneyPerMillionTokens::times_tokens`] multiplication, which divides by one
//! million. It rounds half away from zero, so a half-micro never silently vanishes and
//! the result is symmetric for negative amounts. The policy is applied in exactly one
//! place, [`round_div`].
//!
//! Neither type implements `Default` (a default amount would be a silent zero) or a
//! free-standing `Display` (rendering requires a presentation helper that takes the
//! currency and precision explicitly).

use std::marker::PhantomData;
use std::ops::Add;

/// A currency. Currencies are distinct types so that amounts in different currencies
/// cannot be combined without an explicit conversion.
///
/// The trait is sealed: currencies are a closed set defined here, not invented ad hoc
/// by downstream code.
pub trait Currency: private::Sealed {
    /// The ISO 4217 alphabetic code, for rendering.
    const CODE: &'static str;
}

mod private {
    pub trait Sealed {}
}

/// United States dollar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Usd {}

impl private::Sealed for Usd {}
impl Currency for Usd {
    const CODE: &'static str = "USD";
}

/// Euro.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Eur {}

impl private::Sealed for Eur {}
impl Currency for Eur {
    const CODE: &'static str = "EUR";
}

/// An exact monetary amount in a specific currency, stored as integer micros.
///
/// One micro is 1/1_000_000 of the major currency unit. The amount is exact: no binary
/// floating point is involved, so aggregation order cannot move the last digit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Money<C: Currency> {
    micros: i64,
    _currency: PhantomData<C>,
}

impl<C: Currency> Money<C> {
    /// Constructs from an integer number of micros.
    pub const fn from_micros(micros: i64) -> Self {
        Money {
            micros,
            _currency: PhantomData,
        }
    }

    /// The amount in micros.
    pub const fn micros(self) -> i64 {
        self.micros
    }

    /// The currency's ISO 4217 alphabetic code.
    pub const fn currency_code(self) -> &'static str {
        C::CODE
    }
}

impl<C: Currency> Add for Money<C> {
    type Output = Money<C>;

    /// Adds two amounts of the same currency. There is deliberately no `Add` for two
    /// different currencies: `Money<Usd> + Money<Eur>` does not compile.
    fn add(self, rhs: Money<C>) -> Money<C> {
        Money::from_micros(self.micros + rhs.micros)
    }
}

/// A price per million tokens, in a specific currency.
///
/// Stored as integer micros per million tokens, so a rate of $15.00 per million tokens
/// is `15_000_000` micros per million.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MoneyPerMillionTokens<C: Currency> {
    micros_per_million: i64,
    _currency: PhantomData<C>,
}

impl<C: Currency> MoneyPerMillionTokens<C> {
    /// Constructs from an integer number of micros per million tokens.
    pub const fn from_micros_per_million(micros_per_million: i64) -> Self {
        MoneyPerMillionTokens {
            micros_per_million,
            _currency: PhantomData,
        }
    }

    /// The rate in micros per million tokens.
    pub const fn micros_per_million(self) -> i64 {
        self.micros_per_million
    }

    /// Multiplies by a raw token count, yielding an exact `Money` amount.
    ///
    /// The token count is a raw `u64`; the valuation module sums per-kind token counts
    /// and passes the total here. The division by one million is the single place the
    /// rounding policy is applied.
    pub fn times_tokens(self, tokens: u64) -> Money<C> {
        // The i128 intermediate exists so the multiplication cannot overflow before the
        // division; realistic rates and token counts fit comfortably in i64 afterwards.
        let numerator = i128::from(self.micros_per_million) * i128::from(tokens);
        let micros = round_div(numerator, 1_000_000);
        Money::from_micros(micros as i64)
    }
}

/// Rounds `numerator / denominator` to the nearest integer, ties away from zero.
///
/// This is the single place the rounding policy is applied.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addition_is_exact_integer_arithmetic() {
        let a = Money::<Usd>::from_micros(1_000_000); // $1.00
        let b = Money::<Usd>::from_micros(2_500_000); // $2.50
        assert_eq!((a + b).micros(), 3_500_000); // $3.50
    }

    #[test]
    fn times_tokens_rounds_half_away_from_zero_at_the_boundary() {
        // One micro per million tokens, so the half-micro boundary sits at 500_000
        // tokens. Hand-computed expected values.
        let rate = MoneyPerMillionTokens::<Usd>::from_micros_per_million(1);
        // 499_999 tokens -> 0.499999 micros -> 0 (rounds down).
        assert_eq!(rate.times_tokens(499_999).micros(), 0);
        // 500_000 tokens -> 0.5 micros -> 1 (rounds half away from zero).
        assert_eq!(rate.times_tokens(500_000).micros(), 1);
        // 500_001 tokens -> 0.500001 micros -> 1.
        assert_eq!(rate.times_tokens(500_001).micros(), 1);
    }

    #[test]
    fn times_tokens_rounds_negative_half_away_from_zero() {
        let rate = MoneyPerMillionTokens::<Usd>::from_micros_per_million(-1);
        // -0.5 micros rounds away from zero to -1.
        assert_eq!(rate.times_tokens(500_000).micros(), -1);
        // -0.499999 micros rounds to 0.
        assert_eq!(rate.times_tokens(499_999).micros(), 0);
    }

    #[test]
    fn a_whole_dollar_rate_times_a_million_tokens_is_exact() {
        // $15.00 per million tokens, times exactly one million tokens, is $15.00.
        let rate = MoneyPerMillionTokens::<Usd>::from_micros_per_million(15_000_000);
        assert_eq!(rate.times_tokens(1_000_000).micros(), 15_000_000);
        // Half a million tokens is $7.50.
        assert_eq!(rate.times_tokens(500_000).micros(), 7_500_000);
    }

    #[test]
    fn summing_is_order_independent() {
        // A deterministic pseudo-random sample of amounts, summed forward and backward.
        // Money addition is exact integer addition, so the two orders must agree.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..100 {
            let amounts: Vec<Money<Usd>> = (0..50)
                .map(|_| Money::from_micros((next() % 2_000_001) as i64 - 1_000_000))
                .collect();
            let forward = amounts
                .iter()
                .copied()
                .fold(Money::from_micros(0), |acc, m| acc + m);
            let backward = amounts
                .iter()
                .rev()
                .copied()
                .fold(Money::from_micros(0), |acc, m| acc + m);
            assert_eq!(forward, backward);
        }
    }
}
