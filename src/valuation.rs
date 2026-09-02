//! Usage-vector to API-price equivalent (`aub-wyu.2`).
//!
//! May not depend on:
//! - presentation
//! - store or filesystem
//!
//! The `UsageVector` to `ApiListPriceEquivalent` conversion requires a typed `RateBook`
//! or `RateCard` witness and is owned by this module; no global conversion witness exists.
//!
//! Rounding and monetary precision:
//! - All rates are quoted in integer micros per million tokens (PLAN.md 25.3).
//! - Multiplication uses `MoneyPerMillionTokens::<C>::times_tokens` which rounds half away from zero.
//! - Currencies are distinct phantom-typed `Money<C>` parameters (PLAN.md 25.2).

use crate::domain::money::{Currency, Money, MoneyPerMillionTokens};
use crate::domain::rate_card::{CurrencyCode, RateCard, TokenClass};
use crate::domain::time::UtcDate;
use crate::domain::tokens::{TokenKind, UsageVector};

/// A monetary valuation result representing counterfactual API list-price equivalent.
///
/// Distinct from subscription credit consumption (PLAN.md 25.1, 25.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiListPriceEquivalent<C: Currency> {
    amount: Money<C>,
}

impl<C: Currency> ApiListPriceEquivalent<C> {
    /// Constructs from a typed `Money<C>` amount.
    pub const fn new(amount: Money<C>) -> Self {
        Self { amount }
    }

    /// The wrapped exact monetary amount.
    pub const fn amount(self) -> Money<C> {
        self.amount
    }

    /// The exact amount in micros (1/1_000_000 of major currency unit).
    pub const fn micros(self) -> i64 {
        self.amount.micros()
    }

    /// Zero list price equivalent in currency `C`.
    pub const fn zero() -> Self {
        Self {
            amount: Money::from_micros(0),
        }
    }
}

impl<C: Currency> std::ops::Add for ApiListPriceEquivalent<C> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            amount: self.amount + rhs.amount,
        }
    }
}

/// Identifies a specific rate that was needed to value a usage event but was missing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MissingRate {
    pub vendor: String,
    pub model: String,
    pub token_class: String,
    pub date: UtcDate,
}

impl MissingRate {
    pub fn new(
        vendor: impl Into<String>,
        model: impl Into<String>,
        token_class: impl Into<String>,
        date: UtcDate,
    ) -> Self {
        Self {
            vendor: vendor.into(),
            model: model.into(),
            token_class: token_class.into(),
            date,
        }
    }
}

/// The result of valuing usage against a rate book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValuationOutcome<C: Currency> {
    /// Complete valuation: all consumed token classes were matched and priced.
    Complete(ApiListPriceEquivalent<C>),
    /// Incomplete valuation: one or more token classes lacked matching rates.
    ///
    /// The subtotal represents the known-price subtotal only, and must never be
    /// presented as a complete total (PLAN.md 25.4).
    Incomplete {
        known_price_subtotal: ApiListPriceEquivalent<C>,
        missing_rates: Vec<MissingRate>,
    },
    /// A matching rate card used a currency different from the requested currency `C`.
    UnsupportedCurrency {
        found: CurrencyCode,
        expected: &'static str,
    },
}

/// An immutable in-memory book of versioned rate cards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateBook {
    cards: Vec<RateCard>,
}

impl RateBook {
    /// Constructs a rate book from a slice of rate cards.
    pub fn new(cards: Vec<RateCard>) -> Self {
        Self { cards }
    }

    /// The list of rate cards in this book.
    pub fn cards(&self) -> &[RateCard] {
        &self.cards
    }

    /// Finds the rate card effective for a specific vendor, model, token class, and date.
    ///
    /// If multiple matching cards exist (e.g. revisions on the same date), the one with
    /// the highest `id` or latest `imported_at` is preferred.
    pub fn find_rate(
        &self,
        vendor: &str,
        model: &str,
        token_class: TokenClass,
        date: UtcDate,
    ) -> Option<&RateCard> {
        let normalized_vendor = vendor.trim().to_ascii_lowercase();
        let normalized_model = model.trim().to_ascii_lowercase();

        self.cards
            .iter()
            .filter(|card| {
                let card_vendor = card.draft.vendor.trim().to_ascii_lowercase();
                let card_model = card.draft.model.trim().to_ascii_lowercase();

                card_vendor == normalized_vendor
                    && (card_model == normalized_model || normalized_model.contains(&card_model))
                    && card.draft.token_class == token_class
                    && date >= card.draft.effective_start
                    && card.draft.effective_end.is_none_or(|end| date <= end)
            })
            .max_by_key(|card| (card.draft.effective_start, card.id, card.imported_at))
    }

