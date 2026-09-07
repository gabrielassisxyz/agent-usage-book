//! Local wall-clock rendering for the `aub status` grouped grid.
//!
//! The rest of this binary is deliberately UTC-only: a reading's age must not
//! depend on which clock was handy. The status grid is the one exception the
//! operator asked for, and only for display. A reset instant and the report
//! header read far better as "Wed 11:00" in the timezone the person is sitting
//! in than as a UTC clock they have to shift in their head. JSON stays UTC.
//!
//! The zone comes from the environment the same way every other tool on the
//! machine reads it: `TZ` when set, the system zone otherwise, resolved by
//! libc's `tzset`/`localtime_r`. This module owns the one `extern "C"` call and
//! nothing else in the crate reaches for a zone.

use crate::domain::time::UtcTimestamp;

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A broken-down local time: the fields the grid renders and nothing more.
///
/// Constructed from a UTC instant with [`LocalWallClock::of`] (which asks the
/// environment for the zone) or, in tests, from an explicit offset with
/// [`LocalWallClock::at_offset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalWallClock {
    weekday_index: usize,
    month_index: usize,
    day: u32,
    hour: u32,
    minute: u32,
    /// Seconds east of UTC, as libc's `tm_gmtoff` reports it (`-10_800` for
    /// `-03`).
    offset_seconds: i64,
}

impl LocalWallClock {
    /// The local wall clock for `instant`, with the zone taken from `TZ` or the
    /// system zone. `None` only on a platform with no libc `localtime_r`, which
    /// this binary does not target; the caller then falls back to a UTC form.
    pub fn of(instant: UtcTimestamp) -> Option<Self> {
        let utc_seconds = instant.unix_nanos().div_euclid(1_000_000_000);
        let offset_seconds = utc_offset_seconds(utc_seconds)?;
        Some(Self::at_offset(instant, offset_seconds))
    }

    /// The local wall clock for `instant` at a fixed offset east of UTC. The
    /// civil breakdown reuses the crate's own date algorithm (via
    /// [`UtcTimestamp::utc_date`]) applied to the shifted instant, so the only
    /// zone-specific input is the offset itself.
    pub fn at_offset(instant: UtcTimestamp, offset_seconds: i64) -> Self {
        let utc_seconds = instant.unix_nanos().div_euclid(1_000_000_000);
        let local_seconds = utc_seconds + offset_seconds;
        let seconds_of_day = local_seconds.rem_euclid(86_400);
        let days = local_seconds.div_euclid(86_400);
        // 1970-01-01 was a Thursday, index 4 with Sunday at 0.
        let weekday_index = (days + 4).rem_euclid(7) as usize;
        let date = UtcTimestamp::from_unix_nanos(local_seconds * 1_000_000_000).utc_date();
        let (month, day) = month_and_day(&date.iso());
        Self {
            weekday_index,
            month_index: (month - 1) as usize,
            day,
            hour: (seconds_of_day / 3_600) as u32,
            minute: (seconds_of_day % 3_600 / 60) as u32,
            offset_seconds,
        }
    }

    /// The report header stamp: `Sun 06 Sep 21:59 -03`.
    pub fn header_stamp(&self) -> String {
        format!(
            "{} {:02} {} {:02}:{:02} {}",
            WEEKDAYS[self.weekday_index],
            self.day,
            MONTHS[self.month_index],
            self.hour,
            self.minute,
            self.offset_label(),
        )
    }

    /// A reset clock label: `Wed 11:00`.
    pub fn clock_label(&self) -> String {
        format!(
            "{} {:02}:{:02}",
            WEEKDAYS[self.weekday_index], self.hour, self.minute
        )
    }

    /// `-03` for a whole-hour offset, `-0330` when it carries minutes, `+00`
    /// for UTC.
    fn offset_label(&self) -> String {
        let sign = if self.offset_seconds < 0 { '-' } else { '+' };
        let total_minutes = self.offset_seconds.abs() / 60;
        let hours = total_minutes / 60;
        let minutes = total_minutes % 60;
        if minutes == 0 {
            format!("{sign}{hours:02}")
        } else {
            format!("{sign}{hours:02}{minutes:02}")
        }
    }
}

