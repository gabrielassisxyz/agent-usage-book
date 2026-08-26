//! Rendering helpers that require explicit context.
//!
//! Every helper takes a unit label, a qualification and a precision policy, plus
//! freshness where the value is a meter reading. A bare scalar cannot reach a
//! user-visible surface because no helper here accepts one, and a bare total where
//! known missing evidence affects the aggregate is refused by construction.

use crate::domain::failure::FailureClass;
use crate::domain::freshness::{Freshness, StaleReason};
use crate::domain::quota::QuotaRemaining;
use crate::domain::render::Precision;
use crate::domain::time::{Age, ClockSkewEnvelope, UtcTimestamp, age};
use crate::evidence::CoverageCompleteness;
use crate::presentation::vocabulary::Qualification;

/// Formats a raw integer with the given number of fractional digits, trimming
/// trailing zeros. The raw value is already scaled to the display unit.
pub fn format_number(raw: &str, precision: Precision) -> String {
    let digits = precision.digits() as usize;
    if digits == 0 {
        return raw.to_string();
    }
    let (sign, body) = match raw.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", raw),
    };
    let padded = format!("{body:0>width$}", width = digits + 1);
    let (int, frac) = padded.split_at(padded.len() - digits);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{sign}{int}")
    } else {
        format!("{sign}{int}.{frac}")
    }
}

/// Renders a quota fraction in parts per million as a percentage with the given
/// precision. One percent is 10_000 ppm.
pub fn render_percentage(ppm: u32, precision: Precision) -> String {
    let digits = precision.digits() as u32;
    let divisor = 10u32.pow(digits);
    let scaled = (u64::from(ppm) * u64::from(divisor) + 5_000) / 10_000;
    format_number(&scaled.to_string(), precision)
}

/// Renders a quantity with its unit, precision and qualification.
pub fn render_quantity(
    raw: &str,
    unit: &str,
    precision: Precision,
    qualification: Qualification,
) -> String {
    let value = format_number(raw, precision);
    format!("{value} {unit} ({})", qualification.term())
}

/// Renders a total. A complete aggregate is a total; a partial aggregate is a known
/// subtotal, never a bare total, because a bare total where known missing evidence
/// affects the aggregate is forbidden everywhere.
pub fn render_total(
    raw: &str,
    unit: &str,
    precision: Precision,
    coverage: &CoverageCompleteness,
) -> String {
    let value = format_number(raw, precision);
    match coverage {
        CoverageCompleteness::Complete => format!("Total: {value} {unit}"),
        CoverageCompleteness::Partial { .. } => {
            format!("Known subtotal: {value} {unit}; report incomplete")
        }
    }
}

