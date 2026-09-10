//! The shipped rate book (`aub-b8ng`): the dated book at `rate-book/rates.toml`
//! is what the operator imports, so its internal consistency is a property of
//! the repository rather than of whoever last edited it.
//!
//! The four checks below are written as functions over a parsed book and then
//! run twice: once against the shipped file, which must report nothing, and
//! once against a book that differs from a passing one in exactly the dimension
//! the check owns, which must report it. A consistency check that has never
//! failed is indistinguishable from one that cannot fail.

use std::collections::{BTreeMap, BTreeSet};

use agent_usage_book::domain::rate_card::{RateCardDraft, TokenClass};
use agent_usage_book::domain::time::UtcDate;
use agent_usage_book::rate_book::{self, RateBook};

const SHIPPED: &str = include_str!("../rate-book/rates.toml");

/// The vendors this book is allowed to carry. A new one is a deliberate act:
/// every check below is written per vendor, so an unlisted vendor would slip
/// through all of them unexamined.
const KNOWN_VENDORS: [&str; 4] = ["anthropic", "openai", "ollama", "opencode"];

/// Anthropic prices all five streams, so a model missing one is a hole in the
/// book rather than a fact about the vendor.
const ANTHROPIC_CLASSES: [TokenClass; 5] = [
    TokenClass::Input,
    TokenClass::Output,
    TokenClass::CacheRead,
    TokenClass::CacheWrite5m,
    TokenClass::CacheWrite1h,
];

/// What one rate is keyed by: vendor, model and token class. One rate per key
/// may be effective at a time, which is the property the overlap check owns.
type CardKey = (String, String, &'static str);

/// A half-open effective interval: the first day the rate applies, and the day
/// it stops applying (`None` is open ended).
type Interval = (UtcDate, Option<UtcDate>);

fn shipped() -> RateBook {
    rate_book::parse(SHIPPED).expect("the shipped rate book must parse")
}

fn key(card: &RateCardDraft) -> CardKey {
    (
        card.vendor.clone(),
        card.model.clone(),
        card.token_class.as_str(),
    )
}

/// Two intervals overlap when each starts before the other ends. `effective_end`
/// is the day the rate stops applying, so an interval is half-open and a row
/// starting on another's end day hands off rather than overlapping.
fn overlaps(a: Interval, b: Interval) -> bool {
    let a_before_b_end = b.1.is_none_or(|end| a.0 < end);
    let b_before_a_end = a.1.is_none_or(|end| b.0 < end);
    a_before_b_end && b_before_a_end
}

fn overlapping_intervals(book: &RateBook) -> Vec<String> {
    let mut by_key: BTreeMap<CardKey, Vec<Interval>> = BTreeMap::new();
    for card in &book.cards {
        by_key
            .entry(key(card))
            .or_default()
            .push((card.effective_start, card.effective_end));
    }
    let mut found = Vec::new();
    for ((vendor, model, class), intervals) in by_key {
        for (index, first) in intervals.iter().enumerate() {
            for second in &intervals[index + 1..] {
                if overlaps(*first, *second) {
                    found.push(format!(
                        "{vendor} {model} {class}: {} and {} overlap",
                        first.0.iso(),
                        second.0.iso()
                    ));
                }
            }
        }
    }
    found
}

fn unknown_vendors(book: &RateBook) -> Vec<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for card in &book.cards {
        if !KNOWN_VENDORS.contains(&card.vendor.as_str()) {
            found.insert(format!(
                "vendor {:?} is not one of {KNOWN_VENDORS:?}",
                card.vendor
            ));
        }
    }
    found.into_iter().collect()
}

fn missing_classes(book: &RateBook) -> Vec<String> {
    let mut by_model: BTreeMap<(String, String), BTreeSet<&'static str>> = BTreeMap::new();
    for card in &book.cards {
        by_model
            .entry((card.vendor.clone(), card.model.clone()))
            .or_default()
            .insert(card.token_class.as_str());
    }
    let mut found = Vec::new();
    for ((vendor, model), classes) in by_model {
        let required: Vec<&'static str> = if vendor == "anthropic" {
            ANTHROPIC_CLASSES.iter().map(|c| c.as_str()).collect()
        } else {
            vec![TokenClass::Input.as_str(), TokenClass::Output.as_str()]
        };
        for class in required {
            if !classes.contains(class) {
                found.push(format!("{vendor} {model} has no {class} row"));
            }
        }
    }
    found
}

