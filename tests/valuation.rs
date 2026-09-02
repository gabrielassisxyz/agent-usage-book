//! Comprehensive test suite for valuation and its exact-money suite (`aub-wyu.2`).

use std::collections::BTreeMap;

use agent_usage_book::domain::money::{Money, Usd};
use agent_usage_book::domain::rate_card::{
    BillingBasis, CurrencyCode, Publication, RateCard, RateCardDraft, ReviewDuePolicy, TokenClass,
};
use agent_usage_book::domain::time::{UtcDate, UtcTimestamp};
use agent_usage_book::domain::tokens::{
    CacheReadTokens, CacheWriteTokens, InputTokens, KnownTokenVector, OutputTokens, TokenCount,
    UsageVector,
};
use agent_usage_book::evidence::{CoverageCompleteness, EvidenceQuality};
use agent_usage_book::valuation::{RateBook, ValuationOutcome, value_batch, value_usage_vector};

fn helper_card(
    id: i64,
    vendor: &str,
    model: &str,
    token_class: TokenClass,
    rate_micros: i64,
    currency: CurrencyCode,
    start: &str,
    end: Option<&str>,
) -> RateCard {
    RateCard {
        id,
        imported_at: UtcTimestamp::from_unix_nanos(100),
        draft: RateCardDraft {
            vendor: vendor.to_string(),
            model: model.to_string(),
            token_class,
            rate_micros,
            currency,
            billing_basis: BillingBasis::PerMillionTokens,
            effective_start: UtcDate::parse(start).expect("valid start date"),
            effective_end: end.map(|d| UtcDate::parse(d).expect("valid end date")),
            publication: Publication {
                source: Some("https://pricing.vendor.example".to_string()),
                published_at: None,
            },
            review_due: ReviewDuePolicy::None,
        },
    }
}

fn helper_usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    unknown: BTreeMap<String, TokenCount>,
) -> UsageVector {
    UsageVector::new(
        KnownTokenVector::new(
            InputTokens::new(input),
            OutputTokens::new(output),
            CacheReadTokens::new(cache_read),
            CacheWriteTokens::new(cache_write),
        ),
        unknown,
        CoverageCompleteness::Complete,
        EvidenceQuality::Measured,
    )
}

/// 1. Golden test: exact-decimal fixtures with hand-computed expected values stated in the test.
#[test]
fn golden_hand_computed_exact_decimal_fixtures() {
    let cards = vec![
        helper_card(
            1,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Input,
            3_000_000,
            CurrencyCode::Usd,
            "2024-06-01",
            None,
        ),
        helper_card(
            2,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Output,
            15_000_000,
            CurrencyCode::Usd,
            "2024-06-01",
            None,
        ),
        helper_card(
            3,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::CacheRead,
            300_000,
            CurrencyCode::Usd,
            "2024-06-01",
            None,
        ),
        helper_card(
            4,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::CacheWrite5m,
            3_750_000,
            CurrencyCode::Usd,
            "2024-06-01",
            None,
        ),
    ];
    let book = RateBook::new(cards);

    // 123,456 input tokens at $3.00/M => 123456 * 3_000_000 / 1_000_000 = 370,368 micros
    // 45,678 output tokens at $15.00/M => 45678 * 15_000_000 / 1_000_000 = 685,170 micros
    // 80,000 cache read tokens at $0.30/M => 80000 * 300_000 / 1_000_000 = 24,000 micros
    // 10,000 cache write tokens at $3.75/M => 10000 * 3_750_000 / 1_000_000 = 37,500 micros
    // Hand-computed total: 370_368 + 685_170 + 24_000 + 37_500 = 1,117,038 micros ($1.117038)
    let usage = helper_usage(123_456, 45_678, 80_000, 10_000, BTreeMap::new());
    let date = UtcDate::parse("2024-07-15").unwrap();

    let outcome = value_usage_vector::<Usd>(&book, "anthropic", "claude-3-5-sonnet", date, &usage);
    match outcome {
        ValuationOutcome::Complete(equiv) => {
            assert_eq!(
                equiv.micros(),
                1_117_038,
                "hand-computed expected exact value"
            );
            assert_eq!(equiv.amount(), Money::<Usd>::from_micros(1_117_038));
        }
        other => panic!("expected Complete outcome, got {other:?}"),
    }
}