/// The month (1-12) and day-of-month out of a `YYYY-MM-DD` string. The string
/// is produced by [`crate::domain::time::UtcDate::iso`], so its shape is known.
fn month_and_day(iso: &str) -> (u32, u32) {
    let mut parts = iso.split('-');
    let _year = parts.next();
    let month = parts.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    let day = parts.next().and_then(|d| d.parse().ok()).unwrap_or(1);
    (month, day)
}

/// The offset east of UTC, in seconds, for `utc_seconds`, from `TZ` or the
/// system zone. This is the module's one platform call.
#[cfg(unix)]
fn utc_offset_seconds(utc_seconds: i64) -> Option<i64> {
    use std::mem::MaybeUninit;

    // `time_t` is `i64` on every 64-bit unix this binary targets.
    unsafe extern "C" {
        fn tzset();
        fn localtime_r(time: *const i64, result: *mut Tm) -> *mut Tm;
    }

    /// `struct tm` with the two glibc/BSD extension fields the offset needs.
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const std::ffi::c_char,
    }

    let mut tm = MaybeUninit::<Tm>::zeroed();
    let time = utc_seconds;
    // SAFETY: `tzset` re-reads `TZ`; `localtime_r` fills the caller-owned `tm`
    // and returns null only on failure. `time` outlives the call.
    let filled = unsafe {
        tzset();
        !localtime_r(&time, tm.as_mut_ptr()).is_null()
    };
    if !filled {
        return None;
    }
    // SAFETY: `localtime_r` returned non-null, so `tm` is initialised.
    Some(unsafe { tm.assume_init() }.tm_gmtoff)
}

#[cfg(not(unix))]
fn utc_offset_seconds(_utc_seconds: i64) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instant across the DST-less `America/Sao_Paulo` offset (`-03`),
    /// rendered both ways. The instant is 2026-09-06 21:59:30 UTC, which is
    /// 18:59 local; a reset five hours later is 2026-09-07 02:00 UTC, 23:00
    /// local the same day, and one at 2026-09-07 14:00 UTC is 11:00 local on
    /// the Monday.
    #[test]
    fn sao_paulo_offset_renders_the_header_and_the_reset_label() {
        let sao_paulo = -3 * 3_600;
        let header_instant = UtcTimestamp::from_unix_nanos(
            // 2026-09-06T21:59:30Z
            1_788_731_970 * 1_000_000_000,
        );
        let header = LocalWallClock::at_offset(header_instant, sao_paulo);
        assert_eq!(header.header_stamp(), "Sun 06 Sep 18:59 -03");
        assert_eq!(header.clock_label(), "Sun 18:59");

        let monday_reset = UtcTimestamp::from_unix_nanos(1_788_789_600 * 1_000_000_000); // 2026-09-07T14:00:00Z
        assert_eq!(
            LocalWallClock::at_offset(monday_reset, sao_paulo).clock_label(),
            "Mon 11:00"
        );
    }

    /// The offset label collapses a whole-hour zone to two digits and keeps the
    /// minutes only when a zone carries them; UTC reads `+00`.
    #[test]
    fn offset_label_forms() {
        let instant = UtcTimestamp::from_unix_nanos(0);
        assert_eq!(
            LocalWallClock::at_offset(instant, 0).header_stamp(),
            "Thu 01 Jan 00:00 +00"
        );
        assert_eq!(
            LocalWallClock::at_offset(instant, 5 * 3_600 + 30 * 60).header_stamp(),
            "Thu 01 Jan 05:30 +0530"
        );
        assert_eq!(
            LocalWallClock::at_offset(instant, -(3 * 3_600 + 30 * 60)).clock_label(),
            "Wed 20:30"
        );
    }

    /// `of` reads a real zone: under a pinned `TZ` the offset is the zone's,
    /// not UTC. Guarded to one thread because it mutates process env.
    #[test]
    fn of_reads_the_tz_environment() {
        // SAFETY: this is the only test in the module that touches env, and
        // cargo runs each test binary's threads against the same process; the
        // assertion below only needs the value it just set.
        unsafe {
            std::env::set_var("TZ", "America/Sao_Paulo");
        }
        let instant = UtcTimestamp::from_unix_nanos(1_788_731_970 * 1_000_000_000);
        let local = LocalWallClock::of(instant).expect("a unix host has localtime_r");
        assert_eq!(local.header_stamp(), "Sun 06 Sep 18:59 -03");
        unsafe {
            std::env::remove_var("TZ");
        }
    }
}
