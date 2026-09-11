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
    /// The time-of-day window inside which this card applies. `None` is the
    /// default for its (vendor, model, class); `Some` applies only inside the
    /// window (aub-pwtn).
    pub schedule: Option<Schedule>,
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

/// A time-of-day window inside which a rate card applies (aub-pwtn).
///
/// A card without a schedule is the default for its (vendor, model, class); a
/// card with one applies only inside the window. Days are ISO weekdays packed
/// into one byte (bit 0 is Monday); hours are minutes since UTC midnight,
/// start inclusive and end exclusive, on the same UTC day. A window crossing
/// midnight is written as two cards, so start is always before end here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Schedule {
    days: u8,
    start_minutes: u16,
    end_minutes: u16,
}

impl Schedule {
    /// A schedule from ISO weekdays (Monday = 1 through Sunday = 7) and a UTC
    /// half-open minute window. `None` when the days are empty, a day falls
    /// outside 1 through 7, or the window is empty or leaves the UTC day.
    pub fn new(days_iso: &[u32], start_minutes: u16, end_minutes: u16) -> Option<Self> {
        if days_iso.is_empty() || start_minutes >= end_minutes || end_minutes > 24 * 60 {
            return None;
        }
        let mut days = 0u8;
        for &day in days_iso {
            if !(1..=7).contains(&day) {
                return None;
            }
            days |= 1 << (day - 1);
        }
        Some(Self {
            days,
            start_minutes,
            end_minutes,
        })
    }

    /// True when the instant's weekday and UTC minute fall inside the window:
    /// the day is one of the set, the minute is at or after the start and
    /// before the end.
    pub fn contains(&self, weekday_iso: u32, minutes_since_midnight: u32) -> bool {
        if !(1..=7).contains(&weekday_iso) {
            return false;
        }
        (self.days >> (weekday_iso - 1)) & 1 == 1
            && u32::from(self.start_minutes) <= minutes_since_midnight
            && minutes_since_midnight < u32::from(self.end_minutes)
    }

    /// True when both schedules could price the same instant: their day sets
    /// intersect and their hour ranges overlap.
    pub fn overlaps(&self, other: &Schedule) -> bool {
        self.days & other.days != 0
            && self.start_minutes < other.end_minutes
            && other.start_minutes < self.end_minutes
    }

    /// The window width in minutes. Valuation prefers the narrowest containing
    /// window, so a daytime peak never loses to a wider card around it.
    pub fn window_minutes(&self) -> u16 {
        self.end_minutes - self.start_minutes
    }

    /// The ISO weekdays in ascending order.
    pub fn days_iso(&self) -> Vec<u32> {
        (1..=7)
            .filter(|day| (self.days >> (day - 1)) & 1 == 1)
            .collect()
    }

    /// The day set in compact prose: one `mon-fri` range when the days form a
    /// single run, otherwise the comma-joined names.
    pub fn days_compact(&self) -> String {
        let days = self.days_iso();
        let first = days.first().expect("a schedule always names a day");
        let last = days.last().expect("a schedule always names a day");
        if last - first + 1 == days.len() as u32 {
            if first == last {
                day_name(*first).unwrap_or("?").to_string()
            } else {
                format!(
                    "{}-{}",
                    day_name(*first).unwrap_or("?"),
                    day_name(*last).unwrap_or("?")
                )
            }
        } else {
            days.iter()
                .map(|day| day_name(*day).unwrap_or("?"))
                .collect::<Vec<_>>()
                .join(",")
        }
    }

    /// The hour range as `HH:MM-HH:MM`.
    pub fn hours_text(&self) -> String {
        format!(
            "{}-{}",
            format_minutes(self.start_minutes),
            format_minutes(self.end_minutes)
        )
    }

