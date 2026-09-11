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

use crate::domain::money::{Currency, Money, MoneyPerMillionTokens, Usd};
use crate::domain::provenance::RateCardId;
use crate::domain::rate_card::{CurrencyCode, RateCard, ReviewDuePolicy, Schedule, TokenClass};
use crate::domain::time::{UtcDate, UtcTimestamp};
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
    /// The instant the event happened: the missing fact names when, not just
    /// the day, because a scheduled card can be in force for part of a day
    /// (aub-pwtn).
    pub at: UtcTimestamp,
}

impl MissingRate {
    pub fn new(
        vendor: impl Into<String>,
        model: impl Into<String>,
        token_class: impl Into<String>,
        at: UtcTimestamp,
    ) -> Self {
        Self {
            vendor: vendor.into(),
            model: model.into(),
            token_class: token_class.into(),
            at,
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

impl<C: Currency> ValuationOutcome<C> {
    /// Combines two valuation outcomes, accumulating subtotals and missing rates.
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Complete(a), Self::Complete(b)) => Self::Complete(a + b),
            (
                Self::Complete(a),
                Self::Incomplete {
                    known_price_subtotal,
                    missing_rates,
                },
            ) => Self::Incomplete {
                known_price_subtotal: a + known_price_subtotal,
                missing_rates,
            },
            (
                Self::Incomplete {
                    known_price_subtotal,
                    missing_rates,
                },
                Self::Complete(b),
            ) => Self::Incomplete {
                known_price_subtotal: known_price_subtotal + b,
                missing_rates,
            },
            (
                Self::Incomplete {
                    known_price_subtotal: a,
                    missing_rates: mut m_a,
                },
                Self::Incomplete {
                    known_price_subtotal: b,
                    missing_rates: m_b,
                },
            ) => {
                m_a.extend(m_b);
                Self::Incomplete {
                    known_price_subtotal: a + b,
                    missing_rates: m_a,
                }
            }
            (Self::UnsupportedCurrency { found, expected }, _)
            | (_, Self::UnsupportedCurrency { found, expected }) => {
                Self::UnsupportedCurrency { found, expected }
            }
        }
    }
}

/// An immutable in-memory book of versioned rate cards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateBook {
    cards: Vec<RateCard>,
    explicit_version: Option<RateCardId>,
}

impl RateBook {
    /// Constructs a rate book from a slice of rate cards.
    pub fn new(cards: Vec<RateCard>) -> Self {
        Self {
            cards,
            explicit_version: None,
        }
    }

    /// Constructs a rate book with an explicit version identifier.
    pub fn with_version(cards: Vec<RateCard>, version: RateCardId) -> Self {
        Self {
            cards,
            explicit_version: Some(version),
        }
    }

    /// The list of rate cards in this book.
    pub fn cards(&self) -> &[RateCard] {
        &self.cards
    }

    /// The rate card version identifier for report metadata and explain provenance.
    pub fn version(&self) -> Option<RateCardId> {
        if let Some(explicit) = &self.explicit_version {
            return Some(explicit.clone());
        }
        if self.cards.is_empty() {
            return None;
        }
        let latest_date = self
            .cards
            .iter()
            .map(|card| card.draft.effective_start)
            .max()?;
        Some(RateCardId::new(format!("rate-card-{}", latest_date.iso())))
    }

    /// Returns rate cards that have reached their configured review-due date.
    pub fn stale_cards(&self, at: UtcDate) -> Vec<&RateCard> {
        self.cards
            .iter()
            .filter(|card| match &card.draft.review_due {
                ReviewDuePolicy::On(due_date) => at >= *due_date,
                ReviewDuePolicy::None => false,
            })
            .collect()
    }

