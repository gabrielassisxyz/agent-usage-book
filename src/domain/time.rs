//! Time types, the measurement basis, and the injected clock.
//!
//! Three mistakes are prevented here. Provider observation time and local
//! receive time are distinct types, so a reading's age cannot silently depend
//! on which clock happened to be handy. Every adapter declares a
//! [`MeasurementBasis`], so freshness never decides without naming which clock
//! the provider contract documents. And timeouts take [`MonotonicDuration`],
//! never a wall-clock duration, so a clock adjustment mid-request cannot
//! cancel or extend a blocking operation.
//!
//! The clock is injected ([`Clock`]) because freshness must never be tested
//! against the real wall clock: every state-machine test moves a
//! [`FakeClock`], which is the only way to assert that time alone can make
//! data stale.

use std::time::Instant as StdInstant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// The time since the Unix epoch, in nanoseconds, for a wall-clock instant.
///
/// The one place a [`SystemTime`] is converted to an epoch count outside the
/// clock module's own reads: a file's modification time is a wall-clock
/// instant, and the modules that need it (credential resolution) must not
/// touch the epoch constant themselves. A pre-epoch instant is impossible in
/// practice and maps to zero rather than failing the caller.
pub fn unix_nanos(system_time: SystemTime) -> u128 {
    system_time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// A wall-clock instant in UTC, as nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTimestamp(i64);

impl UtcTimestamp {
    /// A timestamp from nanoseconds since the Unix epoch.
    pub fn from_unix_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the Unix epoch.
    pub fn unix_nanos(self) -> i64 {
        self.0
    }

    /// Parses an RFC 3339 timestamp such as `2026-08-30T14:26:29.342Z` or
    /// `2026-08-30T11:26:29+00:00` into UTC nanoseconds. Transcript sources write
    /// this shape; anything else is `None`, never a guessed instant, because a
    /// timestamp a parser could not read must not place an event in a day.
    pub fn parse_rfc3339(text: &str) -> Option<Self> {
        let text = text.trim();
        let (date, rest) = text.split_once('T')?;
        let date = UtcDate::parse(date)?;
        let (clock, offset_seconds) = split_offset(rest)?;
        let mut clock_parts = clock.splitn(3, ':');
        let hour: i64 = clock_parts.next()?.parse().ok()?;
        let minute: i64 = clock_parts.next()?.parse().ok()?;
        let second_text = clock_parts.next()?;
        let (second, fraction_nanos) = match second_text.split_once('.') {
            Some((whole, fraction)) => (whole.parse::<i64>().ok()?, fraction_to_nanos(fraction)?),
            None => (second_text.parse::<i64>().ok()?, 0),
        };
        if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..61).contains(&second) {
            return None;
        }
        let day_seconds = hour * 3_600 + minute * 60 + second - offset_seconds;
        let seconds = date.days_since_epoch() * 86_400 + day_seconds;
        Some(Self(seconds.checked_mul(1_000_000_000)? + fraction_nanos))
    }

    /// The UTC calendar day this instant falls on.
    pub fn utc_date(self) -> UtcDate {
        UtcDate::from_days_since_epoch(self.0.div_euclid(86_400 * 1_000_000_000))
    }

    /// The absolute difference between two timestamps, in nanoseconds.
    fn abs_diff_nanos(self, other: UtcTimestamp) -> u64 {
        self.0.abs_diff(other.0)
    }
}

/// The trailing zone designator of an RFC 3339 time: `Z`, `+HH:MM` or `-HH:MM`.
/// Returns the clock text and the offset in seconds to subtract to reach UTC.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(clock) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((clock, 0));
    }
    let sign_at = rest.rfind(['+', '-'])?;
    let (clock, offset) = rest.split_at(sign_at);
    let (sign, hhmm) = offset.split_at(1);
    let (hours, minutes) = hhmm.split_once(':')?;
    let seconds = hours.parse::<i64>().ok()? * 3_600 + minutes.parse::<i64>().ok()? * 60;
    Some((clock, if sign == "-" { -seconds } else { seconds }))
}