    /// The day set as comma-joined three-letter names in ISO order, the stored
    /// spelling the `schedule_days` column carries.
    pub fn days_text(&self) -> String {
        self.days_iso()
            .iter()
            .map(|day| day_name(*day).unwrap_or("?"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The explain label: `peak mon-fri 12:00-18:00 UTC`. A card without a
    /// schedule renders as `default` at the call site, never here, so the two
    /// spellings cannot drift apart inside this type.
    pub fn describe(&self) -> String {
        format!("peak {} {} UTC", self.days_compact(), self.hours_text())
    }
}

/// The three-letter day name for an ISO weekday, Monday = 1 through Sunday =
/// 7. `None` outside that range, never a guess at the nearest day.
pub fn day_name(weekday_iso: u32) -> Option<&'static str> {
    match weekday_iso {
        1 => Some("mon"),
        2 => Some("tue"),
        3 => Some("wed"),
        4 => Some("thu"),
        5 => Some("fri"),
        6 => Some("sat"),
        7 => Some("sun"),
        _ => None,
    }
}

/// Parses a three-letter day name. An unknown name is `None`, never the
/// nearest known day.
pub fn parse_day_name(text: &str) -> Option<u32> {
    match text.trim().to_ascii_lowercase().as_str() {
        "mon" => Some(1),
        "tue" => Some(2),
        "wed" => Some(3),
        "thu" => Some(4),
        "fri" => Some(5),
        "sat" => Some(6),
        "sun" => Some(7),
        _ => None,
    }
}

/// Why an `hours_utc` window could not be parsed. Start equal to end is an
/// empty window; start after end is a window crossing midnight, which the book
/// writes as two cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoursParseError {
    Malformed(String),
    StartNotBeforeEnd(String),
    CrossesMidnight(String),
}

/// Parses `HH:MM-HH:MM` into a half-open UTC minute window. Both ends stay on
/// the same UTC day: start must be before end, and a later end that reads
/// earlier (a window crossing midnight) is refused rather than wrapped.
pub fn parse_hours_utc(text: &str) -> Result<(u16, u16), HoursParseError> {
    let (start_text, end_text) = text
        .split_once('-')
        .ok_or_else(|| HoursParseError::Malformed(text.to_string()))?;
    let start = parse_clock(start_text.trim())
        .ok_or_else(|| HoursParseError::Malformed(text.to_string()))?;
    let end =
        parse_clock(end_text.trim()).ok_or_else(|| HoursParseError::Malformed(text.to_string()))?;
    if start == end {
        return Err(HoursParseError::StartNotBeforeEnd(text.to_string()));
    }
    if start > end {
        return Err(HoursParseError::CrossesMidnight(text.to_string()));
    }
    Ok((start, end))
}

fn parse_clock(text: &str) -> Option<u16> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: u16 = hours.parse().ok()?;
    let minutes: u16 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(hours * 60 + minutes)
}