    /// Finds the rate card in force for a specific vendor, model, token class,
    /// and instant (aub-pwtn).
    ///
    /// Vendor and model match exactly (after trimming and lowercasing, so an
    /// operator-written mapping rule that differs only in case still reaches
    /// its card): no substring matching, so a schedule attached to
    /// `deepseek-v4-flash` never reaches `deepseek-v4-flash-vision-exp`.
    /// Among the cards effective on the instant's date, a scheduled card
    /// containing the instant wins, the narrowest window first; otherwise the
    /// unscheduled default card. The tie-break by `effective_start` then `id`
    /// (then `imported_at`) stays for the exact case.
    pub fn find_rate(
        &self,
        vendor: &str,
        model: &str,
        token_class: TokenClass,
        at: UtcTimestamp,
    ) -> Option<&RateCard> {
        let date = at.utc_date();
        let weekday = at.weekday();
        let (hour, minute) = at.hour_minute_utc();
        let minutes = hour * 60 + minute;
        let normalized_vendor = vendor.trim().to_ascii_lowercase();
        let normalized_model = model.trim().to_ascii_lowercase();

        let mut best_default: Option<&RateCard> = None;
        let mut best_scheduled: Option<(&RateCard, u16)> = None;
        for card in &self.cards {
            if !card_exact_date_match(
                card,
                &normalized_vendor,
                &normalized_model,
                token_class,
                date,
            ) {
                continue;
            }
            match &card.draft.schedule {
                None => {
                    if is_later_revision(best_default, card) {
                        best_default = Some(card);
                    }
                }
                Some(schedule) => {
                    if !schedule.contains(weekday, minutes) {
                        continue;
                    }
                    let width = schedule.window_minutes();
                    if is_narrower_or_later(best_scheduled, card, width) {
                        best_scheduled = Some((card, width));
                    }
                }
            }
        }
        best_scheduled.map(|(card, _)| card).or(best_default)
    }

    /// Finds the unscheduled default card for a vendor, model, token class,
    /// and date, ignoring every scheduled card (aub-pwtn).
    ///
    /// An event whose timestamp is a heuristic (the transcript stated no
    /// instant and one was inferred) is valued here rather than through
    /// [`Self::find_rate`], so a valuation never silently assumes off-peak
    /// for an event whose hour is unknown. Same exact matching and tie-break
    /// as [`Self::find_rate`].
    pub fn find_default_rate(
        &self,
        vendor: &str,
        model: &str,
        token_class: TokenClass,
        date: UtcDate,
    ) -> Option<&RateCard> {
        let normalized_vendor = vendor.trim().to_ascii_lowercase();
        let normalized_model = model.trim().to_ascii_lowercase();

        let mut best: Option<&RateCard> = None;
        for card in &self.cards {
            if card.draft.schedule.is_some() {
                continue;
            }
            if !card_exact_date_match(
                card,
                &normalized_vendor,
                &normalized_model,
                token_class,
                date,
            ) {
                continue;
            }
            if is_later_revision(best, card) {
                best = Some(card);
            }
        }
        best
    }

    /// Finds a rate card for an arbitrary / unknown token class name by string match.
    pub fn find_custom_rate(
        &self,
        vendor: &str,
        model: &str,
        token_class_name: &str,
        at: UtcTimestamp,
    ) -> Option<&RateCard> {
        if let Some(known_class) = TokenClass::parse(token_class_name) {
            return self.find_rate(vendor, model, known_class, at);
        }
        None
    }

    /// Finds the default card for an arbitrary / unknown token class name,
    /// ignoring every scheduled card (aub-pwtn).
    pub fn find_default_custom_rate(
        &self,
        vendor: &str,
        model: &str,
        token_class_name: &str,
        date: UtcDate,
    ) -> Option<&RateCard> {
        if let Some(known_class) = TokenClass::parse(token_class_name) {
            return self.find_default_rate(vendor, model, known_class, date);
        }
        None
    }
}

