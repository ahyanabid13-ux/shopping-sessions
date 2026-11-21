//! Timestamps: the three valid forms of rules.md, to epoch milliseconds (UTC).
//!
//! F1 `2026-10-03T23:58:10Z`, F2 `2026-10-03T19:58:10-04:00`, F3 digits-only
//! epoch milliseconds (leading zeros allowed). Anything else is bad.

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const DAY: i64 = 86_400_000;

/// An F3 value too large for a signed 64-bit integer: a valid form, but
/// outside every window. Callers compare it against the window like any value.
pub const TOO_LARGE: i64 = i64::MAX;

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant).
#[inline]
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a day number since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[inline]
fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[inline]
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[inline(always)]
fn digit(b: u8) -> Option<i64> {
    let d = b.wrapping_sub(b'0');
    if d < 10 {
        Some(d as i64)
    } else {
        None
    }
}

#[inline(always)]
fn two(b: &[u8], at: usize) -> Option<i64> {
    Some(digit(b[at])? * 10 + digit(b[at + 1])?)
}

/// F1 and F2 (length 20 or 25, already checked by the caller).
#[inline]
fn parse_iso(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let offset_minutes = if b.len() == 20 {
        if b[19] != b'Z' {
            return None;
        }
        0
    } else {
        let sign = match b[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if b[22] != b':' {
            return None;
        }
        let oh = two(b, 20)?;
        let om = two(b, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        sign * (oh * 60 + om)
    };
    let year = two(b, 0)? * 100 + two(b, 2)?;
    let month = two(b, 5)?;
    let day = two(b, 8)?;
    let hour = two(b, 11)?;
    let minute = two(b, 14)?;
    let second = two(b, 17)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) || hour > 23 || minute > 59 || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hour) * 60 + minute) * MINUTE + second * SECOND - offset_minutes * MINUTE)
}

/// F3: digits only, at least one. Values past i64::MAX are `TOO_LARGE`.
#[inline]
fn parse_f3(b: &[u8]) -> Option<i64> {
    let mut value: u64 = 0;
    let mut overflow = false;
    for &c in b {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        if !overflow {
            match value.checked_mul(10).and_then(|v| v.checked_add(d as u64)) {
                Some(v) => value = v,
                None => overflow = true,
            }
        }
    }
    if overflow || value > i64::MAX as u64 {
        Some(TOO_LARGE)
    } else {
        Some(value as i64)
    }
}

/// Epoch milliseconds of a timestamp field, or None when it is bad.
/// An empty field is bad.
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    // A 20- or 25-byte F3 value has a digit where F1/F2 have '-'.
    if (n == 20 || n == 25) && b[4] == b'-' {
        return parse_iso(b);
    }
    parse_f3(b)
}

/// UTC calendar date (y, m, d) of epoch milliseconds.
pub fn date_of_ms(ms: i64) -> (i64, i64, i64) {
    civil_from_days(ms.div_euclid(DAY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms() {
        let f1 = parse(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse(b"1791071890000"), Some(1_791_071_890_000));
        assert_eq!(parse(b"0001791071890000"), Some(1_791_071_890_000));
        assert_eq!(f1, 1_791_071_890_000);
        assert_eq!(parse(b"2026-10-04T01:30:00+02:00"), parse(b"2026-10-03T23:30:00Z"));
        assert_eq!(parse(b"2026-10-03T10:00:00-00:00"), parse(b"2026-10-03T10:00:00Z"));
        assert_eq!(parse(b"2026-10-03T10:00:00+23:59").is_some(), true);
    }

    #[test]
    fn bad() {
        for bad in [
            &b""[..],
            b"2026-10-03T23:58:10z",
            b"2026-10-03t23:58:10Z",
            b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10",
            b"2026-10-03T23:58:10.123Z",
            b"1791071890000.0",
            b"1.79e12",
            b"-5",
            b"2026-13-03T23:58:10Z",
            b"2026-02-30T23:58:10Z",
            b"2027-02-29T23:58:10Z",
            b"2026-10-03T24:00:00Z",
            b"2026-10-03T23:58:60Z",
            b"2026-10-03T23:58:10+24:00",
            b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200",
            "２026-10-03T10:00:00Z".as_bytes(),
            "٢٥٠".as_bytes(),
        ] {
            assert_eq!(parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(parse(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse(b"179102160000000000000"), Some(TOO_LARGE));
        assert_eq!(parse(b"86400000"), Some(86_400_000));
    }

    #[test]
    fn civil_roundtrip() {
        for days in -800_000..800_000i64 {
            if days % 997 != 0 {
                continue;
            }
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