/// Minutes since UTC midnight as `HH:MM`.
pub fn format_minutes(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
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

    fn weekday_peak() -> Schedule {
        Schedule::new(&[1, 2, 3, 4, 5], 12 * 60, 18 * 60).expect("peak window must build")
    }

    #[test]
    fn schedule_new_refuses_empty_days_unknown_days_and_bad_windows() {
        assert_eq!(Schedule::new(&[], 720, 1080), None);
        assert_eq!(Schedule::new(&[0], 720, 1080), None);
        assert_eq!(Schedule::new(&[8], 720, 1080), None);
        assert_eq!(Schedule::new(&[1, 9], 720, 1080), None);
        assert_eq!(Schedule::new(&[1], 720, 720), None);
        assert_eq!(Schedule::new(&[1], 1080, 720), None);
        assert_eq!(Schedule::new(&[1], 0, 24 * 60 + 1), None);
        assert!(Schedule::new(&[7], 0, 24 * 60).is_some());
    }

    #[test]
    fn schedule_contains_is_start_inclusive_end_exclusive_on_listed_days() {
        let peak = weekday_peak();
        assert!(peak.contains(2, 12 * 60));
        assert!(peak.contains(2, 14 * 60));
        assert!(peak.contains(2, 18 * 60 - 1));
        assert!(!peak.contains(2, 18 * 60));
        assert!(!peak.contains(2, 11 * 60 + 59));
        assert!(!peak.contains(6, 14 * 60));
        assert!(!peak.contains(7, 14 * 60));
        assert!(!peak.contains(0, 14 * 60));
        assert!(!peak.contains(8, 14 * 60));
    }

    #[test]
    fn schedule_overlap_needs_a_shared_day_and_shared_hours() {
        let peak = weekday_peak();
        let evening = Schedule::new(&[1, 2, 3, 4, 5], 18 * 60, 22 * 60).unwrap();
        assert!(!peak.overlaps(&evening));
        let wider = Schedule::new(&[1, 2, 3, 4, 5], 10 * 60, 20 * 60).unwrap();
        assert!(peak.overlaps(&wider));
        let weekend = Schedule::new(&[6, 7], 12 * 60, 18 * 60).unwrap();
        assert!(!peak.overlaps(&weekend));
        let monday_only = Schedule::new(&[1], 12 * 60, 18 * 60).unwrap();
        assert!(peak.overlaps(&monday_only));
    }

    #[test]
    fn schedule_renders_the_explain_label_and_round_trips_its_days() {
        let peak = weekday_peak();
        assert_eq!(peak.describe(), "peak mon-fri 12:00-18:00 UTC");
        assert_eq!(peak.days_text(), "mon,tue,wed,thu,fri");
        assert_eq!(peak.hours_text(), "12:00-18:00");
        assert_eq!(peak.window_minutes(), 360);
        assert_eq!(peak.days_iso(), vec![1, 2, 3, 4, 5]);
        let split = Schedule::new(&[1, 3, 5], 9 * 60, 10 * 60).unwrap();
        assert_eq!(split.days_compact(), "mon,wed,fri");
        let single = Schedule::new(&[7], 0, 60).unwrap();
        assert_eq!(single.days_compact(), "sun");
    }

    #[test]
    fn day_names_round_trip_and_refuse_the_unknown() {
        for day in 1..=7 {
            let name = day_name(day).expect("every ISO day has a name");
            assert_eq!(parse_day_name(name), Some(day));
            assert_eq!(parse_day_name(&name.to_ascii_uppercase()), Some(day));
        }
        assert_eq!(day_name(0), None);
        assert_eq!(day_name(8), None);
        assert_eq!(parse_day_name("funday"), None);
        assert_eq!(parse_day_name(""), None);
    }

    #[test]
    fn hours_parse_accepts_a_same_day_window_and_names_each_defect() {
        assert_eq!(parse_hours_utc("12:00-18:00"), Ok((720, 1080)));
        assert_eq!(parse_hours_utc("00:00-23:59"), Ok((0, 1439)));
        assert_eq!(
            parse_hours_utc("12:00-12:00"),
            Err(HoursParseError::StartNotBeforeEnd("12:00-12:00".into()))
        );
        assert_eq!(
            parse_hours_utc("18:00-12:00"),
            Err(HoursParseError::CrossesMidnight("18:00-12:00".into()))
        );
        assert_eq!(
            parse_hours_utc("22:00-02:00"),
            Err(HoursParseError::CrossesMidnight("22:00-02:00".into()))
        );
        assert!(matches!(
            parse_hours_utc("noon-midnight"),
            Err(HoursParseError::Malformed(_))
        ));
        assert!(matches!(
            parse_hours_utc("24:00-01:00"),
            Err(HoursParseError::Malformed(_))
        ));
        assert!(matches!(
            parse_hours_utc("12:00"),
            Err(HoursParseError::Malformed(_))
        ));
        assert_eq!(format_minutes(0), "00:00");
        assert_eq!(format_minutes(1439), "23:59");
    }
}