/// Exact vendor and model match (after trimming and lowercasing) with exact
/// class and date effectiveness. The effective interval keeps the valuation
/// layer's long-standing end-inclusive reading: a card whose `effective_end`
/// is a date still prices that date.
fn card_exact_date_match(
    card: &RateCard,
    normalized_vendor: &str,
    normalized_model: &str,
    token_class: TokenClass,
    date: UtcDate,
) -> bool {
    card.draft.vendor.trim().to_ascii_lowercase() == normalized_vendor
        && card.draft.model.trim().to_ascii_lowercase() == normalized_model
        && card.draft.token_class == token_class
        && date >= card.draft.effective_start
        && card.draft.effective_end.is_none_or(|end| date <= end)
}

/// True when `card` outranks the current best default: the later
/// `effective_start`, then the higher row id, then the later import.
fn is_later_revision(best: Option<&RateCard>, card: &RateCard) -> bool {
    match best {
        None => true,
        Some(current) => {
            (card.draft.effective_start, card.id, card.imported_at)
                > (
                    current.draft.effective_start,
                    current.id,
                    current.imported_at,
                )
        }
    }
}

/// True when a containing scheduled card outranks the current best: the
/// narrower window first, then the same revision tie-break as defaults.
fn is_narrower_or_later(best: Option<(&RateCard, u16)>, card: &RateCard, width: u16) -> bool {
    match best {
        None => true,
        Some((current, current_width)) => {
            width < current_width
                || (width == current_width && is_later_revision(Some(current), card))
        }
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

/// One rate card that priced one token class of a usage vector: the vendor,
/// model and class it priced, and the schedule that selected it (`None` is
/// the unscheduled default card). Carried alongside the money so `--explain`
/// can name which card valued each group (aub-pwtn).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UsedRateCard {
    pub vendor: String,
    pub model: String,
    pub token_class: String,
    pub schedule: Option<Schedule>,
}

impl UsedRateCard {
    fn of(card: &RateCard) -> Self {
        Self {
            vendor: card.draft.vendor.clone(),
            model: card.draft.model.clone(),
            token_class: card.draft.token_class.as_str().to_string(),
            schedule: card.draft.schedule,
        }
    }

    /// The schedule half of the `--explain` card line: the window
    /// [`Schedule::describe`] renders, or `default` for an unscheduled card.
    /// The literal lives here rather than on [`Schedule`] so the two
    /// spellings cannot drift apart inside the domain type.
    pub fn schedule_label(&self) -> String {
        self.schedule
            .map(|schedule| schedule.describe())
            .unwrap_or_else(|| "default".to_string())
    }
}

/// Values a `UsageVector` at API list-price equivalent in currency `C`, at
/// the instant the usage happened: the card in force at that instant prices
/// each class (aub-pwtn).
///
/// Exhaustively checks every known token kind and unknown component.
/// If any non-zero token component lacks a rate, returns `ValuationOutcome::Incomplete`
/// naming each missing rate and providing the known-price subtotal.
pub fn value_usage_vector<C: Currency>(
    book: &RateBook,
    vendor: &str,
    model: &str,
    at: UtcTimestamp,
    usage: &UsageVector,
) -> ValuationOutcome<C> {
    value_scoped::<C>(book, vendor, model, at, false, usage).0
}

/// Values a `UsageVector` at the unscheduled default cards only, for an event
/// whose timestamp is a heuristic (aub-pwtn). Same contract as
/// [`value_usage_vector`], except schedule windows never participate: a class
/// with only a scheduled card is unvalued rather than valued at a peak the
/// event may not belong to.
pub fn value_usage_vector_default_card<C: Currency>(
    book: &RateBook,
    vendor: &str,
    model: &str,
    at: UtcTimestamp,
    usage: &UsageVector,
) -> ValuationOutcome<C> {
    value_scoped::<C>(book, vendor, model, at, true, usage).0
}

/// The cards [`value_usage_vector`] used for one usage vector, for `--explain`.
/// Empty when nothing priced (every present class unvalued).
pub fn used_rate_cards(
    book: &RateBook,
    vendor: &str,
    model: &str,
    at: UtcTimestamp,
    usage: &UsageVector,
) -> std::collections::BTreeSet<UsedRateCard> {
    value_scoped::<Usd>(book, vendor, model, at, false, usage).1
}

/// The cards [`value_usage_vector_default_card`] used, for `--explain`.
pub fn used_rate_cards_default_card(
    book: &RateBook,
    vendor: &str,
    model: &str,
    at: UtcTimestamp,
    usage: &UsageVector,
) -> std::collections::BTreeSet<UsedRateCard> {
    value_scoped::<Usd>(book, vendor, model, at, true, usage).1
}

#[allow(clippy::type_complexity)]
fn value_scoped<C: Currency>(
    book: &RateBook,
    vendor: &str,
    model: &str,
    at: UtcTimestamp,
    default_only: bool,
    usage: &UsageVector,
) -> (
    ValuationOutcome<C>,
    std::collections::BTreeSet<UsedRateCard>,
) {
    let date = at.utc_date();
    let mut total_micros: i64 = 0;
    let mut missing_rates = Vec::new();
    let mut cards = std::collections::BTreeSet::new();

    let select = |class: TokenClass| {
        if default_only {
            book.find_default_rate(vendor, model, class, date)
        } else {
            book.find_rate(vendor, model, class, at)
        }
    };
    let select_custom = |class_name: &str| {
        if default_only {
            book.find_default_custom_rate(vendor, model, class_name, date)
        } else {
            book.find_custom_rate(vendor, model, class_name, at)
        }
    };

    // 1. Evaluate known token kinds exhaustively
    for &kind in &TokenKind::ALL {
        let count = usage.known().value(kind);
        if count == 0 {
            continue;
        }

        let class = token_kind_to_class(kind);
        match select(class) {
            Some(card) => {
                if card.draft.currency.as_str() != C::CODE {
                    return (
                        ValuationOutcome::UnsupportedCurrency {
                            found: card.draft.currency,
                            expected: C::CODE,
                        },
                        cards,
                    );
                }
                let rate =
                    MoneyPerMillionTokens::<C>::from_micros_per_million(card.draft.rate_micros);
                let cost = rate.times_tokens(count);
                total_micros += cost.micros();
                cards.insert(UsedRateCard::of(card));
            }
            None => {
                missing_rates.push(MissingRate::new(vendor, model, class.as_str(), at));
            }
        }
    }

    // 2. Evaluate unknown token components
    for (class_name, count) in usage.unknown() {
        if count.value() == 0 {
            continue;
        }

        match select_custom(class_name) {
            Some(card) => {
                if card.draft.currency.as_str() != C::CODE {
                    return (
                        ValuationOutcome::UnsupportedCurrency {
                            found: card.draft.currency,
                            expected: C::CODE,
                        },
                        cards,
                    );
                }
                let rate =
                    MoneyPerMillionTokens::<C>::from_micros_per_million(card.draft.rate_micros);
                let cost = rate.times_tokens(count.value());
                total_micros += cost.micros();
                cards.insert(UsedRateCard::of(card));
            }
            None => {
                missing_rates.push(MissingRate::new(vendor, model, class_name.as_str(), at));
            }
        }
    }

    let subtotal = ApiListPriceEquivalent::new(Money::<C>::from_micros(total_micros));
    let outcome = if missing_rates.is_empty() {
        ValuationOutcome::Complete(subtotal)
    } else {
        ValuationOutcome::Incomplete {
            known_price_subtotal: subtotal,
            missing_rates,
        }
    };
    (outcome, cards)
}

/// Values a collection of usage vectors and aggregates them into one result.
///
/// Order independent: summing integer micros is associative and commutative.
pub fn value_batch<C: Currency>(
    book: &RateBook,
    items: &[(&str, &str, UtcTimestamp, &UsageVector)],
) -> ValuationOutcome<C> {
    let mut total_micros: i64 = 0;
    let mut all_missing = Vec::new();

    for &(vendor, model, at, usage) in items {
        match value_usage_vector::<C>(book, vendor, model, at, usage) {
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
                schedule: None,
                publication: Publication {
                    source: Some("test".to_string()),
                    published_at: None,
                },
                review_due: ReviewDuePolicy::None,
            },
        }
    }

    /// A scheduled card for the schedule-selection tests: same shape as
    /// [`make_card`] with a weekday window attached.
    #[allow(clippy::too_many_arguments)]
    fn make_scheduled_card(
        id: i64,
        vendor: &str,
        model: &str,
        token_class: TokenClass,
        rate_micros: i64,
        effective_start: &str,
        days_iso: &[u32],
        start_minutes: u16,
        end_minutes: u16,
    ) -> RateCard {
        let mut card = make_card(
            id,
            vendor,
            model,
            token_class,
            rate_micros,
            effective_start,
            None,
        );
        card.draft.schedule = Schedule::new(days_iso, start_minutes, end_minutes);
        card
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
        let date = UtcDate::parse("2024-07-01").unwrap().start();

        let res = value_usage_vector::<Usd>(&book, "anthropic", "claude-3-5-sonnet", date, &usage);
        match res {
            ValuationOutcome::Complete(equiv) => {
                assert_eq!(
                    equiv.micros(),
                    652_500,
                    "expected exact hand-computed 652,500 micros ($0.6525)"
                );
            }
            ValuationOutcome::Incomplete { .. } | ValuationOutcome::UnsupportedCurrency { .. } => {
                panic!("expected Complete valuation, got {res:?}");
            }
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
        let before = UtcDate::parse("2024-05-31").unwrap().start();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", before, &usage),
            ValuationOutcome::Incomplete { .. }
        ));

        // Exactly on effective start -> complete
        let start = UtcDate::parse("2024-06-01").unwrap().start();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", start, &usage),
            ValuationOutcome::Complete(..)
        ));

        // Exactly on effective end -> complete
        let end = UtcDate::parse("2024-06-30").unwrap().start();
        assert!(matches!(
            value_usage_vector::<Usd>(&book, "anthropic", "claude-3-sonnet", end, &usage),
            ValuationOutcome::Complete(..)
        ));

        // Day after effective end -> missing rate
        let after = UtcDate::parse("2024-07-01").unwrap().start();
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
        let date = UtcDate::parse("2024-07-01").unwrap().start();

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
            ValuationOutcome::Complete(..) | ValuationOutcome::UnsupportedCurrency { .. } => {
                panic!("expected Incomplete outcome, got {outcome:?}");
            }
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

        let date_july = UtcDate::parse("2024-07-15").unwrap().start();
        let date_august = UtcDate::parse("2024-08-15").unwrap().start();

        let cost_july =
            match value_usage_vector::<Usd>(&book, "openai", "gpt-4o", date_july, &usage) {
                ValuationOutcome::Complete(eq) => eq,
                ValuationOutcome::Incomplete { .. }
                | ValuationOutcome::UnsupportedCurrency { .. } => {
                    panic!("expected July to be complete");
                }
            };
        assert_eq!(cost_july.micros(), 5_000_000); // $5.00

        let cost_aug =
            match value_usage_vector::<Usd>(&book, "openai", "gpt-4o", date_august, &usage) {
                ValuationOutcome::Complete(eq) => eq,
                ValuationOutcome::Incomplete { .. }
                | ValuationOutcome::UnsupportedCurrency { .. } => {
                    panic!("expected August to be complete");
                }
            };
        assert_eq!(cost_aug.micros(), 2_500_000); // $2.50

        let total = cost_july + cost_aug;
        assert_eq!(total.micros(), 7_500_000); // $7.50
    }

    /// The Ollama DeepSeek book: one default input row at $0.22/M and one
    /// weekday 12:00 to 18:00 UTC peak row at $0.44/M for
    /// `deepseek-v4-flash`, both open-ended from 2026-09-07.
    fn ollama_deepseek_book() -> RateBook {
        RateBook::new(vec![
            make_card(
                1,
                "ollama",
                "deepseek-v4-flash",
                TokenClass::Input,
                220_000,
                "2026-09-07",
                None,
            ),
            make_scheduled_card(
                2,
                "ollama",
                "deepseek-v4-flash",
                TokenClass::Input,
                440_000,
                "2026-09-07",
                &[1, 2, 3, 4, 5],
                12 * 60,
                18 * 60,
            ),
        ])
    }

    fn valued_micros(book: &RateBook, at: UtcTimestamp, usage: &UsageVector) -> i64 {
        match value_usage_vector::<Usd>(book, "ollama", "deepseek-v4-flash", at, usage) {
            ValuationOutcome::Complete(equiv) => equiv.micros(),
            ValuationOutcome::Incomplete { .. } | ValuationOutcome::UnsupportedCurrency { .. } => {
                panic!("expected a complete valuation at {at:?}");
            }
        }
    }

    /// The five acceptance instants: a weekday evening is the default, a
    /// weekday afternoon is the peak, a Saturday afternoon is the default,
    /// and the window is start-inclusive and end-exclusive.
    #[test]
    fn peak_schedule_values_each_instant_at_the_card_in_force() {
        let book = ollama_deepseek_book();
        let usage = sample_usage(1_000_000, 0, 0, 0);
        // 1M input tokens: $0.22 default (220_000 micros), $0.44 peak (440_000).
        let instant = UtcTimestamp::parse_rfc3339;
        assert_eq!(
            valued_micros(&book, instant("2026-09-08T20:00:00Z").unwrap(), &usage),
            220_000,
            "Tuesday 20:00 UTC is past the window"
        );
        assert_eq!(
            valued_micros(&book, instant("2026-09-08T14:00:00Z").unwrap(), &usage),
            440_000,
            "Tuesday 14:00 UTC is inside the window"
        );
        assert_eq!(
            valued_micros(&book, instant("2026-09-12T14:00:00Z").unwrap(), &usage),
            220_000,
            "Saturday 14:00 UTC shares the hours but not the days"
        );
        assert_eq!(
            valued_micros(&book, instant("2026-09-08T12:00:00Z").unwrap(), &usage),
            440_000,
            "the window start is inclusive"
        );
        assert_eq!(
            valued_micros(&book, instant("2026-09-08T18:00:00Z").unwrap(), &usage),
            220_000,
            "the window end is exclusive"
        );
    }

    /// The heuristic-timestamp path values at the default card even inside
    /// the peak window, so an unknown hour never silently becomes a peak.
    #[test]
    fn a_heuristic_timestamp_values_at_the_default_card() {
        let book = ollama_deepseek_book();
        let usage = sample_usage(1_000_000, 0, 0, 0);
        let peak = UtcTimestamp::parse_rfc3339("2026-09-08T14:00:00Z").unwrap();
        match value_usage_vector_default_card::<Usd>(
            &book,
            "ollama",
            "deepseek-v4-flash",
            peak,
            &usage,
        ) {
            ValuationOutcome::Complete(equiv) => assert_eq!(equiv.micros(), 220_000),
            ValuationOutcome::Incomplete { .. } | ValuationOutcome::UnsupportedCurrency { .. } => {
                panic!("the default card prices a peak-hour heuristic event");
            }
        }
        let cards =
            used_rate_cards_default_card(&book, "ollama", "deepseek-v4-flash", peak, &usage);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards.iter().next().unwrap().schedule_label(), "default");
        let cards = used_rate_cards(&book, "ollama", "deepseek-v4-flash", peak, &usage);
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards.iter().next().unwrap().schedule_label(),
            "peak mon-fri 12:00-18:00 UTC"
        );
    }

    /// Vendor and model match exactly: a book holding only
    /// `deepseek-v4-flash` leaves `deepseek-v4-flash-vision-exp` unvalued.
    /// Under the old substring match the longer id contained the card's
    /// model and priced; this test fails on that implementation.
    #[test]
    fn vendor_and_model_match_exactly_not_by_substring() {
        let book = ollama_deepseek_book();
        let usage = sample_usage(1_000_000, 0, 0, 0);
        let peak = UtcTimestamp::parse_rfc3339("2026-09-08T14:00:00Z").unwrap();
        match value_usage_vector::<Usd>(
            &book,
            "ollama",
            "deepseek-v4-flash-vision-exp",
            peak,
            &usage,
        ) {
            ValuationOutcome::Incomplete { missing_rates, .. } => {
                assert_eq!(missing_rates.len(), 1);
                assert_eq!(missing_rates[0].model, "deepseek-v4-flash-vision-exp");
            }
            ValuationOutcome::Complete(..) | ValuationOutcome::UnsupportedCurrency { .. } => {
                panic!("a model the book does not name must stay unvalued");
            }
        }
        // The named model still prices at the same instant.
        assert_eq!(valued_micros(&book, peak, &usage), 440_000);
    }

    /// Property: over a book holding a default, a weekday peak and a
    /// disjoint weekend card, every instant of a week resolves to at most
    /// one card, and it is the scheduled one whenever a scheduled card
    /// contains the instant, else the default. A brute-force oracle over
    /// the same three cards decides the expectation independently of
    /// `find_rate`.
    #[test]
    fn selection_is_deterministic_and_prefers_the_containing_schedule() {
        let book = RateBook::new(vec![
            make_card(
                1,
                "ollama",
                "deepseek-v4-flash",
                TokenClass::Input,
                220_000,
                "2026-09-07",
                None,
            ),
            make_scheduled_card(
                2,
                "ollama",
                "deepseek-v4-flash",
                TokenClass::Input,
                440_000,
                "2026-09-07",
                &[1, 2, 3, 4, 5],
                12 * 60,
                18 * 60,
            ),
            make_scheduled_card(
                3,
                "ollama",
                "deepseek-v4-flash",
                TokenClass::Input,
                110_000,
                "2026-09-07",
                &[6, 7],
                0,
                24 * 60,
            ),
        ]);
        // Monday 2026-09-07 00:00 UTC through Sunday 23:00 UTC, hourly.
        let week_start = UtcTimestamp::parse_rfc3339("2026-09-07T00:00:00Z")
            .unwrap()
            .unix_nanos();
        for hour_offset in 0..(7 * 24) {
            let at = UtcTimestamp::from_unix_nanos(
                week_start + i64::from(hour_offset) * 3_600 * 1_000_000_000,
            );
            let found = book.find_rate("ollama", "deepseek-v4-flash", TokenClass::Input, at);
            let expected = oracle_rate_micros(at);
            assert_eq!(
                found.map(|card| card.draft.rate_micros),
                Some(expected),
                "wrong card at weekday={} {:02}:{:02}",
                at.weekday(),
                at.hour_minute_utc().0,
                at.hour_minute_utc().1,
            );
        }
    }

    /// The brute-force oracle for the property above: the weekday peak
    /// window, else the weekend all-day card on Saturday and Sunday, else
    /// the default. Written against the clock, never against `find_rate`.
    fn oracle_rate_micros(at: UtcTimestamp) -> i64 {
        let weekday = at.weekday();
        let (hour, minute) = at.hour_minute_utc();
        let minutes = hour * 60 + minute;
        if (1..=5).contains(&weekday) && (12 * 60..18 * 60).contains(&minutes) {
            440_000
        } else if weekday >= 6 {
            110_000
        } else {
            220_000
        }
    }
}
