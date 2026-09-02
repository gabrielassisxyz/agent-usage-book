//! Rate cards: immutable dated vendor prices, read like every other versioned
//! reference data (PLAN.md sections 12.15, 25.3).
//!
//! A rate card is not a meter reading. It carries an effective interval, a
//! publication reference, an import timestamp and a review-due policy instead of
//! the meter's freshness enum, because authentication is nonsensical for a local
//! price book and forcing the shape through one enum would lose that precision.
//!
//! Records are immutable by construction: a corrected price is a new record, and
//! the store layer enforces the same rule mechanically, refusing every rewrite
//! of the table at the schema level. Nothing here computes a valuation;
//! that is the valuation module's job (aub-wyu.2), which resolves the record
//! effective at an event's time.

use crate::domain::time::{UtcDate, UtcTimestamp};

/// Which token stream a rate prices.
///
/// The variants are the five kinds the existing price table distinguishes. A
/// vendor that prices a new stream adds a variant here, and the exhaustive
/// matches in this module and the store layer refuse to compile until the new
/// class states how it persists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenClass {
    Input,
    Output,
    CacheRead,
    /// Cache writes billed at the five-minute TTL price.
    CacheWrite5m,
    /// Cache writes billed at the one-hour TTL price.
    CacheWrite1h,
}

impl TokenClass {
    /// The symbolic form the rate book file and the store both use.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenClass::Input => "input",
            TokenClass::Output => "output",
            TokenClass::CacheRead => "cache_read",
            TokenClass::CacheWrite5m => "cache_write_5m",
            TokenClass::CacheWrite1h => "cache_write_1h",
        }
    }

    /// Parses the symbolic form. An unknown class is a refused card, never a
    /// guess at the nearest known one.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "input" => Some(TokenClass::Input),
            "output" => Some(TokenClass::Output),
            "cache_read" => Some(TokenClass::CacheRead),
            "cache_write_5m" => Some(TokenClass::CacheWrite5m),
            "cache_write_1h" => Some(TokenClass::CacheWrite1h),
            _ => None,
        }
    }
}

/// The unit a rate is quoted against. The existing book is quoted per million
/// tokens; the exhaustive match keeps a future basis a compile-time decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BillingBasis {
    PerMillionTokens,
}

impl BillingBasis {
    /// The symbolic form the rate book file and the store both use.
    pub fn as_str(self) -> &'static str {
        match self {
            BillingBasis::PerMillionTokens => "per_million_tokens",
        }
    }

    /// Parses the symbolic form.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "per_million_tokens" => Some(BillingBasis::PerMillionTokens),
            _ => None,
        }
    }
}

/// A currency code. Runtime data, unlike the compile-time currency types in
/// [`crate::domain::money`]: a rate card is imported, and its currency arrives
/// as text. Converting into a typed `Money<C>` is a named function in the
/// valuation layer, never a silent cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CurrencyCode {
    Usd,
    Eur,
}

impl CurrencyCode {
    /// The ISO 4217 alphabetic code.
    pub fn as_str(self) -> &'static str {
        match self {
            CurrencyCode::Usd => "USD",
            CurrencyCode::Eur => "EUR",
        }
    }

    /// Parses an ISO 4217 alphabetic code. An unknown code is a refused card:
    /// pricing it in some other currency silently would be exactly the unit
    /// confusion this project exists to prevent.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "USD" => Some(CurrencyCode::Usd),
            "EUR" => Some(CurrencyCode::Eur),
            _ => None,
        }
    }
}

/// The review-due policy (section 25.3). A rate card is temporal reference
/// data, not a live reading: when it should be re-reviewed is stated here, and
/// nothing about authentication enters the shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewDuePolicy {
    /// No review obligation recorded.
    None,
    /// The card must be reviewed on or after this date. Introductory pricing
    /// that expires is the case this exists for: the expiry is the review.
    On(UtcDate),
}

impl ReviewDuePolicy {
    /// The stored symbolic form. `None` stays absent rather than becoming a
    /// sentinel value, so the database row and the domain type agree about
    /// what was never recorded.
    pub fn iso(&self) -> Option<String> {
        match self {
            ReviewDuePolicy::None => None,
            ReviewDuePolicy::On(date) => Some(date.iso()),
        }
    }
}

/// Publication provenance, with absence explicit (section 32): a card whose
/// original publication is unknown records that fact instead of presenting
/// itself as fully sourced.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Publication {
    /// Where the price was read from, e.g. the vendor's API reference page.
    /// `None` means the import could not name a source.
    pub source: Option<String>,
    /// When the vendor published the price, when that fact is known. `None`
    /// means the publication date is not part of the record.
    pub published_at: Option<UtcTimestamp>,
}

impl Publication {
    /// Provenance is complete only when both halves are present. The flag is
    /// what `rate-card show` reports, so missing provenance is visible rather
    /// than silent.
    pub fn fully_sourced(&self) -> bool {
        self.source.is_some() && self.published_at.is_some()
    }
}

/// What one import contributes for one rate component, before the store stamps
/// the import time and assigns the row id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateCardDraft {
    /// The vendor the rate belongs to, e.g. `anthropic`.
    pub vendor: String,
    /// The model the rate prices. The existing book keys on a model substring;
    /// the string is kept verbatim so matching stays the valuation layer's
    /// decision.
    pub model: String,
    pub token_class: TokenClass,
    /// The rate in integer micros of [`Self::currency`] per the billing basis.
    /// Exact integer arithmetic, same convention as `Money`.
    pub rate_micros: i64,
    pub currency: CurrencyCode,
    pub billing_basis: BillingBasis,
    /// The first day the rate is effective.
    pub effective_start: UtcDate,
    /// The day after which the rate no longer applies; `None` is open-ended.
    pub effective_end: Option<UtcDate>,
    pub publication: Publication,
    pub review_due: ReviewDuePolicy,
}