    /// Finds a rate card for an arbitrary / unknown token class name by string match.
    pub fn find_custom_rate(
        &self,
        vendor: &str,
        model: &str,
        token_class_name: &str,
        date: UtcDate,
    ) -> Option<&RateCard> {
        if let Some(known_class) = TokenClass::parse(token_class_name) {
            return self.find_rate(vendor, model, known_class, date);
        }
        None
    }
}

/// Maps a known `TokenKind` to the standard `TokenClass` priced in rate cards.
///
/// Exhaustive match over all `TokenKind` variants with NO wildcard arm.
/// If a new variant is added to `TokenKind`, this function fails compilation
/// until the variant is explicitly mapped.
pub const fn token_kind_to_class(kind: TokenKind) -> TokenClass {
    match kind {
        TokenKind::Input => TokenClass::Input,
        TokenKind::Output => TokenClass::Output,
        TokenKind::CacheRead => TokenClass::CacheRead,
        TokenKind::CacheWrite => TokenClass::CacheWrite5m,
    }
}

/// Values a `UsageVector` at API list-price equivalent in currency `C`.
///
/// Exhaustively checks every known token kind and unknown component.
/// If any non-zero token component lacks a rate, returns `ValuationOutcome::Incomplete`
/// naming each missing rate and providing the known-price subtotal.
pub fn value_usage_vector<C: Currency>(
    book: &RateBook,
    vendor: &str,
    model: &str,
    date: UtcDate,
    usage: &UsageVector,
) -> ValuationOutcome<C> {
    let mut total_micros: i64 = 0;
    let mut missing_rates = Vec::new();

    // 1. Evaluate known token kinds exhaustively
    for &kind in &TokenKind::ALL {
        let count = usage.known().value(kind);
        if count == 0 {
            continue;
        }

        let class = token_kind_to_class(kind);
        match book.find_rate(vendor, model, class, date) {
            Some(card) => {
                if card.draft.currency.as_str() != C::CODE {
                    return ValuationOutcome::UnsupportedCurrency {
                        found: card.draft.currency,
                        expected: C::CODE,
                    };
                }
                let rate =
                    MoneyPerMillionTokens::<C>::from_micros_per_million(card.draft.rate_micros);
                let cost = rate.times_tokens(count);
                total_micros += cost.micros();
            }
            None => {
                missing_rates.push(MissingRate::new(vendor, model, class.as_str(), date));
            }
        }
    }

    // 2. Evaluate unknown token components
    for (class_name, count) in usage.unknown() {
        if count.value() == 0 {
            continue;
        }

        match book.find_custom_rate(vendor, model, class_name, date) {
            Some(card) => {
                if card.draft.currency.as_str() != C::CODE {
                    return ValuationOutcome::UnsupportedCurrency {
                        found: card.draft.currency,
                        expected: C::CODE,
                    };
                }
                let rate =
                    MoneyPerMillionTokens::<C>::from_micros_per_million(card.draft.rate_micros);
                let cost = rate.times_tokens(count.value());
                total_micros += cost.micros();
            }
            None => {
                missing_rates.push(MissingRate::new(vendor, model, class_name.as_str(), date));
            }
        }
    }

    let subtotal = ApiListPriceEquivalent::new(Money::<C>::from_micros(total_micros));
    if missing_rates.is_empty() {
        ValuationOutcome::Complete(subtotal)
    } else {
        ValuationOutcome::Incomplete {
            known_price_subtotal: subtotal,
            missing_rates,
        }
    }
}