/// Fractional seconds as nanoseconds: up to nine digits, the rest truncated.
fn fraction_to_nanos(fraction: &str) -> Option<i64> {
    if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let digits: String = fraction.chars().take(9).collect();
    let scale = 10i64.pow(9 - digits.len() as u32);
    Some(digits.parse::<i64>().ok()? * scale)
}

/// A calendar day in UTC. The unit reports group by; it carries no clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDate {
    year: i64,
    month: u32,
    day: u32,
}

impl UtcDate {
    /// A date from `YYYY-MM-DD`. Rejects an impossible month or day rather than
    /// normalising it.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.splitn(3, '-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: u32 = parts.next()?.parse().ok()?;
        let day: u32 = parts.next()?.parse().ok()?;
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return None;
        }
        Some(Self { year, month, day })
    }

    /// The `YYYY-MM-DD` form. A date is a label, not a quantity, so the text is
    /// produced here rather than through a rendering helper.
    pub fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Midnight at the start of this day.
    pub fn start(self) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(self.days_since_epoch() * 86_400 * 1_000_000_000)
    }

    /// The day after this one, across month and year boundaries.
    pub fn next(self) -> Self {
        Self::from_days_since_epoch(self.days_since_epoch() + 1)
    }

    /// This day plus `days` days.
    pub fn plus_days(self, days: i64) -> Self {
        Self::from_days_since_epoch(self.days_since_epoch() + days)
    }

    /// Days since 1970-01-01, negative before it (Howard Hinnant's civil algorithm).
    fn days_since_epoch(self) -> i64 {
        let year = if self.month <= 2 {
            self.year - 1
        } else {
            self.year
        };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let month_from_march = i64::from((self.month + 9) % 12);
        let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    fn from_days_since_epoch(days: i64) -> Self {
        let shifted = days + 719_468;
        let era = shifted.div_euclid(146_097);
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_from_march = (5 * day_of_year + 2) / 153;
        let day = (day_of_year - (153 * month_from_march + 2) / 5 + 1) as u32;
        let month = if month_from_march < 10 {
            month_from_march + 3
        } else {
            month_from_march - 9
        } as u32;
        let year = year_of_era + era * 400 + i64::from(month <= 2);
        Self { year, month, day }
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

/// The provider's observation time, as documented by the endpoint.
///
/// Distinct from [`ReceivedAt`] and from a bare [`UtcTimestamp`]: a reading's
/// age must not depend on which clock happened to be handy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderObservedAt(UtcTimestamp);

impl ProviderObservedAt {
    /// A provider timestamp from a UTC instant.
    pub fn new(timestamp: UtcTimestamp) -> Self {
        Self(timestamp)
    }

    /// The underlying UTC instant.
    pub fn as_utc(self) -> UtcTimestamp {
        self.0
    }
}

/// The local receive time.
///
/// Distinct from [`ProviderObservedAt`] and from a bare [`UtcTimestamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReceivedAt(UtcTimestamp);

impl ReceivedAt {
    /// A receive timestamp from a UTC instant.
    pub fn new(timestamp: UtcTimestamp) -> Self {
        Self(timestamp)
    }

    /// The underlying UTC instant.
    pub fn as_utc(self) -> UtcTimestamp {
        self.0
    }
}

/// A duration measured on the monotonic clock, in nanoseconds.
///
/// The only duration type accepted by the timeout and command-budget
/// interfaces. A wall-clock duration must not reach those interfaces, because
/// a clock adjustment mid-request would otherwise cancel or extend a blocking
/// operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicDuration(u64);

impl MonotonicDuration {
    /// A duration from nanoseconds.
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// A duration from whole seconds.
    pub const fn from_seconds(seconds: u64) -> Self {
        Self(seconds.saturating_mul(1_000_000_000))
    }

    /// A duration from milliseconds.
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(1_000_000))
    }

    /// Nanoseconds.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

/// A point on the monotonic clock, in nanoseconds since an arbitrary epoch.
///
/// Only differences between two instants are meaningful; the epoch is chosen
/// by the clock implementation and is never compared across clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicInstant(u64);

impl MonotonicInstant {
    /// The monotonic duration elapsed from `earlier` to `self`, saturating at
    /// zero when `earlier` is not actually earlier.
    pub fn duration_since(self, earlier: MonotonicInstant) -> MonotonicDuration {
        MonotonicDuration(self.0.saturating_sub(earlier.0))
    }
}

/// The age of a reading, in nanoseconds.
///
/// Non-negative by construction: the only way to build one is through
/// [`age`], which refuses a measurement time in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Age(u64);

impl Age {
    /// Nanoseconds.
    pub fn as_nanos(self) -> u64 {
        self.0
    }
}

/// Which clock a provider contract documents as the measurement time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeasurementBasis {
    /// The endpoint documents a field as the measurement time.
    ProviderObserved,
    /// The endpoint documents no measurement time; use local receive time.
    LocallyReceived,
    /// The provider's semantics require the conservative reading: the older
    /// of the provider timestamp and the receive timestamp.
    OlderOfTheTwo,
}

