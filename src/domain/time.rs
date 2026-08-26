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

    /// The absolute difference between two timestamps, in nanoseconds.
    fn abs_diff_nanos(self, other: UtcTimestamp) -> u64 {
        self.0.abs_diff(other.0)
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
    pub fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// A duration from whole seconds.
    pub fn from_seconds(seconds: u64) -> Self {
        Self(seconds.saturating_mul(1_000_000_000))
    }

    /// Nanoseconds.
    pub fn as_nanos(self) -> u64 {
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

    fn ts(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(seconds * 1_000_000_000)
    }

    fn envelope(seconds: u64) -> ClockSkewEnvelope {
        ClockSkewEnvelope::new(MonotonicDuration::from_seconds(seconds))
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

    /// For a fixed observation, age is monotonically non-decreasing as the
    /// fake clock advances.
    #[test]
    fn age_is_monotonic_as_the_clock_advances() {
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
}
