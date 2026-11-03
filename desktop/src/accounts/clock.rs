//! Dates and spans for the accounts card, without a date crate: the reset times cswap reports
//! (`2026-11-05T02:59:59+00:00`), shown the way the web app shows them, in the viewer's own time
//! zone (`November 5 at 02:59`), and the countdowns and ages next to them (`2d 5h`, `3h ago`).
//!
//! Everything but [`now`] and [`local_offset`] is pure: the callers pass the clock in, so the
//! tests do not depend on when or where they run.

use std::time::{SystemTime, UNIX_EPOCH};

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Days from 1970-01-01 to the civil date `y-m-d` (proleptic Gregorian; `m` is 1..=12).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date (year, month 1..=12, day 1..=31) `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A run of exactly `n` ASCII digits at the start of `s`, and the rest.
fn digits(s: &str, n: usize) -> Option<(u32, &str)> {
    let head = s.get(..n)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, &s[n..]))
}

/// Seconds since the Unix epoch of an ISO 8601 / RFC 3339 timestamp such as
/// `2026-11-05T02:59:59Z`, `2026-11-05T02:59:59.123456+00:00` or `2026-11-05 05:59:59+0300`.
/// A timestamp without an offset is taken as UTC. `None` for anything else.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let (year, rest) = digits(s, 4)?;
    let (month, rest) = digits(rest.strip_prefix('-')?, 2)?;
    let (day, rest) = digits(rest.strip_prefix('-')?, 2)?;
    let rest = rest.strip_prefix(['T', 't', ' '])?;
    let (hour, rest) = digits(rest, 2)?;
    let (minute, rest) = digits(rest.strip_prefix(':')?, 2)?;
    let (second, mut rest) = digits(rest.strip_prefix(':')?, 2)?;
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &frac[n..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, tail) = digits(&rest[1..], 2)?;
            let (om, tail) = match tail.strip_prefix(':').unwrap_or(tail) {
                "" => (0, ""),
                t => digits(t, 2)?,
            };
            if !tail.is_empty() || oh > 23 || om > 59 {
                return None;
            }
            sign * (i64::from(oh) * 3600 + i64::from(om) * 60)
        }
    };
    let year = i64::from(year);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // A leap second counts as the last second of its minute.
    let second = second.min(59);
    Some(
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
            - offset,
    )
}

/// `secs` as the web app shows a reset time, `November 5 at 02:59`, in the zone `offset` seconds
/// east of UTC.
pub fn long_when(secs: i64, offset: i64) -> String {
    let local = secs + offset;
    let (_, month, day) = civil_from_days(local.div_euclid(86_400));
    let in_day = local.rem_euclid(86_400);
    format!(
        "{} {day} at {:02}:{:02}",
        MONTHS[month as usize - 1],
        in_day / 3600,
        in_day % 3600 / 60
    )
}

/// cswap's countdown for `left` seconds: `2d 5h`, `2h 10m`, `34m`; a time that has passed is `0m`.
pub fn countdown(left: i64) -> String {
    let left = left.max(0);
    let (days, hours, minutes) = (left / 86_400, left % 86_400 / 3600, left % 3600 / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// The web app's `ago()`: whole minutes, hours or days, rounded, for something `age` seconds old.
pub fn ago(age: i64) -> String {
    // JavaScript's Math.round: halves go up.
    let minutes = (age as f64 / 60.0 + 0.5).floor() as i64;
    if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 1440 {
        format!("{}h ago", (minutes as f64 / 60.0 + 0.5).floor() as i64)
    } else {
        format!("{}d ago", (minutes as f64 / 1440.0 + 0.5).floor() as i64)
    }
}

/// Seconds since the Unix epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// How far the machine's time zone is east of UTC at the instant `at` (seconds since the epoch),
/// DST included; 0 when the C library cannot say, and on platforms where this is not wired up.
#[cfg(all(unix, target_pointer_width = "64"))]
pub fn local_offset(at: i64) -> i64 {
    /// `struct tm` of glibc, musl and macOS alike: nine ints, then the offset and the zone name.
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: i64,
        zone: *const u8,
    }
    extern "C" {
        fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
    }
    // SAFETY: both pointers are valid for the call; `Tm` is all integers and a pointer, so zeroed
    // is a valid value, and `localtime_r` fills it in before `gmtoff` is read (null: it failed).
    unsafe {
        let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
        if localtime_r(&at, tm.as_mut_ptr()).is_null() {
            return 0;
        }
        tm.assume_init().gmtoff
    }
}

/// Without a C library to ask the zone is UTC.
#[cfg(not(all(unix, target_pointer_width = "64")))]
pub fn local_offset(_at: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_timestamps_in_every_spelling_cswap_and_python_emit() {
        for (text, secs) in [
            ("2026-11-05T02:59:59+00:00", 1_793_847_599),
            ("2026-11-05T02:59:59Z", 1_793_847_599),
            ("2026-11-05T02:59:59.123456+00:00", 1_793_847_599),
            ("2026-11-05T02:59:00", 1_793_847_540),
            ("2026-11-05 02:59:00 ", 1_793_847_540),
            ("2026-11-05T05:59:00+03:00", 1_793_847_540),
            ("2026-11-05T05:59:00+0300", 1_793_847_540),
            ("2026-11-04T21:59:00-05:00", 1_793_847_540),
            ("2028-02-29T12:00:00Z", 1_835_438_400),
            ("1970-01-01T00:00:00Z", 0),
            ("2026-12-31T23:59:59Z", 1_798_761_599),
        ] {
            assert_eq!(parse_iso(text), Some(secs), "{text}");
        }
        // A leap second is the last second of its minute.
        assert_eq!(
            parse_iso("2026-12-31T23:59:60Z"),
            parse_iso("2026-12-31T23:59:59Z")
        );
    }

    #[test]
    fn what_is_not_a_timestamp_is_none() {
        for text in [
            "",
            "soon",
            "2026-11-05",
            "2026-13-05T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-11-31T00:00:00Z",
            "2026-11-05T24:00:00Z",
            "2026-11-05T02:60:00Z",
            "2026-11-05T02:59:59.Z",
            "2026-11-05T02:59:59+3",
            "2026-11-05T02:59:59+03:00:00",
            "2026-11-05T02:59:59 PST",
            "2026/11/05T02:59:59Z",
            "２０２６-11-05T02:59:59Z",
        ] {
            assert_eq!(parse_iso(text), None, "{text:?}");
        }
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
            assert!((1..=12).contains(&m) && (1..=days_in_month(y, m)).contains(&d));
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn a_reset_time_reads_like_the_web_apps() {
        let secs = parse_iso("2026-11-05T02:59:59Z").unwrap();
        assert_eq!(long_when(secs, 0), "November 5 at 02:59");
        // East of UTC, and west of it across midnight into the day before.
        assert_eq!(long_when(secs, 3 * 3600), "November 5 at 05:59");
        assert_eq!(long_when(secs, -5 * 3600), "November 4 at 21:59");
        assert_eq!(long_when(secs, -3 * 86_400), "November 2 at 02:59");
        let new_year = parse_iso("2026-12-31T23:30:00Z").unwrap();
        assert_eq!(long_when(new_year, 3600), "January 1 at 00:30");
        assert_eq!(long_when(0, 0), "January 1 at 00:00");
    }

    #[test]
    fn countdowns_are_cswaps() {
        assert_eq!(countdown(-5), "0m");
        assert_eq!(countdown(0), "0m");
        assert_eq!(countdown(59), "0m");
        assert_eq!(countdown(34 * 60 + 59), "34m");
        assert_eq!(countdown(3600), "1h 0m");
        assert_eq!(countdown(2 * 3600 + 10 * 60), "2h 10m");
        assert_eq!(countdown(86_400), "1d 0h");
        assert_eq!(countdown(2 * 86_400 + 5 * 3600 + 59 * 60), "2d 5h");
    }

    #[test]
    fn ages_round_like_the_web_apps_ago() {
        assert_eq!(ago(0), "0m ago");
        assert_eq!(ago(29), "0m ago");
        assert_eq!(ago(30), "1m ago");
        assert_eq!(ago(59 * 60), "59m ago");
        assert_eq!(ago(89 * 60), "1h ago");
        assert_eq!(ago(90 * 60), "2h ago");
        assert_eq!(ago(23 * 3600), "23h ago");
        assert_eq!(ago(36 * 3600), "2d ago");
        assert_eq!(ago(3 * 86_400), "3d ago");
    }

    #[test]
    fn the_local_zone_is_a_real_one() {
        let offset = local_offset(now());
        assert!(offset.abs() <= 14 * 3600, "{offset}");
        assert_eq!(offset % 900, 0, "zones are whole quarter hours: {offset}");
        assert!(now() > 1_700_000_000);
    }
}
