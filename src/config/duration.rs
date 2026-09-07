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

/// Renders a resolved duration for `aub config` (aub-ukh5): the largest whole
/// unit that divides it exactly (`12m`, `48h` renders as `2d`, `90s` stays
/// `90s`), so the printed value is exact rather than truncated to a smaller
/// unit the way an age display would. A named function rather than a
/// `Display` impl: `MonotonicDuration` is a domain quantity and the quantity
/// inventory forbids it a free-standing `Display`, so rendering stays a
/// call-site choice here instead.
pub fn format_config_duration(duration: MonotonicDuration) -> String {
    const NANOS_PER_SECOND: u64 = 1_000_000_000;
    let nanos = duration.as_nanos();
    if nanos >= 86_400 * NANOS_PER_SECOND && nanos.is_multiple_of(86_400 * NANOS_PER_SECOND) {
        format!("{}d", nanos / (86_400 * NANOS_PER_SECOND))
    } else if nanos >= 3_600 * NANOS_PER_SECOND && nanos.is_multiple_of(3_600 * NANOS_PER_SECOND) {
        format!("{}h", nanos / (3_600 * NANOS_PER_SECOND))
    } else if nanos >= 60 * NANOS_PER_SECOND && nanos.is_multiple_of(60 * NANOS_PER_SECOND) {
        format!("{}m", nanos / (60 * NANOS_PER_SECOND))
    } else if nanos.is_multiple_of(NANOS_PER_SECOND) {
        format!("{}s", nanos / NANOS_PER_SECOND)
    } else {
        format!("{nanos}ns")
    }
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

    #[test]
    fn config_durations_render_in_the_largest_exact_unit() {
        assert_eq!(
            format_config_duration(parse_duration("12m").unwrap()),
            "12m"
        );
        assert_eq!(format_config_duration(parse_duration("5s").unwrap()), "5s");
        assert_eq!(
            format_config_duration(parse_duration("120s").unwrap()),
            "2m"
        );
        assert_eq!(
            format_config_duration(parse_duration("90s").unwrap()),
            "90s"
        );
        assert_eq!(
            format_config_duration(parse_duration("30d").unwrap()),
            "30d"
        );
        assert_eq!(
            format_config_duration(MonotonicDuration::from_seconds(0)),
            "0s"
        );
    }

    #[test]
    fn config_duration_rendering_is_exact_never_truncated() {
        // 48h is exactly 2d, so the largest exact unit wins; 90s has no
        // exact minute form, so it stays seconds rather than losing 30s to
        // a truncated minute rendering.
        assert_eq!(format_config_duration(parse_duration("48h").unwrap()), "2d");
        assert_eq!(
            format_config_duration(parse_duration("90s").unwrap()),
            "90s"
        );
    }
}