/// A persisted rate card: a draft plus the facts only the store can supply.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateCard {
    pub id: i64,
    /// When the import that produced this record ran.
    pub imported_at: UtcTimestamp,
    pub draft: RateCardDraft,
}

/// Parses a decimal rate string into exact micros, refusing rather than
/// rounding. Up to six fractional digits are representable; anything finer
/// would lose value silently, so it is an error. A negative rate is a defect,
/// not a price.
pub fn parse_rate_micros(text: &str) -> Result<i64, RateCardParseError> {
    let text = text.trim();
    let negative = text.starts_with('-');
    let body = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match body.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (body, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return Err(RateCardParseError::RateNotANumber(text.to_string()));
    }
    if fraction.len() > 6 {
        return Err(RateCardParseError::RateTooFine(text.to_string()));
    }
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| RateCardParseError::RateNotANumber(text.to_string()))?
    };
    let mut micros = whole
        .checked_mul(1_000_000)
        .ok_or_else(|| RateCardParseError::RateOutOfRange(text.to_string()))?;
    let mut scale = 100_000i64;
    for digit in fraction.chars() {
        let digit: u32 = digit
            .to_digit(10)
            .ok_or_else(|| RateCardParseError::RateNotANumber(text.to_string()))?;
        micros = micros
            .checked_add(i64::from(digit) * scale)
            .ok_or_else(|| RateCardParseError::RateOutOfRange(text.to_string()))?;
        scale /= 10;
    }
    if negative {
        return Err(RateCardParseError::NegativeRate(text.to_string()));
    }
    Ok(micros)
}

/// Why a rate-card value could not be parsed. Every variant carries the input
/// text, so the import report names the defect instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateCardParseError {
    RateNotANumber(String),
    /// Finer than one micro: the value is not exactly representable.
    RateTooFine(String),
    RateOutOfRange(String),
    NegativeRate(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> UtcDate {
        UtcDate::parse(text).expect("test date must parse")
    }

    #[test]
    fn token_class_round_trips_through_its_symbolic_form() {
        for class in [
            TokenClass::Input,
            TokenClass::Output,
            TokenClass::CacheRead,
            TokenClass::CacheWrite5m,
            TokenClass::CacheWrite1h,
        ] {
            assert_eq!(TokenClass::parse(class.as_str()), Some(class));
        }
    }

    #[test]
    fn an_unknown_token_class_is_refused_not_guessed() {
        assert_eq!(TokenClass::parse("cached_output"), None);
    }

    #[test]
    fn an_unknown_currency_is_refused_not_guessed() {
        assert_eq!(CurrencyCode::parse("BRL"), None);
        assert_eq!(CurrencyCode::parse("usd"), None);
        assert_eq!(CurrencyCode::parse("USD"), Some(CurrencyCode::Usd));
    }

    #[test]
    fn decimal_rates_convert_to_exact_micros() {
        assert_eq!(parse_rate_micros("10.00"), Ok(10_000_000));
        assert_eq!(parse_rate_micros("3.75"), Ok(3_750_000));
        assert_eq!(parse_rate_micros("0.10"), Ok(100_000));
        assert_eq!(parse_rate_micros("15"), Ok(15_000_000));
        assert_eq!(parse_rate_micros("0.000001"), Ok(1));
    }

    #[test]
    fn a_rate_finer_than_one_micro_is_refused_not_rounded() {
        assert_eq!(
            parse_rate_micros("0.0000001"),
            Err(RateCardParseError::RateTooFine("0.0000001".into()))
        );
    }

    #[test]
    fn a_negative_rate_is_a_defect_not_a_price() {
        assert_eq!(
            parse_rate_micros("-1.00"),
            Err(RateCardParseError::NegativeRate("-1.00".into()))
        );
    }

    #[test]
    fn a_nonsense_rate_is_refused_by_name() {
        assert_eq!(
            parse_rate_micros("abc"),
            Err(RateCardParseError::RateNotANumber("abc".into()))
        );
        assert_eq!(
            parse_rate_micros(""),
            Err(RateCardParseError::RateNotANumber("".into()))
        );
    }

    /// Planted negative: the naive implementation of "no freshness enum" would
    /// reuse `MeasurementBasis` or add an `auth_required`-shaped state. The
    /// review-due policy carries none of that vocabulary, and the type layout
    /// itself is the assertion this test pins: an effective interval and a
    /// review date, nothing else.
    #[test]
    fn the_review_policy_carries_no_freshness_vocabulary() {
        let none = ReviewDuePolicy::None;
        let on_date = ReviewDuePolicy::On(date("2026-08-31"));
        assert_ne!(none, on_date);
        // The policy renders from a date alone; no freshness reason, no
        // staleness state, no authentication arm exists to construct.
        assert_eq!(on_date, ReviewDuePolicy::On(date("2026-08-31")));
    }

    #[test]
    fn publication_provenance_is_explicit_about_absence() {
        let missing = Publication {
            source: None,
            published_at: None,
        };
        assert!(!missing.fully_sourced());
        let half = Publication {
            source: Some("claude-api reference".into()),
            published_at: None,
        };
        assert!(!half.fully_sourced());
        let full = Publication {
            source: Some("claude-api reference".into()),
            published_at: Some(UtcTimestamp::from_unix_nanos(0)),
        };
        assert!(full.fully_sourced());
    }
}