/// Renders a meter reading with its freshness, age and reason. Freshness is conveyed
/// in text, never by colour alone: the state is readable from the words themselves.
pub fn render_meter_reading(
    reading: &Freshness<QuotaRemaining>,
    unit: &str,
    precision: Precision,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> String {
    match reading {
        Freshness::Fresh { observed, .. } => {
            let value = render_percentage(observed.value().as_ppm().get(), precision);
            match observed_age(observed, now, envelope) {
                Some(age) => format!("{value}{unit} left · {}", render_age(age)),
                None => format!("{value}{unit} left"),
            }
        }
        Freshness::Stale {
            last_good, reason, ..
        } => {
            let value = last_good
                .as_ref()
                .map(|observed| render_percentage(observed.value().as_ppm().get(), precision));
            let age = last_good
                .as_ref()
                .and_then(|observed| observed_age(observed, now, envelope));
            match (value, age) {
                (Some(value), Some(age)) => format!(
                    "~{value}{unit} · stale {} · {}",
                    render_age(age),
                    render_stale_reason(*reason)
                ),
                (Some(value), None) => {
                    format!("~{value}{unit} · stale · {}", render_stale_reason(*reason))
                }
                (None, _) => format!("? · stale · {}", render_stale_reason(*reason)),
            }
        }
        Freshness::AuthRequired { .. } => "auth!".to_string(),
    }
}

fn observed_age(
    observed: &crate::domain::freshness::Observed<QuotaRemaining>,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> Option<Age> {
    age(
        observed.provider_observed_at(),
        observed.received_at(),
        observed.measurement_basis(),
        now,
        envelope,
    )
    .ok()
}

/// Renders a stale reason as the fixed human wording.
pub fn render_stale_reason(reason: StaleReason) -> &'static str {
    match reason {
        StaleReason::AgeExceeded => "age exceeded",
        StaleReason::NoSuccessfulObservation => "no successful sample",
        StaleReason::SourceUnreachable(class) => render_failure_class(class),
        StaleReason::MalformedProviderResponse => "malformed response",
        StaleReason::RateLimited => "rate limited",
        StaleReason::SamplingGap => "sampling gap",
        StaleReason::ClockAnomaly => "clock anomaly",
        StaleReason::CollectorInterrupted => "collector interrupted",
        StaleReason::CredentialChangedUnverified => "credential changed",
    }
}

/// Renders a failure class as the fixed human wording.
pub fn render_failure_class(class: FailureClass) -> &'static str {
    match class {
        FailureClass::DnsFailure => "dns failure",
        FailureClass::ConnectTimeout
        | FailureClass::ReadTimeout
        | FailureClass::TotalBudgetExpired => "timeout",
        FailureClass::HttpStatus(_) => "http error",
        FailureClass::RateLimited { .. } => "rate limited",
        FailureClass::MalformedBody | FailureClass::MissingRequiredField => "malformed response",
    }
}

