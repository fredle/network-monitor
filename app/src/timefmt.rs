//! Time helpers. Timestamps travel as unix milliseconds (UTC); local-time
//! formatting goes through the Windows time-zone database so DST is correct.

use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub milli: u32,
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

pub fn utc_civil(ms: i64) -> Civil {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000) as u32;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    Civil { year, month, day, hour: sod / 3600, minute: (sod / 60) % 60, second: sod % 60, milli }
}

/// `2026-09-30T20:00:00.000Z`, the form Event Log XPath expects.
pub fn utc_iso(ms: i64) -> String {
    let c = utc_civil(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second, c.milli
    )
}

pub fn local_civil(ms: i64) -> Civil {
    // Unix ms -> FILETIME (100 ns ticks since 1601-01-01).
    let ticks = (ms as i128 * 10_000 + 116_444_736_000_000_000i128).max(0) as u64;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return utc_civil(ms);
        }
    }
    Civil {
        year: local.wYear as i32,
        month: local.wMonth as u32,
        day: local.wDay as u32,
        hour: local.wHour as u32,
        minute: local.wMinute as u32,
        second: local.wSecond as u32,
        milli: local.wMilliseconds as u32,
    }
}

pub fn local_hms(ms: i64) -> String {
    let c = local_civil(ms);
    format!("{:02}:{:02}:{:02}", c.hour, c.minute, c.second)
}

pub fn local_hms_millis(ms: i64) -> String {
    let c = local_civil(ms);
    format!("{:02}:{:02}:{:02}.{:03}", c.hour, c.minute, c.second, c.milli)
}

pub fn local_date_compact(ms: i64) -> String {
    let c = local_civil(ms);
    format!("{:04}{:02}{:02}", c.year, c.month, c.day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        let c = utc_civil(0);
        assert_eq!((c.year, c.month, c.day, c.hour), (1970, 1, 1, 0));
    }

    #[test]
    fn known_instant_formats_as_iso() {
        // 2026-09-30T21:32:57.123Z
        assert_eq!(utc_iso(1_790_803_977_123), "2026-09-30T21:32:57.123Z");
    }

    #[test]
    fn leap_day_is_handled() {
        // 2024-02-29T12:00:00Z
        let c = utc_civil(1_709_208_000_000);
        assert_eq!((c.year, c.month, c.day, c.hour), (2024, 2, 29, 12));
    }

    #[test]
    fn negative_times_do_not_panic() {
        let c = utc_civil(-1);
        assert_eq!((c.year, c.month, c.day, c.second, c.milli), (1969, 12, 31, 59, 999));
    }

    #[test]
    fn local_formatting_produces_a_clock_string() {
        let s = local_hms(now_ms());
        assert_eq!(s.len(), 8);
        assert_eq!(&s[2..3], ":");
    }
}