/// The configured maximum skew between the provider's clock and the local
/// clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSkewEnvelope(MonotonicDuration);

impl ClockSkewEnvelope {
    /// An envelope from a maximum skew duration.
    pub fn new(max_skew: MonotonicDuration) -> Self {
        Self(max_skew)
    }

    /// The maximum skew, in nanoseconds.
    pub fn as_nanos(self) -> u64 {
        self.0.as_nanos()
    }
}

/// A provider timestamp outside the configured clock-skew envelope, or a
/// measurement time in the future.
///
/// Never a licence to manufacture a negative age or a freshness in the
/// future.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockAnomaly {
    /// The provider timestamp that was checked, when one was present.
    pub provider_observed: Option<ProviderObservedAt>,
    /// The receive timestamp the provider timestamp was checked against.
    pub received: ReceivedAt,
    /// The current time the age was computed at.
    pub now: UtcTimestamp,
}

/// Computes the age of a reading at `now`.
///
/// The measurement time is chosen by `basis`. When a provider timestamp is
/// present it is checked against the clock-skew envelope: a provider
/// timestamp more than `envelope` away from the receive timestamp is a
/// [`ClockAnomaly`]. A measurement time in the future is also a
/// [`ClockAnomaly`], never a negative age.
pub fn age(
    provider_observed: Option<ProviderObservedAt>,
    received: ReceivedAt,
    basis: MeasurementBasis,
    now: UtcTimestamp,
    envelope: ClockSkewEnvelope,
) -> Result<Age, ClockAnomaly> {
    if let Some(provider) = provider_observed {
        let skew = provider.as_utc().abs_diff_nanos(received.as_utc());
        if skew > envelope.as_nanos() {
            return Err(ClockAnomaly {
                provider_observed: Some(provider),
                received,
                now,
            });
        }
    }

    let measurement = match basis {
        MeasurementBasis::ProviderObserved => provider_observed
            .map(ProviderObservedAt::as_utc)
            .unwrap_or_else(|| received.as_utc()),
        MeasurementBasis::LocallyReceived => received.as_utc(),
        MeasurementBasis::OlderOfTheTwo => match provider_observed {
            Some(provider) => provider.as_utc().min(received.as_utc()),
            None => received.as_utc(),
        },
    };

    if measurement > now {
        return Err(ClockAnomaly {
            provider_observed,
            received,
            now,
        });
    }

    Ok(Age(now.unix_nanos().abs_diff(measurement.unix_nanos())))
}

/// The injected clock: wall-clock time and monotonic time.
///
/// No module reads the system clock directly; every caller takes a `Clock` as
/// a parameter so tests can move a [`FakeClock`] instead of the real wall
/// clock.
pub trait Clock {
    /// The current wall-clock time in UTC.
    fn now(&self) -> UtcTimestamp;

    /// The current monotonic time.
    fn monotonic_now(&self) -> MonotonicInstant;
}

/// The real clock, reading the system clock.
#[derive(Debug, Clone, Copy)]
pub struct RealClock {
    base: StdInstant,
}