/// Anthropic's cache-write prices are stated on the page as multiples of the
/// input rate, so a row that drifts from the multiple is a transcription error
/// and not a price. Exact integer arithmetic, because the whole point of the
/// micros representation is that no rate is checked through a float.
fn cache_write_multiples(book: &RateBook) -> Vec<String> {
    let mut inputs: BTreeMap<String, i64> = BTreeMap::new();
    for card in &book.cards {
        if card.vendor == "anthropic" && card.token_class == TokenClass::Input {
            inputs.insert(card.model.clone(), card.rate_micros);
        }
    }
    let mut found = Vec::new();
    for card in &book.cards {
        if card.vendor != "anthropic" {
            continue;
        }
        let (numerator, denominator) = match card.token_class {
            TokenClass::CacheWrite5m => (125i128, 100i128),
            TokenClass::CacheWrite1h => (2i128, 1i128),
            _ => continue,
        };
        let Some(input) = inputs.get(&card.model) else {
            found.push(format!("{} has no input row to check against", card.model));
            continue;
        };
        let expected = i128::from(*input) * numerator;
        if i128::from(card.rate_micros) * denominator != expected {
            found.push(format!(
                "{} {}: {} micros is not {numerator}/{denominator} of input {input}",
                card.model,
                card.token_class.as_str(),
                card.rate_micros
            ));
        }
    }
    found
}

#[test]
fn the_shipped_book_prices_one_rate_per_vendor_model_and_class_at_a_time() {
    assert_eq!(overlapping_intervals(&shipped()), Vec::<String>::new());
}

#[test]
fn the_shipped_book_carries_only_vendors_these_checks_understand() {
    assert_eq!(unknown_vendors(&shipped()), Vec::<String>::new());
}

#[test]
fn the_shipped_book_prices_every_stream_each_vendor_publishes() {
    assert_eq!(missing_classes(&shipped()), Vec::<String>::new());
}

#[test]
fn the_shipped_book_keeps_anthropic_cache_writes_on_their_multiples() {
    assert_eq!(cache_write_multiples(&shipped()), Vec::<String>::new());
}

/// The shipped book is what the operator imports, so the ids the ledger stores
/// are the ones that must be present. Each id below is one of the defects that
/// made the fixture unusable as a real book (`aub-b8ng`): models the vendor had
/// released and the fixture never gained, one spelled without the date suffix
/// the ledger stores, and one OpenAI row carrying an untiered name no rollout
/// on this machine uses.
#[test]
fn the_shipped_book_covers_the_ids_the_fixture_missed() {
    let book = shipped();
    let models: BTreeSet<&str> = book.cards.iter().map(|card| card.model.as_str()).collect();
    for model in [
        "claude-fable-5-1",
        "claude-sonnet-4-6",
        "claude-opus-4-5-20251101",
        "claude-haiku-4-5-20251001",
        "gpt-5.6-terra",
        "gpt-6-astra",
    ] {
        assert!(models.contains(model), "{model} is missing from the book");
    }
    assert!(
        !models.contains("gpt-5.6"),
        "the untiered OpenAI id names no rollout and must not be a row"
    );
}

// --------------------------------------------------------------------------
// The planted negatives. Each book below is the same three cards, differing
// from a passing one in exactly the dimension its check owns.
// --------------------------------------------------------------------------

fn card(
    vendor: &str,
    model: &str,
    class: &str,
    rate: &str,
    start: &str,
    end: Option<&str>,
) -> String {
    let end = match end {
        None => String::new(),
        Some(day) => format!("effective_end = \"{day}\"\n"),
    };
    format!(
        "[[card]]\nvendor = \"{vendor}\"\nmodel = \"{model}\"\ntoken_class = \"{class}\"\n\
         rate = \"{rate}\"\ncurrency = \"USD\"\nbilling_basis = \"per_million_tokens\"\n\
         effective_start = \"{start}\"\n{end}published_at = \"2026-09-07\"\n\
         source = \"test\"\n\n"
    )
}

fn parse_cards(text: &str) -> RateBook {
    rate_book::parse(text).expect("the constructed book must parse")
}

