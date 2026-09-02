//! The rate book file: the import source `aub rate-card import` reads (PLAN.md
//! section 25.3).
//!
//! A rate file on disk is an import source, never a runtime witness: valuation
//! reads the immutable versioned records in the database, and this module only
//! turns the file's text into drafts the store can persist. Every date comment
//! in the pre-existing hardcoded price table becomes structured metadata here:
//! an effective interval and a publication reference, so a figure derived from
//! a rate can always name the rate it used and when that rate was true.
//!
//! Unknown keys are refused, not ignored: a rate book that grew a field this
//! parser does not know must fail loudly at import rather than silently drop
//! the field's meaning.

use crate::domain::rate_card::{
    BillingBasis, CurrencyCode, RateCardDraft, RateCardParseError, ReviewDuePolicy, TokenClass,
    parse_rate_micros,
};
use crate::domain::time::{UtcDate, UtcTimestamp};

/// The keys a card entry may carry. Anything else is refused, so a field the
/// importer silently drops is impossible by construction.
const CARD_KEYS: [&str; 10] = [
    "vendor",
    "model",
    "token_class",
    "rate",
    "currency",
    "billing_basis",
    "effective_start",
    "effective_end",
    "published_at",
    "source",
];

/// Why a rate book could not be parsed. The card index (0-based, in file
/// order) names where, so the operator fixes one entry per message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateBookError {
    pub card_index: usize,
    pub reason: String,
}

impl std::fmt::Display for RateBookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "card {}: {}", self.card_index, self.reason)
    }
}

/// A parsed rate book: the drafts in file order, ready for the store's
/// idempotent insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateBook {
    pub cards: Vec<RateCardDraft>,
}

/// Parses rate book TOML text into dated drafts.
///
/// The file shape is one `[[card]]` table per rate component:
///
/// ```toml
/// [[card]]
/// vendor = "anthropic"
/// model = "claude-fable-5"
/// token_class = "input"
/// rate = "10.00"
/// currency = "USD"
/// billing_basis = "per_million_tokens"
/// effective_start = "2026-06-24"
/// source = "claude-api reference"
/// ```
///
/// `rate` is a decimal string parsed exactly (`parse_rate_micros`); dates are
/// `YYYY-MM-DD`; `published_at` is an RFC 3339 instant or a bare date (midnight
/// UTC); `review_due` is an optional date. Missing provenance stays missing:
/// the draft records the absence rather than filling it in.
pub fn parse(text: &str) -> Result<RateBook, RateBookError> {
    let table: toml::Table = text.parse().map_err(|error| RateBookError {
        card_index: 0,
        reason: format!("file is not valid TOML: {error}"),
    })?;
    let cards = table
        .get("card")
        .ok_or_else(|| RateBookError {
            card_index: 0,
            reason: "no [[card]] entries".to_string(),
        })?
        .as_array()
        .ok_or_else(|| RateBookError {
            card_index: 0,
            reason: "[[card]] must be an array of tables".to_string(),
        })?;
    if cards.is_empty() {
        return Err(RateBookError {
            card_index: 0,
            reason: "no [[card]] entries".to_string(),
        });
    }
    let mut parsed = Vec::with_capacity(cards.len());
    for (index, entry) in cards.iter().enumerate() {
        let card = entry.as_table().ok_or_else(|| RateBookError {
            card_index: index,
            reason: "entry must be a table".to_string(),
        })?;
        for key in card.keys() {
            if !CARD_KEYS.contains(&key.as_str()) {
                return Err(RateBookError {
                    card_index: index,
                    reason: format!("unknown key {key:?}; known keys are {CARD_KEYS:?}"),
                });
            }
        }
        parsed.push(parse_card(index, card)?);
    }
    Ok(RateBook { cards: parsed })
}

fn required<'a>(index: usize, card: &'a toml::Table, key: &str) -> Result<&'a str, RateBookError> {
    card.get(key)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| RateBookError {
            card_index: index,
            reason: format!("missing required string key {key:?}"),
        })
}