impl RealClock {
    /// A real clock whose monotonic epoch is "now".
    pub fn new() -> Self {
        Self {
            base: StdInstant::now(),
        }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now(&self) -> UtcTimestamp {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch");
        UtcTimestamp(since_epoch.as_nanos() as i64)
    }

    fn monotonic_now(&self) -> MonotonicInstant {
        MonotonicInstant(self.base.elapsed().as_nanos() as u64)
    }
}

/// A controllable clock for tests.
#[derive(Debug, Clone, Copy)]
pub struct FakeClock {
    now: UtcTimestamp,
    monotonic: MonotonicInstant,
}

impl FakeClock {
    /// A fake clock starting at `now` with a zero monotonic epoch.
    pub fn new(now: UtcTimestamp) -> Self {
        Self {
            now,
            monotonic: MonotonicInstant(0),
        }
    }

    /// Advances both the wall clock and the monotonic clock by `duration`.
    pub fn advance(&mut self, duration: MonotonicDuration) {
        self.now = UtcTimestamp(self.now.unix_nanos() + duration.as_nanos() as i64);
        self.monotonic = MonotonicInstant(self.monotonic.0 + duration.as_nanos());
    }

    /// Sets the wall-clock time directly.
    pub fn set_now(&mut self, now: UtcTimestamp) {
        self.now = now;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> UtcTimestamp {
        self.now
    }

    fn monotonic_now(&self) -> MonotonicInstant {
        self.monotonic
    }
}

/// A timeout, measured on the monotonic clock.
///
/// The only way to build one is from a [`MonotonicDuration`]; a wall-clock
/// duration does not type-check here, which is the compile-time guard against
/// a clock adjustment cancelling or extending a blocking operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(MonotonicDuration);

impl Timeout {
    /// A timeout from a monotonic duration.
    pub fn new(duration: MonotonicDuration) -> Self {
        Self(duration)
    }

    /// The monotonic duration.
    pub fn duration(self) -> MonotonicDuration {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ts(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(seconds * 1_000_000_000)
    }

    fn envelope(seconds: u64) -> ClockSkewEnvelope {
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(seconds))
    }

    /// The epoch conversion reports the instant's nanoseconds since the epoch.
    #[test]
    fn unix_nanos_reports_nanoseconds_since_the_epoch() {
        let instant = UNIX_EPOCH + std::time::Duration::from_secs(2);
        assert_eq!(unix_nanos(instant), 2_000_000_000);
    }

    /// A pre-epoch instant maps to zero rather than failing the caller.
    #[test]
    fn unix_nanos_maps_a_pre_epoch_instant_to_zero() {
        let instant = UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(unix_nanos(instant), 0);
    }

    /// A provider timestamp exactly at the envelope boundary is inside; one
    /// nanosecond past it, in either direction, is a `ClockAnomaly`.
    #[test]
    fn clock_skew_envelope_boundaries_in_both_directions() {
        let received = ReceivedAt::new(ts(1_000));
        let now = ts(1_100);
        let env = envelope(10);

        // Inside, both directions.
        let ahead = ProviderObservedAt::new(ts(1_010));
        let behind = ProviderObservedAt::new(ts(990));
        assert!(
            age(
                Some(ahead),
                received,
                MeasurementBasis::ProviderObserved,
                now,
                env
            )
            .is_ok()
        );
        assert!(
            age(
                Some(behind),
                received,
                MeasurementBasis::ProviderObserved,
                now,
                env
            )
            .is_ok()
        );

        // Outside, both directions.
        let too_far_ahead =
            ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(1_010 * 1_000_000_000 + 1));
        let too_far_behind =
            ProviderObservedAt::new(UtcTimestamp::from_unix_nanos(990 * 1_000_000_000 - 1));
        assert!(
            age(
                Some(too_far_ahead),
                received,
                MeasurementBasis::ProviderObserved,
                now,
                env
            )
            .is_err()
        );
        assert!(
            age(
                Some(too_far_behind),
                received,
                MeasurementBasis::ProviderObserved,
                now,
                env
            )
            .is_err()
        );
    }

