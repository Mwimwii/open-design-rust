//! Tiny UTC calendar helpers.
//!
//! The Rust daemon carries no date crate (the TypeScript side uses `Date` and
//! `os`), so the couple of civil-date conversions the diagnostics bundle needs
//! — an ISO-8601 millisecond stamp for the manifest and a DOS date/time for
//! the zip entries — live here.

use std::time::{SystemTime, UNIX_EPOCH};

/// Civil UTC calendar fields for an instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcParts {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millis: u32,
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (mp + if mp < 10 { 3 } else { -9 }) as u32;
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

impl UtcParts {
    pub fn from_system_time(now: SystemTime) -> Self {
        let millis_total = match now.duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_millis() as i64,
            Err(err) => -(err.duration().as_millis() as i64),
        };
        let days = millis_total.div_euclid(86_400_000);
        let ms_of_day = millis_total.rem_euclid(86_400_000);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (ms_of_day / 3_600_000) as u32,
            minute: ((ms_of_day / 60_000) % 60) as u32,
            second: ((ms_of_day / 1_000) % 60) as u32,
            millis: (ms_of_day % 1_000) as u32,
        }
    }

    /// `2026-10-17T13:00:47.465Z` — the shape `Date#toISOString` produces.
    pub fn to_iso_millis(self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.millis
        )
    }

    /// DOS date (bits: yyyyyyym mmmddddd) and time (hhhhhmmm mmmsssss) as the
    /// zip format stores them, floored at the 1980 epoch the format cannot
    /// represent anything before.
    pub fn to_dos(self) -> (u16, u16) {
        let year = if self.year < 1980 { 1980 } else { self.year } as u32;
        let dos_date = (((year - 1980) << 9) | (self.month << 5) | self.day) as u16;
        let dos_time = ((self.hour << 11) | (self.minute << 5) | (self.second / 2)) as u16;
        (dos_date, dos_time)
    }
}

/// Current instant as an ISO-8601 millisecond stamp.
pub fn now_iso_millis() -> String {
    UtcParts::from_system_time(SystemTime::now()).to_iso_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64, millis: u32) -> SystemTime {
        UNIX_EPOCH + Duration::new(secs, millis * 1_000_000)
    }

    #[test]
    fn formats_a_known_instant() {
        // 2026-10-17T13:00:47.465Z
        let parts = UtcParts::from_system_time(at(1_792_242_047, 465));
        assert_eq!(parts.to_iso_millis(), "2026-10-17T13:00:47.465Z");
    }

    #[test]
    fn formats_the_epoch() {
        assert_eq!(
            UtcParts::from_system_time(UNIX_EPOCH).to_iso_millis(),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn handles_a_leap_day() {
        // 2024-02-29T00:00:00.000Z
        assert_eq!(
            UtcParts::from_system_time(at(1_709_164_800, 0)).to_iso_millis(),
            "2024-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn dos_time_is_floored_at_1980() {
        let (date, time) = UtcParts::from_system_time(UNIX_EPOCH).to_dos();
        assert_eq!(date, (1 << 5) | 1);
        assert_eq!(time, 0);
    }
}