fn optional_string(card: &toml::Table, key: &str) -> Option<String> {
    card.get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

fn optional_date(
    card: &toml::Table,
    index: usize,
    key: &str,
) -> Result<Option<UtcDate>, RateBookError> {
    match optional_string(card, key) {
        None => Ok(None),
        Some(text) => UtcDate::parse(&text)
            .map(Some)
            .ok_or_else(|| RateBookError {
                card_index: index,
                reason: format!("{key} {text:?} is not a YYYY-MM-DD date"),
            }),
    }
}

fn parse_card(index: usize, card: &toml::Table) -> Result<RateCardDraft, RateBookError> {
    let vendor = required(index, card, "vendor")?.to_string();
    let model = required(index, card, "model")?.to_string();
    let token_class = TokenClass::parse(required(index, card, "token_class")?).ok_or_else(|| {
        RateBookError {
            card_index: index,
            reason: format!(
                "token_class {:?} is not one of input | output | cache_read | cache_write_5m | cache_write_1h",
                required(index, card, "token_class").unwrap_or("")
            ),
        }
    })?;
    let rate_text = required(index, card, "rate")?;
    let rate_micros = parse_rate_micros(rate_text).map_err(|error| {
        let reason = reason_for(error.clone());
        RateBookError {
            card_index: index,
            reason: format!("rate {error:?}: {reason}"),
        }
    })?;
    let currency =
        CurrencyCode::parse(required(index, card, "currency")?).ok_or_else(|| RateBookError {
            card_index: index,
            reason: format!(
                "currency {:?} is not a supported ISO 4217 code",
                required(index, card, "currency").unwrap_or("")
            ),
        })?;
    let billing_basis =
        BillingBasis::parse(required(index, card, "billing_basis")?).ok_or_else(|| {
            RateBookError {
                card_index: index,
                reason: format!(
                    "billing_basis {:?} is not supported",
                    required(index, card, "billing_basis").unwrap_or("")
                ),
            }
        })?;
    let effective_start_text = required(index, card, "effective_start")?;
    let effective_start = UtcDate::parse(effective_start_text).ok_or_else(|| RateBookError {
        card_index: index,
        reason: format!("effective_start {effective_start_text:?} is not a YYYY-MM-DD date"),
    })?;
    let effective_end = optional_date(card, index, "effective_end")?;
    let review_due = optional_date(card, index, "review_due")?;
    let published_at = match optional_string(card, "published_at") {
        None => None,
        Some(text) => Some(parse_published_at(index, &text)?),
    };
    let source = optional_string(card, "source");
    Ok(RateCardDraft {
        vendor,
        model,
        token_class,
        rate_micros,
        currency,
        billing_basis,
        effective_start,
        effective_end,
        publication: crate::domain::rate_card::Publication {
            source,
            published_at,
        },
        review_due: match review_due {
            None => ReviewDuePolicy::None,
            Some(date) => ReviewDuePolicy::On(date),
        },
    })
}

fn parse_published_at(index: usize, text: &str) -> Result<UtcTimestamp, RateBookError> {
    if let Some(timestamp) = UtcTimestamp::parse_rfc3339(text) {
        return Ok(timestamp);
    }
    // A bare publication date is accepted and anchored at midnight UTC: the
    // price table this importer replaces recorded dates, not instants, and a
    // date is honest metadata where an invented time of day would not be.
    let date = UtcDate::parse(text).ok_or_else(|| RateBookError {
        card_index: index,
        reason: format!("published_at {text:?} is neither RFC 3339 nor a YYYY-MM-DD date"),
    })?;
    Ok(date.start())
}

fn reason_for(error: RateCardParseError) -> String {
    match error {
        RateCardParseError::RateNotANumber(text) => format!("{text:?} is not a decimal number"),
        RateCardParseError::RateTooFine(text) => {
            format!("{text:?} is finer than one micro and cannot be stored exactly")
        }
        RateCardParseError::RateOutOfRange(text) => format!("{text:?} overflows the micros range"),
        RateCardParseError::NegativeRate(text) => {
            format!("{text:?} is negative; rates are non-negative")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> RateBook {
        parse(text).expect("rate book must parse")
    }

    const MINIMAL_CARD: &str = r#"
[[card]]
vendor = "anthropic"
model = "claude-fable-5"
token_class = "input"
rate = "10.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
"#;

    #[test]
    fn a_minimal_card_parses_with_an_open_interval_and_missing_provenance() {
        let book = parse_ok(MINIMAL_CARD);
        assert_eq!(book.cards.len(), 1);
        let card = &book.cards[0];
        assert_eq!(card.vendor, "anthropic");
        assert_eq!(card.rate_micros, 10_000_000);
        assert_eq!(card.effective_end, None);
        assert!(!card.publication.fully_sourced());
        assert_eq!(card.review_due, ReviewDuePolicy::None);
    }

    /// The integration point the design names: the date comments the existing
    /// hardcoded price table carries become structured metadata.
    #[test]
    fn date_comments_become_structured_metadata() {
        let book = parse_ok(
            r#"
# Anthropic rows read 2026-06-24 from the claude-api reference.
[[card]]
vendor = "anthropic"
model = "claude-sonnet-5"
token_class = "input"
rate = "3.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "claude-api reference"

# Introductory pricing that expires; review on the expiry date.
[[card]]
vendor = "anthropic"
model = "claude-sonnet-5"
token_class = "input"
rate = "2.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
effective_end = "2026-08-31"
published_at = "2026-06-24"
source = "claude-api reference"
review_due = "2026-08-31"
"#,
        );
        assert_eq!(book.cards.len(), 2);
        let standard = &book.cards[0];
        assert_eq!(standard.effective_start.iso(), "2026-06-24");
        assert_eq!(standard.effective_end, None);
        assert!(standard.publication.fully_sourced());
        assert_eq!(
            standard.publication.source.as_deref(),
            Some("claude-api reference")
        );
        let intro = &book.cards[1];
        assert_eq!(
            intro.effective_end.map(UtcDate::iso),
            Some("2026-08-31".to_string())
        );
        assert_eq!(
            intro.review_due,
            ReviewDuePolicy::On(UtcDate::parse("2026-08-31").unwrap())
        );
        assert_eq!(intro.rate_micros, 2_000_000);
    }

    #[test]
    fn an_unknown_key_is_refused_naming_the_key() {
        let error = parse(MINIMAL_CARD.replace(
            "effective_start = \"2026-06-24\"",
            "effective_start = \"2026-06-24\"\nintro = \"maybe\"",
        ))
        .expect_err("unknown key must be refused");
        assert_eq!(error.card_index, 0);
        assert!(
            error.reason.contains("intro"),
            "reason must name the key: {}",
            error.reason
        );
    }

    #[test]
    fn an_unknown_token_class_is_refused_naming_the_card() {
        let error =
            parse(MINIMAL_CARD.replace("token_class = \"input\"", "token_class = \"cached\""))
                .expect_err("unknown class must be refused");
        assert_eq!(error.card_index, 0);
        assert!(error.reason.contains("token_class"));
    }

    #[test]
    fn a_second_card_defect_names_the_second_index() {
        let error = parse("text-that-is-not-a-table");
        assert!(error.is_err());
        let error = parse(
            r#"
[[card]]
vendor = "anthropic"
model = "a"
token_class = "input"
rate = "1.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-01-01"

[[card]]
vendor = "anthropic"
model = "b"
token_class = "input"
rate = "1.00.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-01-01"
"#,
        )
        .expect_err("bad rate must be refused");
        assert_eq!(error.card_index, 1, "the defect is in the second card");
        assert!(error.reason.contains("1.00.00"));
    }

    #[test]
    fn an_empty_book_is_refused_rather_than_importing_nothing() {
        let error = parse("").expect_err("empty book must be refused");
        assert!(error.reason.contains("no [[card]] entries"));
    }

    #[test]
    fn a_bare_publication_date_anchors_at_midnight_utc() {
        let book = parse_ok(MINIMAL_CARD.replace(
            "effective_start = \"2026-06-24\"",
            "effective_start = \"2026-06-24\"\npublished_at = \"2026-06-24\"",
        ));
        let published = book.cards[0]
            .publication
            .published_at
            .expect("date must parse");
        assert_eq!(
            published.unix_nanos(),
            UtcDate::parse("2026-06-24").unwrap().start().unix_nanos()
        );
    }

    #[test]
    fn missing_required_key_is_refused_by_name() {
        let error = parse(MINIMAL_CARD.replace("vendor = \"anthropic\"", "vendor_missing = true"))
            .expect_err("missing vendor must be refused");
        assert!(error.reason.contains("vendor"), "{}", error.reason);
    }
}
