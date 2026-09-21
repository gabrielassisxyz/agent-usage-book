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
    BillingBasis, CardQuality, CurrencyCode, HoursParseError, QuotaWindowKind, RateCardDraft,
    RateCardParseError, RateDenomination, RateUnit, ReviewDuePolicy, Schedule, TokenClass,
    WindowEstimate, parse_day_name, parse_hours_utc, parse_rate_micros,
};
use crate::domain::time::{UtcDate, UtcTimestamp};

/// The keys a card entry may carry. Anything else is refused, so a field the
/// importer silently drops is impossible by construction.
const CARD_KEYS: [&str; 15] = [
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
    "review_due",
    "schedule",
    "window",
    "unit",
    "quality",
];

/// The keys a card's `schedule` table may carry.
const SCHEDULE_KEYS: [&str; 2] = ["days", "hours_utc"];

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
    check_consistency(&parsed)?;
    Ok(RateBook { cards: parsed })
}

/// Refuses a book in which two cards could price the same instant: for every
/// (vendor, model, class) and every date, at most one unscheduled card, and no
/// two scheduled cards whose day sets intersect and whose hour ranges overlap.
/// A default beside its peak rows is the intended shape and always passes: a
/// scheduled card never conflicts with an unscheduled one.
fn check_consistency(cards: &[RateCardDraft]) -> Result<(), RateBookError> {
    for (later, card) in cards.iter().enumerate() {
        for (earlier, other) in cards[..later].iter().enumerate() {
            if card.vendor != other.vendor
                || card.model != other.model
                || card.token_class != other.token_class
            {
                continue;
            }
            // Two cards of different bases price different dimensions and are
            // never both in force over the same figure: a money card values
            // tokens at a price, a percent-of-window card states window
            // movement. Two percent-of-window cards for different windows are
            // the same case.
            if card.billing_basis != other.billing_basis
                || card.window_estimate.map(|estimate| estimate.window)
                    != other.window_estimate.map(|estimate| estimate.window)
            {
                continue;
            }
            if !effective_overlap(
                card.effective_start,
                card.effective_end,
                other.effective_start,
                other.effective_end,
            ) {
                continue;
            }
            let conflict = match (&card.schedule, &other.schedule) {
                (None, None) => true,
                (Some(first), Some(second)) => first.overlaps(second),
                (None, Some(_)) | (Some(_), None) => false,
            };
            if conflict {
                return Err(RateBookError {
                    card_index: later,
                    reason: format!(
                        "overlaps card {earlier} for {}/{}/{}: both cards are in force on the same date",
                        card.vendor,
                        card.model,
                        card.token_class.as_str(),
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Two effective intervals overlap when each starts before the other ends.
/// `effective_end` is exclusive, so a row starting on another's end day hands
/// off rather than overlapping.
fn effective_overlap(
    first_start: UtcDate,
    first_end: Option<UtcDate>,
    second_start: UtcDate,
    second_end: Option<UtcDate>,
) -> bool {
    let first_before_second_end = second_end.is_none_or(|end| first_start < end);
    let second_before_first_end = first_end.is_none_or(|end| second_start < end);
    first_before_second_end && second_before_first_end
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
    let (denomination, window_estimate) = parse_denomination(index, card, billing_basis)?;
    let effective_start_text = required(index, card, "effective_start")?;
    let effective_start = UtcDate::parse(effective_start_text).ok_or_else(|| RateBookError {
        card_index: index,
        reason: format!("effective_start {effective_start_text:?} is not a YYYY-MM-DD date"),
    })?;
    let effective_end = optional_date(card, index, "effective_end")?;
    let review_due = optional_date(card, index, "review_due")?;
    let schedule = match card.get("schedule") {
        None => None,
        Some(value) => Some(parse_schedule(index, value)?),
    };
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
        denomination,
        billing_basis,
        window_estimate,
        effective_start,
        effective_end,
        schedule,
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

/// Reads the keys that state what a rate is counted in, refusing every
/// combination the two bases do not have (`aub-8vpc`).
///
/// The two bases are mutually exclusive about these keys, and the refusals are
/// deliberately not symmetric prose: a money card that grew a `window` was
/// written against the wrong basis, and a percent-of-window card that names a
/// `currency` is claiming a price for something that is not one. Each message
/// names the card index (through [`RateBookError`]) and the offending field, so
/// an operator fixes one key per message.
fn parse_denomination(
    index: usize,
    card: &toml::Table,
    billing_basis: BillingBasis,
) -> Result<(RateDenomination, Option<WindowEstimate>), RateBookError> {
    let refuse = |field: &str, reason: &str| RateBookError {
        card_index: index,
        reason: format!("{field} {reason}"),
    };
    match billing_basis {
        BillingBasis::PerMillionTokens => {
            for forbidden in ["window", "unit", "quality"] {
                if card.contains_key(forbidden) {
                    return Err(refuse(
                        forbidden,
                        "is only accepted on a percent_of_window_per_million_tokens card",
                    ));
                }
            }
            let text = required(index, card, "currency")?;
            let currency = CurrencyCode::parse(text).ok_or_else(|| RateBookError {
                card_index: index,
                reason: format!("currency {text:?} is not a supported ISO 4217 code"),
            })?;
            Ok((RateDenomination::Money(currency), None))
        }
        BillingBasis::PercentOfWindowPerMillionTokens => {
            if card.contains_key("schedule") {
                // Not in this bead's card syntax, and refused rather than
                // ignored: a time-of-day window on an approximation of a quota
                // window is a second window with no consumer, and the valuation
                // path for this basis never consults one.
                return Err(refuse(
                    "schedule",
                    "is not accepted on a percent_of_window_per_million_tokens card",
                ));
            }
            if card.contains_key("currency") {
                return Err(refuse(
                    "currency",
                    "is refused on a percent_of_window_per_million_tokens card: the rate is \
                     percentage points of a quota window, not a price",
                ));
            }
            let window_text = required(index, card, "window").map_err(|_| {
                refuse(
                    "window",
                    "is required on a percent_of_window_per_million_tokens card and must be \
                     five_hour or seven_day",
                )
            })?;
            let window = QuotaWindowKind::parse(window_text).ok_or_else(|| RateBookError {
                card_index: index,
                reason: format!("window {window_text:?} is not one of five_hour | seven_day"),
            })?;
            let unit_text = required(index, card, "unit").map_err(|_| {
                refuse(
                    "unit",
                    "is required on a percent_of_window_per_million_tokens card and must be \
                     percentage_points",
                )
            })?;
            let unit = RateUnit::parse(unit_text).ok_or_else(|| RateBookError {
                card_index: index,
                reason: format!("unit {unit_text:?} is not percentage_points"),
            })?;
            let quality_text = required(index, card, "quality").map_err(|_| {
                refuse(
                    "quality",
                    "is required on a percent_of_window_per_million_tokens card and must be \
                     estimate: this basis admits approximations only",
                )
            })?;
            let quality = CardQuality::parse(quality_text).ok_or_else(|| RateBookError {
                card_index: index,
                reason: format!(
                    "quality {quality_text:?} is not estimate; a percent_of_window_per_million_tokens \
                     card is a declared estimate and a measured figure for the same quantity is a \
                     fitted calibration"
                ),
            })?;
            if optional_string(card, "source").is_none() {
                return Err(refuse(
                    "source",
                    "is required on a percent_of_window_per_million_tokens card: an estimate \
                     whose origin is not recorded becomes the number",
                ));
            }
            Ok((
                RateDenomination::Points(unit),
                Some(WindowEstimate {
                    window,
                    unit,
                    quality,
                }),
            ))
        }
    }
}

/// Parses an optional `schedule = { days = [...], hours_utc = "HH:MM-HH:MM" }`
/// table. Every refusal names the card index and the schedule field, so the
/// operator fixes one entry per message.
fn parse_schedule(index: usize, value: &toml::Value) -> Result<Schedule, RateBookError> {
    let table = value.as_table().ok_or_else(|| RateBookError {
        card_index: index,
        reason: "schedule must be a table with days and hours_utc".to_string(),
    })?;
    for key in table.keys() {
        if !SCHEDULE_KEYS.contains(&key.as_str()) {
            return Err(RateBookError {
                card_index: index,
                reason: format!("unknown schedule key {key:?}; known keys are {SCHEDULE_KEYS:?}"),
            });
        }
    }
    let days_value = table.get("days").ok_or_else(|| RateBookError {
        card_index: index,
        reason: "schedule is missing days".to_string(),
    })?;
    let days_array = days_value.as_array().ok_or_else(|| RateBookError {
        card_index: index,
        reason: "schedule.days must be an array of day names".to_string(),
    })?;
    if days_array.is_empty() {
        return Err(RateBookError {
            card_index: index,
            reason: "schedule.days must name at least one day".to_string(),
        });
    }
    let mut days = Vec::with_capacity(days_array.len());
    for day in days_array {
        let name = day.as_str().ok_or_else(|| RateBookError {
            card_index: index,
            reason: "schedule.days must be an array of day names".to_string(),
        })?;
        days.push(parse_day_name(name).ok_or_else(|| RateBookError {
            card_index: index,
            reason: format!(
                "schedule.days {name:?} is not one of mon | tue | wed | thu | fri | sat | sun"
            ),
        })?);
    }
    let hours_text = table
        .get("hours_utc")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| RateBookError {
            card_index: index,
            reason: "schedule is missing hours_utc".to_string(),
        })?;
    let (start, end) = parse_hours_utc(hours_text).map_err(|error| {
        let reason = match error {
            HoursParseError::Malformed(_) => {
                format!("schedule.hours_utc {hours_text:?} is not HH:MM-HH:MM")
            }
            HoursParseError::StartNotBeforeEnd(_) => {
                format!("schedule.hours_utc {hours_text:?} starts at or after it ends")
            }
            HoursParseError::CrossesMidnight(_) => {
                format!("schedule.hours_utc {hours_text:?} crosses midnight; write it as two cards")
            }
        };
        RateBookError {
            card_index: index,
            reason,
        }
    })?;
    Schedule::new(&days, start, end).ok_or_else(|| RateBookError {
        card_index: index,
        reason: "schedule.days and schedule.hours_utc do not form a valid window".to_string(),
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
effective_start = "2026-08-31"
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
        assert_eq!(standard.effective_start.iso(), "2026-08-31");
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
        let error = parse(&MINIMAL_CARD.replace(
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
            parse(&MINIMAL_CARD.replace("token_class = \"input\"", "token_class = \"cached\""))
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
        let book = parse_ok(&MINIMAL_CARD.replace(
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
        let error = parse(&MINIMAL_CARD.replace("vendor = \"anthropic\"", "vendor_missing = true"))
            .expect_err("missing vendor must be refused");
        assert!(error.reason.contains("vendor"), "{}", error.reason);
    }

    const SCHEDULED_CARD: &str = r#"
[[card]]
vendor = "ollama"
model = "deepseek-v4-flash"
token_class = "input"
rate = "0.44"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-09-07"
schedule = { days = ["mon", "tue", "wed", "thu", "fri"], hours_utc = "12:00-18:00" }
"#;

    fn unscheduled_card(model: &str, rate: &str, end: Option<&str>) -> String {
        let end = match end {
            None => String::new(),
            Some(day) => format!("effective_end = \"{day}\"\n"),
        };
        format!(
            "[[card]]\nvendor = \"ollama\"\nmodel = \"{model}\"\ntoken_class = \"input\"\n\
             rate = \"{rate}\"\ncurrency = \"USD\"\nbilling_basis = \"per_million_tokens\"\n\
             effective_start = \"2026-09-07\"\n{end}\n"
        )
    }

    #[test]
    fn a_scheduled_card_parses_its_window() {
        let book = parse_ok(&format!("{MINIMAL_CARD}{SCHEDULED_CARD}"));
        assert_eq!(book.cards.len(), 2);
        let scheduled = &book.cards[1];
        let window = scheduled.schedule.expect("schedule must parse");
        assert_eq!(window.days_iso(), vec![1, 2, 3, 4, 5]);
        assert_eq!(window.describe(), "peak mon-fri 12:00-18:00 UTC");
        assert!(book.cards[0].schedule.is_none());
    }

    #[test]
    fn schedule_refusals_name_the_card_index_and_the_field() {
        let unknown_day = SCHEDULED_CARD.replace("\"tue\"", "\"funday\"");
        let error = parse(&format!("{MINIMAL_CARD}{unknown_day}"))
            .expect_err("unknown day must be refused");
        assert_eq!(error.card_index, 1);
        assert!(error.reason.contains("schedule.days"), "{}", error.reason);
        assert!(error.reason.contains("funday"), "{}", error.reason);

        let empty_days = SCHEDULED_CARD.replace(
            "days = [\"mon\", \"tue\", \"wed\", \"thu\", \"fri\"]",
            "days = []",
        );
        let error =
            parse(&format!("{MINIMAL_CARD}{empty_days}")).expect_err("empty days must be refused");
        assert_eq!(error.card_index, 1);
        assert!(error.reason.contains("schedule.days"), "{}", error.reason);

        let zero_window = SCHEDULED_CARD.replace("12:00-18:00", "12:00-12:00");
        let error = parse(&format!("{MINIMAL_CARD}{zero_window}"))
            .expect_err("start not before end must be refused");
        assert_eq!(error.card_index, 1);
        assert!(
            error.reason.contains("schedule.hours_utc"),
            "{}",
            error.reason
        );

        let overnight = SCHEDULED_CARD.replace("12:00-18:00", "22:00-02:00");
        let error = parse(&format!("{MINIMAL_CARD}{overnight}"))
            .expect_err("a window crossing midnight must be refused");
        assert_eq!(error.card_index, 1);
        assert!(
            error.reason.contains("schedule.hours_utc"),
            "{}",
            error.reason
        );
        assert!(error.reason.contains("two cards"), "{}", error.reason);

        let malformed = SCHEDULED_CARD.replace("12:00-18:00", "noon");
        let error = parse(&format!("{MINIMAL_CARD}{malformed}"))
            .expect_err("malformed hours must be refused");
        assert_eq!(error.card_index, 1);
        assert!(
            error.reason.contains("schedule.hours_utc"),
            "{}",
            error.reason
        );
    }

    #[test]
    fn a_default_beside_its_peak_rows_is_consistent() {
        let book = format!(
            "{}{}{}",
            unscheduled_card("deepseek-v4-flash", "0.22", None),
            SCHEDULED_CARD,
            SCHEDULED_CARD.replace("deepseek-v4-flash", "deepseek-v4-pro"),
        );
        assert_eq!(
            parse(&book)
                .expect("default beside peaks must pass")
                .cards
                .len(),
            3
        );
    }

    #[test]
    fn two_unscheduled_open_ended_rows_for_one_triple_are_refused() {
        let book = format!(
            "{}{}",
            unscheduled_card("deepseek-v4-flash", "0.22", None),
            unscheduled_card("deepseek-v4-flash", "0.30", None),
        );
        let error = parse(&book).expect_err("two open-ended defaults must be refused");
        assert_eq!(error.card_index, 1);
        assert!(error.reason.contains("card 0"), "{}", error.reason);
    }

    #[test]
    fn two_scheduled_rows_overlapping_on_a_shared_day_are_refused() {
        let second = SCHEDULED_CARD.replace("12:00-18:00", "17:00-20:00");
        let book = format!(
            "{}{}{}",
            unscheduled_card("deepseek-v4-flash", "0.22", None),
            SCHEDULED_CARD,
            second,
        );
        let error = parse(&book).expect_err("overlapping peaks must be refused");
        assert_eq!(error.card_index, 2);
        assert!(error.reason.contains("card 1"), "{}", error.reason);
    }

    #[test]
    fn two_scheduled_rows_sharing_hours_on_disjoint_days_are_accepted() {
        let weekend = SCHEDULED_CARD.replace(
            "days = [\"mon\", \"tue\", \"wed\", \"thu\", \"fri\"]",
            "days = [\"sat\", \"sun\"]",
        );
        let book = format!(
            "{}{}{}",
            unscheduled_card("deepseek-v4-flash", "0.22", None),
            SCHEDULED_CARD,
            weekend,
        );
        assert_eq!(
            parse(&book).expect("disjoint days must pass").cards.len(),
            3
        );
    }

    #[test]
    fn a_handoff_between_two_unscheduled_rows_is_not_an_overlap() {
        let handoff = format!(
            "{}[[card]]\nvendor = \"ollama\"\nmodel = \"deepseek-v4-flash\"\ntoken_class = \"input\"\n\
             rate = \"0.30\"\ncurrency = \"USD\"\nbilling_basis = \"per_million_tokens\"\n\
             effective_start = \"2026-09-07\"\n\n",
            unscheduled_card("deepseek-v4-flash", "0.22", Some("2026-09-07")),
        );
        assert_eq!(parse(&handoff).expect("a handoff must pass").cards.len(), 2);
    }

    // --- percent-of-window estimate cards (`aub-8vpc`) ------------------------

    /// One well-formed estimate card. Every refusal below is this text with
    /// exactly one key changed, so a refusal proves the key it names rather
    /// than some other difference.
    const ESTIMATE_CARD: &str = r#"
[[card]]
vendor = "anthropic"
model = "claude-fable-5"
token_class = "input"
rate = "0.85"
billing_basis = "percent_of_window_per_million_tokens"
window = "five_hour"
unit = "percentage_points"
quality = "estimate"
source = "operator notes, tools aub replaces"
effective_start = "2026-09-07"
"#;

    fn refusal(text: &str) -> RateBookError {
        parse(text).expect_err("the card must be refused")
    }

    #[test]
    fn an_estimate_card_parses_with_its_window_unit_and_quality() {
        let book = parse_ok(ESTIMATE_CARD);
        let card = &book.cards[0];
        assert_eq!(
            card.billing_basis,
            BillingBasis::PercentOfWindowPerMillionTokens
        );
        assert_eq!(card.rate_micros, 850_000);
        assert_eq!(
            card.denomination,
            RateDenomination::Points(RateUnit::PercentagePoints)
        );
        assert_eq!(
            card.window_estimate,
            Some(WindowEstimate {
                window: QuotaWindowKind::FiveHour,
                unit: RateUnit::PercentagePoints,
                quality: CardQuality::Estimate,
            })
        );
    }

    #[test]
    fn an_estimate_card_naming_a_currency_is_refused() {
        let with_currency = ESTIMATE_CARD.replace(
            "unit = \"percentage_points\"",
            "unit = \"percentage_points\"\ncurrency = \"USD\"",
        );
        let error = refusal(&with_currency);
        assert_eq!(error.card_index, 0);
        assert!(error.reason.starts_with("currency "), "{}", error.reason);
    }

    #[test]
    fn an_estimate_card_without_a_window_is_refused() {
        let error = refusal(&ESTIMATE_CARD.replace("window = \"five_hour\"\n", ""));
        assert!(error.reason.starts_with("window "), "{}", error.reason);
    }

    #[test]
    fn an_estimate_card_naming_an_unknown_window_is_refused() {
        let error = refusal(&ESTIMATE_CARD.replace("five_hour", "one_month"));
        assert!(
            error.reason.contains("five_hour | seven_day"),
            "{}",
            error.reason
        );
    }

    #[test]
    fn an_estimate_card_without_a_unit_is_refused() {
        let error = refusal(&ESTIMATE_CARD.replace("unit = \"percentage_points\"\n", ""));
        assert!(error.reason.starts_with("unit "), "{}", error.reason);
    }

    #[test]
    fn an_estimate_card_without_a_quality_is_refused() {
        let error = refusal(&ESTIMATE_CARD.replace("quality = \"estimate\"\n", ""));
        assert!(error.reason.starts_with("quality "), "{}", error.reason);
    }

    /// The one refusal that carries the whole point of the basis: a card of
    /// this shape may not claim to be measured.
    #[test]
    fn an_estimate_card_claiming_to_be_measured_is_refused() {
        let error = refusal(&ESTIMATE_CARD.replace("\"estimate\"", "\"measured\""));
        assert!(error.reason.contains("is not estimate"), "{}", error.reason);
    }

    #[test]
    fn an_estimate_card_without_a_source_is_refused() {
        let error = refusal(
            &ESTIMATE_CARD.replace("source = \"operator notes, tools aub replaces\"\n", ""),
        );
        assert!(error.reason.starts_with("source "), "{}", error.reason);
    }

    #[test]
    fn a_money_card_carrying_a_window_or_a_quality_is_refused() {
        for extra in [
            "window = \"five_hour\"",
            "quality = \"estimate\"",
            "unit = \"percentage_points\"",
        ] {
            let card = MINIMAL_CARD.replace(
                "billing_basis = \"per_million_tokens\"",
                &format!("billing_basis = \"per_million_tokens\"\n{extra}"),
            );
            let error = refusal(&card);
            assert!(
                error
                    .reason
                    .contains("only accepted on a percent_of_window_per_million_tokens card"),
                "{}",
                error.reason
            );
        }
    }

    /// A money card and an estimate card for the same vendor, model and class
    /// price different dimensions, so they are not an overlap. The planted
    /// negative is the same pair with both cards on one basis, which is.
    #[test]
    fn a_money_card_and_an_estimate_card_for_one_class_coexist() {
        let both = format!("{MINIMAL_CARD}{ESTIMATE_CARD}");
        assert_eq!(parse(&both).expect("two bases must coexist").cards.len(), 2);

        let two_estimates = format!("{ESTIMATE_CARD}{ESTIMATE_CARD}");
        let error = refusal(&two_estimates);
        assert!(error.reason.contains("overlaps card 0"), "{}", error.reason);
    }

    /// Two estimates for the two different windows are two facts about one
    /// class, not a conflict.
    #[test]
    fn estimates_for_two_windows_are_not_an_overlap() {
        let weekly = ESTIMATE_CARD.replace("five_hour", "seven_day");
        let both = format!("{ESTIMATE_CARD}{weekly}");
        assert_eq!(
            parse(&both).expect("two windows must coexist").cards.len(),
            2
        );
    }
}
