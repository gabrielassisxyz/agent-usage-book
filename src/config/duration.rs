//! Parses the duration strings the configuration sketch uses (`"1m"`, `"120s"`,
//! `"48h"`) into [`MonotonicDuration`], so every duration-shaped key in [`super`]'s
//! model is a typed quantity rather than a string a consumer has to parse itself.

use crate::domain::time::MonotonicDuration;

/// Parses a duration string: an unsigned integer followed by exactly one unit suffix,
/// `s` (seconds), `m` (minutes) or `h` (hours). No fractional durations, no bare
/// numbers (a unit-less `"60"` is ambiguous between seconds and something else, and
/// this project's own house rule is that an ambiguous quantity is a defect, not a
/// convenience), and no unit outside this closed set.
pub fn parse_duration(raw: &str) -> Result<MonotonicDuration, String> {
    let raw = raw.trim();
    let Some((digits, unit, multiplier)) = split_unit(raw) else {
        return Err(format!(
            "{raw:?} is not a duration: expected a number followed by s, m, h or d"
        ));
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{raw:?} is not a duration: {digits:?} is not an unsigned integer"
        ));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("{raw:?} is not a duration: {digits:?} overflows"))?;
    let seconds = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{raw:?} is not a duration: {unit} overflows"))?;
    Ok(MonotonicDuration::from_seconds(seconds))
}

/// Splits `raw` into its digit prefix and unit suffix, returning the suffix's
/// multiplier in seconds. `None` when the last character is not one of the three
/// recognized unit letters.
fn split_unit(raw: &str) -> Option<(&str, &str, u64)> {
    let (multiplier, unit_len) = match raw.as_bytes().last() {
        Some(b's') => (1, 1),
        Some(b'm') => (60, 1),
        Some(b'h') => (3_600, 1),
        Some(b'd') => (86_400, 1),
        _ => return None,
    };
    let split_at = raw.len() - unit_len;
    Some((&raw[..split_at], &raw[split_at..], multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_recognized_unit() {
        assert_eq!(parse_duration("1m").unwrap().as_nanos(), 60_000_000_000);
        assert_eq!(parse_duration("120s").unwrap().as_nanos(), 120_000_000_000);
        assert_eq!(
            parse_duration("48h").unwrap().as_nanos(),
            48 * 3_600 * 1_000_000_000
        );
        // Days joined the vocabulary with the coverage selector, whose plan
        // example reads `--since 30d` (PLAN.md section 27).
        assert_eq!(
            parse_duration("30d").unwrap().as_nanos(),
            30 * 86_400 * 1_000_000_000
        );
    }

    #[test]
    fn rejects_a_bare_number_with_no_unit() {
        assert!(parse_duration("60").is_err());
    }

    #[test]
    fn rejects_an_unrecognized_unit() {
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn rejects_a_fractional_value() {
        assert!(parse_duration("1.5m").is_err());
    }

    #[test]
    fn rejects_an_empty_string() {
        assert!(parse_duration("").is_err());
    }
}