/// Renders an age as a compact human duration: seconds, minutes, hours or days.
pub fn render_age(age: Age) -> String {
    let seconds = age.as_nanos() / 1_000_000_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::attempt::AttemptId;
    use crate::domain::freshness::Observed;
    use crate::domain::quota::QuotaFractionPpm;
    use crate::domain::time::{MeasurementBasis, MonotonicDuration, ReceivedAt};

    const NANOS_PER_SECOND: i64 = 1_000_000_000;

    fn now() -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(1_000_000 * NANOS_PER_SECOND)
    }

    fn envelope() -> ClockSkewEnvelope {
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(60))
    }

    fn remaining(ppm: u32) -> QuotaRemaining {
        QuotaRemaining::new(QuotaFractionPpm::new(ppm as i32).unwrap())
    }

    fn observed(ppm: u32, received: UtcTimestamp) -> Observed<QuotaRemaining> {
        Observed::new(
            remaining(ppm),
            None,
            ReceivedAt::new(received),
            MeasurementBasis::LocallyReceived,
        )
    }

    /// The design's example status renderings (PLAN.md section 48), so a wording
    /// change is a deliberate diff.
    #[test]
    fn golden_status_renderings() {
        let now = now();
        let envelope = envelope();
        let precision = crate::presentation::precision::PERCENT;

        let fresh = Freshness::Fresh {
            observed: observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 5 * 3_600 * NANOS_PER_SECOND),
            ),
            latest_attempt: AttemptId::new(1),
        };
        assert_eq!(
            render_meter_reading(&fresh, "%", precision, now, envelope),
            "38% left · 5h"
        );

        let stale_timeout = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 14 * 60 * NANOS_PER_SECOND),
            )),
            latest_attempt: AttemptId::new(2),
            reason: StaleReason::SourceUnreachable(FailureClass::ConnectTimeout),
        };
        assert_eq!(
            render_meter_reading(&stale_timeout, "%", precision, now, envelope),
            "~38% · stale 14m · timeout"
        );

        let auth = Freshness::<QuotaRemaining>::AuthRequired {
            last_good: None,
            latest_attempt: AttemptId::new(3),
        };
        assert_eq!(
            render_meter_reading(&auth, "%", precision, now, envelope),
            "auth!"
        );

        let stale_interrupted = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 9 * 60 * NANOS_PER_SECOND),
            )),
            latest_attempt: AttemptId::new(4),
            reason: StaleReason::CollectorInterrupted,
        };
        assert_eq!(
            render_meter_reading(&stale_interrupted, "%", precision, now, envelope),
            "~38% · stale 9m · collector interrupted"
        );

        let never_observed = Freshness::<QuotaRemaining>::Stale {
            last_good: None,
            latest_attempt: AttemptId::new(5),
            reason: StaleReason::NoSuccessfulObservation,
        };
        assert_eq!(
            render_meter_reading(&never_observed, "%", precision, now, envelope),
            "? · stale · no successful sample"
        );
    }

    /// A partial aggregate is never rendered as a bare total: it is a known subtotal.
    #[test]
    fn a_partial_aggregate_is_never_a_bare_total() {
        let complete = render_total(
            "1200000",
            "tokens",
            crate::presentation::precision::TOKENS,
            &CoverageCompleteness::Complete,
        );
        assert_eq!(complete, "Total: 1200000 tokens");

        let partial = render_total(
            "1200000",
            "tokens",
            crate::presentation::precision::TOKENS,
            &CoverageCompleteness::partial([crate::evidence::ComponentKind::new("cache-write")]),
        );
        assert!(
            !partial.contains("Total:"),
            "a partial aggregate must not be a bare total: {partial}"
        );
        assert!(partial.contains("Known subtotal"));
    }

    /// A stale value is rendered with its age and reason attached, never as a
    /// standalone number: the rendered line always carries the stale marker.
    #[test]
    fn a_stale_value_carries_its_age_and_reason() {
        let now = now();
        let stale = Freshness::Stale {
            last_good: Some(observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 14 * 60 * NANOS_PER_SECOND),
            )),
            latest_attempt: AttemptId::new(1),
            reason: StaleReason::SourceUnreachable(FailureClass::ReadTimeout),
        };
        let rendered = render_meter_reading(
            &stale,
            "%",
            crate::presentation::precision::PERCENT,
            now,
            envelope(),
        );
        assert!(
            rendered.contains("stale"),
            "stale must be in text: {rendered}"
        );
        assert!(rendered.contains("14m"), "age must be attached: {rendered}");
        assert!(
            rendered.contains("timeout"),
            "reason must be attached: {rendered}"
        );
    }

    /// Freshness is conveyed in text, never by colour alone: with no colour at all
    /// the state is still readable from the words.
    #[test]
    fn freshness_is_readable_without_colour() {
        let now = now();
        let envelope = envelope();
        let precision = crate::presentation::precision::PERCENT;

        let fresh = Freshness::Fresh {
            observed: observed(
                380_000,
                UtcTimestamp::from_unix_nanos(now.unix_nanos() - 5 * 3_600 * NANOS_PER_SECOND),
            ),
            latest_attempt: AttemptId::new(1),
        };
        let stale = Freshness::Stale {
            last_good: None,
            latest_attempt: AttemptId::new(2),
            reason: StaleReason::NoSuccessfulObservation,
        };
        let auth = Freshness::<QuotaRemaining>::AuthRequired {
            last_good: None,
            latest_attempt: AttemptId::new(3),
        };

        // No colour is ever added; the words alone distinguish the three states.
        assert!(render_meter_reading(&fresh, "%", precision, now, envelope).contains("left"));
        assert!(render_meter_reading(&stale, "%", precision, now, envelope).contains("stale"));
        assert_eq!(
            render_meter_reading(&auth, "%", precision, now, envelope),
            "auth!"
        );
    }

    #[test]
    fn format_number_trims_trailing_zeros() {
        assert_eq!(format_number("3800", Precision::new(2)), "38");
        assert_eq!(format_number("3855", Precision::new(2)), "38.55");
        assert_eq!(format_number("5", Precision::new(2)), "0.05");
        assert_eq!(format_number("42", Precision::new(0)), "42");
        assert_eq!(format_number("-1234", Precision::new(2)), "-12.34");
    }

    #[test]
    fn render_percentage_converts_ppm() {
        assert_eq!(render_percentage(380_000, Precision::new(2)), "38");
        assert_eq!(render_percentage(385_500, Precision::new(2)), "38.55");
        assert_eq!(render_percentage(1_000_000, Precision::new(2)), "100");
    }
}