#[test]
fn two_open_ended_rows_for_one_class_are_reported_as_overlapping() {
    let handing_off = format!(
        "{}{}",
        card(
            "openai",
            "gpt-5.6-terra",
            "input",
            "2.00",
            "2026-06-24",
            Some("2026-09-07")
        ),
        card(
            "openai",
            "gpt-5.6-terra",
            "input",
            "2.50",
            "2026-09-07",
            None
        ),
    );
    assert_eq!(
        overlapping_intervals(&parse_cards(&handing_off)),
        Vec::<String>::new()
    );

    // The same two rows with the earlier one left open ended: nothing else
    // changes, and now both are effective on the same day.
    let overlapping = format!(
        "{}{}",
        card(
            "openai",
            "gpt-5.6-terra",
            "input",
            "2.00",
            "2026-06-24",
            None
        ),
        card(
            "openai",
            "gpt-5.6-terra",
            "input",
            "2.50",
            "2026-09-07",
            None
        ),
    );
    assert_eq!(overlapping_intervals(&parse_cards(&overlapping)).len(), 1);
}

#[test]
fn an_anthropic_model_missing_one_of_the_five_classes_is_reported() {
    let mut complete = String::new();
    for (class, rate) in [
        ("input", "1.00"),
        ("output", "5.00"),
        ("cache_read", "0.10"),
        ("cache_write_5m", "1.25"),
        ("cache_write_1h", "2.00"),
    ] {
        complete.push_str(&card(
            "anthropic",
            "claude-haiku-4-5-20251001",
            class,
            rate,
            "2026-06-24",
            None,
        ));
    }
    assert_eq!(
        missing_classes(&parse_cards(&complete)),
        Vec::<String>::new()
    );

    let without_cache_read = complete.replace(
        &card(
            "anthropic",
            "claude-haiku-4-5-20251001",
            "cache_read",
            "0.10",
            "2026-06-24",
            None,
        ),
        "",
    );
    assert_eq!(missing_classes(&parse_cards(&without_cache_read)).len(), 1);
}

#[test]
fn a_non_anthropic_model_missing_output_is_reported() {
    let complete = format!(
        "{}{}",
        card(
            "ollama",
            "deepseek-v4-pro",
            "input",
            "0.66",
            "2026-09-07",
            None
        ),
        card(
            "ollama",
            "deepseek-v4-pro",
            "output",
            "1.98",
            "2026-09-07",
            None
        ),
    );
    assert_eq!(
        missing_classes(&parse_cards(&complete)),
        Vec::<String>::new()
    );

    let input_only = card(
        "ollama",
        "deepseek-v4-pro",
        "input",
        "0.66",
        "2026-09-07",
        None,
    );
    assert_eq!(missing_classes(&parse_cards(&input_only)).len(), 1);
}

#[test]
fn an_anthropic_cache_write_off_its_multiple_is_reported() {
    let correct = format!(
        "{}{}{}",
        card(
            "anthropic",
            "claude-opus-5",
            "input",
            "5.00",
            "2026-06-24",
            None
        ),
        card(
            "anthropic",
            "claude-opus-5",
            "cache_write_5m",
            "6.25",
            "2026-06-24",
            None
        ),
        card(
            "anthropic",
            "claude-opus-5",
            "cache_write_1h",
            "10.00",
            "2026-06-24",
            None
        ),
    );
    assert_eq!(
        cache_write_multiples(&parse_cards(&correct)),
        Vec::<String>::new()
    );

    // 6.24 instead of 6.25: one micro-digit out, which is exactly the shape a
    // transcription error takes and exactly what a float comparison would miss.
    let one_digit_out = correct.replace("\"6.25\"", "\"6.24\"");
    assert_eq!(cache_write_multiples(&parse_cards(&one_digit_out)).len(), 1);
}

#[test]
fn a_vendor_no_check_understands_is_reported() {
    let known = card(
        "openai",
        "gpt-6-astra",
        "input",
        "10.00",
        "2026-09-07",
        None,
    );
    assert_eq!(unknown_vendors(&parse_cards(&known)), Vec::<String>::new());

    let unknown = card(
        "openai-beta",
        "gpt-6-astra",
        "input",
        "10.00",
        "2026-09-07",
        None,
    );
    assert_eq!(unknown_vendors(&parse_cards(&unknown)).len(), 1);
}