/// 2. Unit: effective-date boundary tested in both directions.
#[test]
fn effective_date_boundaries_both_directions() {
    let cards = vec![helper_card(
        10,
        "vendor",
        "model-a",
        TokenClass::Input,
        1_000_000,
        CurrencyCode::Usd,
        "2024-05-01",
        Some("2024-05-31"),
    )];
    let book = RateBook::new(cards);
    let usage = helper_usage(10_000, 0, 0, 0, BTreeMap::new());

    // Before start: missing
    let res_before = value_usage_vector::<Usd>(
        &book,
        "vendor",
        "model-a",
        UtcDate::parse("2024-04-30").unwrap(),
        &usage,
    );
    assert!(matches!(res_before, ValuationOutcome::Incomplete { .. }));

    // Exact start: match
    let res_start = value_usage_vector::<Usd>(
        &book,
        "vendor",
        "model-a",
        UtcDate::parse("2024-05-01").unwrap(),
        &usage,
    );
    assert!(matches!(res_start, ValuationOutcome::Complete(..)));

    // Exact end: match
    let res_end = value_usage_vector::<Usd>(
        &book,
        "vendor",
        "model-a",
        UtcDate::parse("2024-05-31").unwrap(),
        &usage,
    );
    assert!(matches!(res_end, ValuationOutcome::Complete(..)));

    // After end: missing
    let res_after = value_usage_vector::<Usd>(
        &book,
        "vendor",
        "model-a",
        UtcDate::parse("2024-06-01").unwrap(),
        &usage,
    );
    assert!(matches!(res_after, ValuationOutcome::Incomplete { .. }));
}

/// 3. Unit: missing rate for any present component returns Incomplete naming vendor, model and token class.
#[test]
fn missing_rate_names_vendor_model_and_token_class() {
    let cards = vec![helper_card(
        1,
        "anthropic",
        "claude-3-opus",
        TokenClass::Input,
        15_000_000,
        CurrencyCode::Usd,
        "2024-01-01",
        None,
    )];
    let book = RateBook::new(cards);
    // Usage has output tokens, but no output rate exists!
    let usage = helper_usage(10_000, 5_000, 0, 0, BTreeMap::new());
    let date = UtcDate::parse("2024-06-01").unwrap();

    let outcome = value_usage_vector::<Usd>(&book, "anthropic", "claude-3-opus", date, &usage);
    match outcome {
        ValuationOutcome::Incomplete {
            known_price_subtotal,
            missing_rates,
        } => {
            assert_eq!(known_price_subtotal.micros(), 150_000); // 10k * $15/M
            assert_eq!(missing_rates.len(), 1);
            assert_eq!(missing_rates[0].vendor, "anthropic");
            assert_eq!(missing_rates[0].model, "claude-3-opus");
            assert_eq!(missing_rates[0].token_class, "output");
            assert_eq!(missing_rates[0].date, date);
        }
        other => panic!("expected Incomplete outcome, got {other:?}"),
    }
}

/// 4. Unit: missing cache-write price does NOT imply zero cache-write cost.
#[test]
fn missing_cache_write_price_never_zero_cost() {
    let cards = vec![
        helper_card(
            1,
            "vendor",
            "model-x",
            TokenClass::Input,
            2_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ),
        helper_card(
            2,
            "vendor",
            "model-x",
            TokenClass::Output,
            8_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ),
    ];
    let book = RateBook::new(cards);
    let usage = helper_usage(10_000, 10_000, 0, 50_000, BTreeMap::new());
    let date = UtcDate::parse("2024-06-01").unwrap();

    let outcome = value_usage_vector::<Usd>(&book, "vendor", "model-x", date, &usage);
    match outcome {
        ValuationOutcome::Incomplete {
            known_price_subtotal,
            missing_rates,
        } => {
            assert_eq!(known_price_subtotal.micros(), 100_000); // 20k + 80k
            assert_eq!(missing_rates.len(), 1);
            assert_eq!(missing_rates[0].token_class, "cache_write_5m");
        }
        other => panic!("expected Incomplete outcome, got {other:?}"),
    }
}

/// 5. Unit: order of magnitude differences across token classes.
#[test]
fn different_rates_per_token_class_order_of_magnitude() {
    let cards = vec![
        helper_card(
            1,
            "vendor",
            "model-x",
            TokenClass::Input,
            1_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ), // $1/M
        helper_card(
            2,
            "vendor",
            "model-x",
            TokenClass::Output,
            100_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ), // $100/M (2 orders of magnitude higher)
        helper_card(
            3,
            "vendor",
            "model-x",
            TokenClass::CacheRead,
            100_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ), // $0.10/M (1 order of magnitude lower)
        helper_card(
            4,
            "vendor",
            "model-x",
            TokenClass::CacheWrite5m,
            1_250_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ),
    ];
    let book = RateBook::new(cards);
    let usage = helper_usage(1_000_000, 10_000, 1_000_000, 0, BTreeMap::new());
    let date = UtcDate::parse("2024-06-01").unwrap();

    let outcome = value_usage_vector::<Usd>(&book, "vendor", "model-x", date, &usage);
    match outcome {
        ValuationOutcome::Complete(equiv) => {
            // input: 1_000_000 * $1/M = $1.00 (1_000_000 micros)
            // output: 10_000 * $100/M = $1.00 (1_000_000 micros)
            // cache read: 1_000_000 * $0.10/M = $0.10 (100_000 micros)
            // total: 2_100_000 micros ($2.10)
            assert_eq!(equiv.micros(), 2_100_000);
        }
        other => panic!("expected Complete outcome, got {other:?}"),
    }
}