/// Values a collection of usage vectors and aggregates them into one result.
///
/// Order independent: summing integer micros is associative and commutative.
pub fn value_batch<C: Currency>(
    book: &RateBook,
    items: &[(&str, &str, UtcDate, &UsageVector)],
) -> ValuationOutcome<C> {
    let mut total_micros: i64 = 0;
    let mut all_missing = Vec::new();

    for &(vendor, model, date, usage) in items {
        match value_usage_vector::<C>(book, vendor, model, date, usage) {
            ValuationOutcome::Complete(equiv) => {
                total_micros += equiv.micros();
            }
            ValuationOutcome::Incomplete {
                known_price_subtotal,
                mut missing_rates,
            } => {
                total_micros += known_price_subtotal.micros();
                all_missing.append(&mut missing_rates);
            }
            ValuationOutcome::UnsupportedCurrency { found, expected } => {
                return ValuationOutcome::UnsupportedCurrency { found, expected };
            }
        }
    }

    all_missing.sort();
    all_missing.dedup();

    let subtotal = ApiListPriceEquivalent::new(Money::<C>::from_micros(total_micros));
    if all_missing.is_empty() {
        ValuationOutcome::Complete(subtotal)
    } else {
        ValuationOutcome::Incomplete {
            known_price_subtotal: subtotal,
            missing_rates: all_missing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::domain::money::Usd;
    use crate::domain::rate_card::{BillingBasis, Publication, RateCardDraft, ReviewDuePolicy};
    use crate::domain::time::UtcTimestamp;
    use crate::domain::tokens::{
        CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens,
    };
    use crate::evidence::{CoverageCompleteness, EvidenceQuality};

    fn make_card(
        id: i64,
        vendor: &str,
        model: &str,
        token_class: TokenClass,
        rate_micros: i64,
        effective_start: &str,
        effective_end: Option<&str>,
    ) -> RateCard {
        RateCard {
            id,
            imported_at: UtcTimestamp::from_unix_nanos(0),
            draft: RateCardDraft {
                vendor: vendor.to_string(),
                model: model.to_string(),
                token_class,
                rate_micros,
                currency: CurrencyCode::Usd,
                billing_basis: BillingBasis::PerMillionTokens,
                effective_start: UtcDate::parse(effective_start).unwrap(),
                effective_end: effective_end.map(|d| UtcDate::parse(d).unwrap()),
                publication: Publication {
                    source: Some("test".to_string()),
                    published_at: None,
                },
                review_due: ReviewDuePolicy::None,
            },
        }
    }

    fn sample_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> UsageVector {
        UsageVector::new(
            KnownTokenVector::new(
                InputTokens::new(input),
                OutputTokens::new(output),
                CacheReadTokens::new(cache_read),
                CacheWriteTokens::new(cache_write),
            ),
            BTreeMap::new(),
            CoverageCompleteness::Complete,
            EvidenceQuality::Measured,
        )
    }

    /// Golden: exact decimal fixtures with hand-computed expected values.
    /// Sonnet 3.5: Input $3.00/M, Output $15.00/M, CacheRead $0.30/M, CacheWrite $3.75/M
    /// 100k in, 20k out, 50k cache read, 10k cache write
    /// in: 100_000 * 3.00/1_000_000 = $0.30 (300_000 micros)
    /// out: 20_000 * 15.00/1_000_000 = $0.30 (300_000 micros)
    /// read: 50_000 * 0.30/1_000_000 = $0.015 (15_000 micros)
    /// write: 10_000 * 3.75/1_000_000 = $0.0375 (37_500 micros)
    /// Total: $0.6525 = 652_500 micros
    #[test]
    fn golden_exact_decimal_valuation() {
        let cards = vec![
            make_card(
                1,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::Input,
                3_000_000,
                "2024-06-01",
                None,
            ),
            make_card(
                2,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::Output,
                15_000_000,
                "2024-06-01",
                None,
            ),
            make_card(
                3,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::CacheRead,
                300_000,
                "2024-06-01",
                None,
            ),
            make_card(
                4,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::CacheWrite5m,
                3_750_000,
                "2024-06-01",
                None,
            ),
        ];
        let book = RateBook::new(cards);
        let usage = sample_usage(100_000, 20_000, 50_000, 10_000);
        let date = UtcDate::parse("2024-07-01").unwrap();

        let res = value_usage_vector::<Usd>(&book, "anthropic", "claude-3-5-sonnet", date, &usage);
        match res {
            ValuationOutcome::Complete(equiv) => {
                assert_eq!(
                    equiv.micros(),
                    652_500,
                    "expected exact hand-computed 652,500 micros ($0.6525)"
                );
            }
            other => panic!("expected Complete valuation, got {other:?}"),
        }
    }

    /// Unit: effective-date boundary tested in both directions.
    #[test]
    fn effective_date_boundaries() {
        let cards = vec![make_card(
            1,
            "anthropic",
            "claude-3-sonnet",
            TokenClass::Input,
            3_000_000,
            "2024-06-01",
            Some("2024-06-30"),
        )];
        let book = RateBook::new(cards);
        let usage = sample_usage(100_000, 0, 0, 0);

        // Day before effective start -> missing rate
        let before = UtcDate::parse("2024-05-31").unwrap();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", before, &usage),
            ValuationOutcome::Incomplete { .. }
        ));

        // Exactly on effective start -> complete
        let start = UtcDate::parse("2024-06-01").unwrap();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", start, &usage),
            ValuationOutcome::Complete(..)
        ));

        // Exactly on effective end -> complete
        let end = UtcDate::parse("2024-06-30").unwrap();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", end, &usage),
            ValuationOutcome::Complete(..)
        ));

        // Day after effective end -> missing rate
        let after = UtcDate::parse("2024-07-01").unwrap();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", after, &usage),
            ValuationOutcome::Incomplete { .. }
        ));
    }

    /// Unit: missing cache-write price does NOT imply zero cache-write cost.
    #[test]
    fn missing_cache_write_price_is_incomplete_never_zero_cost() {
        let cards = vec![
            make_card(
                1,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::Input,
                3_000_000,
                "2024-06-01",
                None,
            ),
            make_card(
                2,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::Output,
                15_000_000,
                "2024-06-01",
                None,
            ),
            make_card(
                3,
                "anthropic",
                "claude-3-5-sonnet",
                TokenClass::CacheRead,
                300_000,
                "2024-06-01",
                None,
            ),
            // Deliberately omit CacheWrite5m!
        ];
        let book = RateBook::new(cards);
        let usage = sample_usage(100_000, 20_000, 50_000, 10_000);
        let date = UtcDate::parse("2024-07-01").unwrap();

        let outcome =
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-5-sonnet", date, &usage);
        match outcome {
            ValuationOutcome::Incomplete {
                known_price_subtotal,
                missing_rates,
            } => {
                // Known subtotal is input (300_000) + output (300_000) + read (15_000) = 615_000 micros
                assert_eq!(known_price_subtotal.micros(), 615_000);
                assert_eq!(missing_rates.len(), 1);
                assert_eq!(missing_rates[0].token_class, "cache_write_5m");
            }
            other => panic!("expected Incomplete outcome, got {other:?}"),
        }
    }

    /// Unit: mid-period model price change produces two differently valued halves and correct sum.
    #[test]
    fn mid_period_model_price_change() {
        let cards = vec![
            make_card(
                1,
                "openai",
                "gpt-4o",
                TokenClass::Input,
                5_000_000,
                "2024-01-01",
                Some("2024-07-31"),
            ),
            make_card(
                2,
                "openai",
                "gpt-4o",
                TokenClass::Input,
                2_500_000,
                "2024-08-01",
                None,
            ),
        ];
        let book = RateBook::new(cards);
        let usage = sample_usage(1_000_000, 0, 0, 0);

        let date_july = UtcDate::parse("2024-07-15").unwrap();
        let date_august = UtcDate::parse("2024-08-15").unwrap();

        let cost_july =
            match value_usage_vector::<Usd>(&book, "openai", "gpt-4o", date_july, &usage) {
                ValuationOutcome::Complete(eq) => eq,
                other => panic!("expected July to be complete, got {other:?}"),
            };
        assert_eq!(cost_july.micros(), 5_000_000); // $5.00

        let cost_aug =
            match value_usage_vector::<Usd>(&book, "openai", "gpt-4o", date_august, &usage) {
                ValuationOutcome::Complete(eq) => eq,
                other => panic!("expected August to be complete, got {other:?}"),
            };
        assert_eq!(cost_aug.micros(), 2_500_000); // $2.50

        let total = cost_july + cost_aug;
        assert_eq!(total.micros(), 7_500_000); // $7.50
    }
}
