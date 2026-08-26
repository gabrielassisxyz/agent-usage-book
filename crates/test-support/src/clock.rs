//! A clock abstraction and its test fake.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The clock abstraction production code reads time from. Tests inject [`FakeClock`]
/// instead of the real wall clock, so freshness and window logic is tested against
/// controlled time rather than the machine's.
pub trait Clock {
    /// The current wall-clock time.
    fn now(&self) -> SystemTime;
}

/// The real wall clock, used by production code.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock a test can set and advance.
pub struct FakeClock {
    now: SystemTime,
}

impl FakeClock {
    /// A clock fixed at the given time.
    pub fn at(now: SystemTime) -> Self {
        FakeClock { now }
    }

    /// A clock fixed at the given number of seconds since the Unix epoch.
    pub fn at_epoch_seconds(seconds: u64) -> Self {
        FakeClock {
            now: UNIX_EPOCH + Duration::from_secs(seconds),
        }
    }

    /// Moves the clock to the given time.
    pub fn set(&mut self, now: SystemTime) {
        self.now = now;
    }

    /// Advances the clock by the given duration.
    pub fn advance(&mut self, duration: Duration) {
        self.now += duration;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        self.now
    }
}