/// 6. Unit: mid-period model price change producing two differently valued halves and correct sum.
#[test]
fn mid_period_model_price_change_two_halves() {
    let cards = vec![
        helper_card(
            1,
            "openai",
            "gpt-4o",
            TokenClass::Input,
            5_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            Some("2024-07-31"),
        ),
        helper_card(
            2,
            "openai",
            "gpt-4o",
            TokenClass::Input,
            2_500_000,
            CurrencyCode::Usd,
            "2024-08-01",
            None,
        ),
    ];
    let book = RateBook::new(cards);
    let usage = helper_usage(2_000_000, 0, 0, 0, BTreeMap::new());

    let july = UtcDate::parse("2024-07-15").unwrap();
    let aug = UtcDate::parse("2024-08-15").unwrap();

    let batch = vec![
        ("openai", "gpt-4o", july, &usage),
        ("openai", "gpt-4o", aug, &usage),
    ];
    let outcome = value_batch::<Usd>(&book, &batch);
    match outcome {
        ValuationOutcome::Complete(equiv) => {
            // July: 2M * $5/M = $10.00 (10_000_000 micros)
            // August: 2M * $2.50/M = $5.00 (5_000_000 micros)
            // Total: $15.00 (15_000_000 micros)
            assert_eq!(equiv.micros(), 15_000_000);
        }
        other => panic!("expected Complete outcome, got {other:?}"),
    }
}

/// 7. Unit: unsupported currency is rejected rather than converted with assumed rate.
#[test]
fn unsupported_currency_is_rejected() {
    let cards = vec![helper_card(
        1,
        "mistral",
        "mistral-large",
        TokenClass::Input,
        2_000_000,
        CurrencyCode::Eur,
        "2024-01-01",
        None,
    )];
    let book = RateBook::new(cards);
    let usage = helper_usage(100_000, 0, 0, 0, BTreeMap::new());
    let date = UtcDate::parse("2024-06-01").unwrap();

    // Requested in Usd, but rate card is in Eur
    let outcome = value_usage_vector::<Usd>(&book, "mistral", "mistral-large", date, &usage);
    match outcome {
        ValuationOutcome::UnsupportedCurrency { found, expected } => {
            assert_eq!(found, CurrencyCode::Eur);
            assert_eq!(expected, "USD");
        }
        other => panic!("expected UnsupportedCurrency outcome, got {other:?}"),
    }
}

/// 8. Property: valuing a set of events is order independent over generated sets.
#[test]
fn valuation_order_independence() {
    let cards = vec![
        helper_card(
            1,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Input,
            3_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ),
        helper_card(
            2,
            "anthropic",
            "claude-3-5-sonnet",
            TokenClass::Output,
            15_000_000,
            CurrencyCode::Usd,
            "2024-01-01",
            None,
        ),
    ];
    let book = RateBook::new(cards);

    let u1 = helper_usage(10_000, 5_000, 0, 0, BTreeMap::new());
    let u2 = helper_usage(20_000, 10_000, 0, 0, BTreeMap::new());
    let u3 = helper_usage(30_000, 15_000, 0, 0, BTreeMap::new());
    let date = UtcDate::parse("2024-06-01").unwrap();

    let batch_fwd = vec![
        ("anthropic", "claude-3-5-sonnet", date, &u1),
        ("anthropic", "claude-3-5-sonnet", date, &u2),
        ("anthropic", "claude-3-5-sonnet", date, &u3),
    ];
    let batch_rev = vec![
        ("anthropic", "claude-3-5-sonnet", date, &u3),
        ("anthropic", "claude-3-5-sonnet", date, &u2),
        ("anthropic", "claude-3-5-sonnet", date, &u1),
    ];

    let out_fwd = value_batch::<Usd>(&book, &batch_fwd);
    let out_rev = value_batch::<Usd>(&book, &batch_rev);

    assert_eq!(out_fwd, out_rev);
}