    /// A provider timestamp in the future is an anomaly, never a negative age,
    /// even when the skew is within the envelope.
    #[test]
    fn future_provider_timestamp_is_an_anomaly_not_a_negative_age() {
        let received = ReceivedAt::new(ts(1_000));
        let now = ts(1_000);
        let env = envelope(60);
        let future = ProviderObservedAt::new(ts(1_001));

        let result = age(
            Some(future),
            received,
            MeasurementBasis::ProviderObserved,
            now,
            env,
        );
        assert!(result.is_err());
    }

    /// A measurement time in the future (chosen by the basis) is an anomaly.
    #[test]
    fn future_measurement_time_is_an_anomaly() {
        let received = ReceivedAt::new(ts(1_001));
        let now = ts(1_000);
        let env = envelope(60);

        let result = age(None, received, MeasurementBasis::LocallyReceived, now, env);
        assert!(result.is_err());
    }

    /// The age is the difference between `now` and the measurement time.
    #[test]
    fn age_is_now_minus_measurement_time() {
        let received = ReceivedAt::new(ts(1_000));
        let now = ts(1_050);
        let env = envelope(60);

        let result = age(None, received, MeasurementBasis::LocallyReceived, now, env).unwrap();
        assert_eq!(result.as_nanos(), 50_000_000_000);
    }

    /// `OlderOfTheTwo` picks the older of the provider and receive timestamps.
    #[test]
    fn older_of_the_two_picks_the_older_timestamp() {
        let received = ReceivedAt::new(ts(1_000));
        let provider = ProviderObservedAt::new(ts(1_010));
        let now = ts(1_050);
        let env = envelope(60);

        let result = age(
            Some(provider),
            received,
            MeasurementBasis::OlderOfTheTwo,
            now,
            env,
        )
        .unwrap();
        // Older is the receive timestamp (1_000), so age is 50s.
        assert_eq!(result.as_nanos(), 50_000_000_000);
    }

    proptest::proptest! {
        #[test]
        fn prop_age_is_monotonic_as_the_clock_advances(
            received_sec in 1_000i64..100_000i64,
            provider_offset_sec in -30i64..=30i64,
            init_offset_sec in 30i64..100i64,
            advances in proptest::collection::vec(1u64..100u64, 1..20),
        ) {
            let received = ReceivedAt::new(ts(received_sec));
            let provider = ProviderObservedAt::new(ts(received_sec + provider_offset_sec));
            let env = envelope(60);
            let mut clock = FakeClock::new(ts(received_sec + init_offset_sec));

            let mut previous = age(
                Some(provider),
                received,
                MeasurementBasis::ProviderObserved,
                clock.now(),
                env,
            )
            .unwrap()
            .as_nanos();

            for delta in advances {
                clock.advance(MonotonicDuration::from_seconds(delta));
                let current = age(
                    Some(provider),
                    received,
                    MeasurementBasis::ProviderObserved,
                    clock.now(),
                    env,
                )
                .unwrap()
                .as_nanos();
                prop_assert!(
                    current >= previous,
                    "age must not decrease as the clock advances"
                );
                previous = current;
            }
        }
    }

    /// Retained hand-picked regression: walks 100 1-second advances from a fixed start.
    #[test]
    fn age_is_monotonic_as_the_clock_advances_hand_picked() {
        let received = ReceivedAt::new(ts(1_000));
        let provider = ProviderObservedAt::new(ts(1_005));
        let env = envelope(60);

        let mut clock = FakeClock::new(ts(1_010));
        let mut previous = age(
            Some(provider),
            received,
            MeasurementBasis::ProviderObserved,
            clock.now(),
            env,
        )
        .unwrap()
        .as_nanos();

        for _ in 0..100 {
            clock.advance(MonotonicDuration::from_seconds(1));
            let current = age(
                Some(provider),
                received,
                MeasurementBasis::ProviderObserved,
                clock.now(),
                env,
            )
            .unwrap()
            .as_nanos();
            assert!(
                current >= previous,
                "age must not decrease as the clock advances"
            );
            previous = current;
        }
    }

    /// The fake clock advances both wall and monotonic time together.
    #[test]
    fn fake_clock_advances_both_clocks() {
        let mut clock = FakeClock::new(ts(1_000));
        clock.advance(MonotonicDuration::from_seconds(5));
        assert_eq!(clock.now(), ts(1_005));
        assert_eq!(
            clock.monotonic_now().duration_since(MonotonicInstant(0)),
            MonotonicDuration::from_seconds(5)
        );
    }

    /// The real clock reports a wall-clock time after the Unix epoch.
    #[test]
    fn real_clock_reports_a_time_after_the_epoch() {
        let clock = RealClock::new();
        assert!(clock.now().unix_nanos() > 0);
    }

    #[test]
    fn monotonic_duration_from_millis_constructs_correct_nanos() {
        let dur = MonotonicDuration::from_millis(250);
        assert_eq!(dur.as_nanos(), 250_000_000);
    }
}

#[cfg(test)]
mod calendar_tests {
    use super::*;

    #[test]
    fn rfc3339_with_fraction_and_z_parses_to_utc_nanos() {
        let parsed = UtcTimestamp::parse_rfc3339("2026-08-30T14:26:29.342Z").unwrap();
        assert_eq!(parsed.unix_nanos(), 1_788_099_989_342_000_000);
    }

    #[test]
    fn rfc3339_with_an_explicit_offset_is_shifted_to_utc() {
        let zulu = UtcTimestamp::parse_rfc3339("2026-08-30T11:26:29Z").unwrap();
        let minus_three = UtcTimestamp::parse_rfc3339("2026-08-30T08:26:29-03:00").unwrap();
        let plus_zero = UtcTimestamp::parse_rfc3339("2026-08-30T11:26:29+00:00").unwrap();
        assert_eq!(zulu, minus_three);
        assert_eq!(zulu, plus_zero);
    }

    /// The planted negative: a date-only string, an impossible day and a missing
    /// zone are all refused rather than guessed.
    #[test]
    fn unreadable_timestamps_are_none_not_guessed() {
        assert_eq!(UtcTimestamp::parse_rfc3339("2026-08-30"), None);
        assert_eq!(UtcTimestamp::parse_rfc3339("2026-02-30T00:00:00Z"), None);
        assert_eq!(UtcTimestamp::parse_rfc3339("2026-08-30T14:26:29"), None);
        assert_eq!(UtcTimestamp::parse_rfc3339("2026-08-30T24:00:00Z"), None);
    }

    #[test]
    fn a_day_boundary_separates_the_last_and_first_nanosecond() {
        let last = UtcTimestamp::parse_rfc3339("2026-08-30T23:59:59.999999999Z").unwrap();
        let first = UtcTimestamp::parse_rfc3339("2026-08-31T00:00:00Z").unwrap();
        assert_eq!(last.utc_date().iso(), "2026-08-30");
        assert_eq!(first.utc_date().iso(), "2026-08-31");
        assert_eq!(
            UtcDate::parse("2026-08-30").unwrap().next().iso(),
            "2026-08-31"
        );
        assert_eq!(
            UtcDate::parse("2026-12-31").unwrap().next().iso(),
            "2027-01-01"
        );
        assert_eq!(
            UtcDate::parse("2028-02-28").unwrap().next().iso(),
            "2028-02-29"
        );
    }

    #[test]
    fn dates_round_trip_through_days_since_epoch() {
        for text in [
            "1970-01-01",
            "1999-12-31",
            "2000-02-29",
            "2026-08-30",
            "2100-03-01",
        ] {
            let date = UtcDate::parse(text).unwrap();
            assert_eq!(date.start().utc_date(), date, "{text}");
            assert_eq!(date.iso(), text);
        }
        assert_eq!(
            UtcDate::parse("1970-01-01").unwrap().start().unix_nanos(),
            0
        );
    }

    #[test]
    fn a_pre_epoch_instant_still_lands_on_its_calendar_day() {
        let before = UtcTimestamp::from_unix_nanos(-1);
        assert_eq!(before.utc_date().iso(), "1969-12-31");
    }
}
