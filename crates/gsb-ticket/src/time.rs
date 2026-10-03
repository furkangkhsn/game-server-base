//! The claims' times: RFC 3339 date-times (PASETO's registered `exp`,
//! `nbf`, `iat` are ISO 8601 strings, not numbers), as Unix seconds.
//!
//! Read: `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)` — what JavaScript's
//! `Date.prototype.toISOString` writes (`…T12:00:00.000Z`) and what the
//! PASETO vectors write (`…T00:00:00+00:00`); `t`/`z` in lower case too.
//! A fraction is truncated (the checks work in whole seconds). Written:
//! `YYYY-MM-DDTHH:MM:SSZ`.
//!
//! Strict on purpose: a time the parser cannot read is a refused ticket
//! (`claims`), never a defaulted one.

/// Unix seconds of a proleptic-Gregorian civil date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count (`civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `n` ASCII digits at `s[at..]` as a number.
fn digits(s: &[u8], at: usize, n: usize) -> Option<i64> {
    let part = s.get(at..at + n)?;
    part.iter().try_fold(0i64, |acc, c| {
        c.is_ascii_digit().then(|| acc * 10 + i64::from(c - b'0'))
    })
}

/// Parse an RFC 3339 date-time into Unix seconds (`None`: not one).
pub fn parse(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let (y, mo, d) = (digits(b, 0, 4)?, digits(b, 5, 2)?, digits(b, 8, 2)?);
    let (h, mi, se) = (digits(b, 11, 2)?, digits(b, 14, 2)?, digits(b, 17, 2)?);
    let seps = [(4, b'-'), (7, b'-'), (13, b':'), (16, b':')];
    if seps.iter().any(|&(i, c)| b.get(i) != Some(&c)) || !matches!(b.get(10), Some(b'T' | b't')) {
        return None;
    }
    // A leap second (`:60`) reads as the next second's start.
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || se > 60
    {
        return None;
    }
    let mut at = 19;
    if b.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while b.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return None;
        }
    }
    let offset = match b.get(at)? {
        b'Z' | b'z' if at + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if at + 6 == b.len() && b[at + 3] == b':' => {
            let (oh, om) = (digits(b, at + 1, 2)?, digits(b, at + 4, 2)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            if *sign == b'-' { -o } else { o }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// The system clock as Unix seconds (`0` for a clock before 1970).
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spellings_a_lobby_writes_are_read() {
        let t = 1_640_995_200; // 2022-01-01T00:00:00Z
        for s in [
            "2022-01-01T00:00:00Z",
            "2022-01-01T00:00:00+00:00",
            "2022-01-01T00:00:00.000Z",
            "2022-01-01t00:00:00.999999z",
            "2022-01-01T03:00:00+03:00",
            "2021-12-31T19:30:00-04:30",
        ] {
            assert_eq!(parse(s), Some(t), "{s}");
        }
        assert_eq!(parse("2024-02-29T12:34:56Z"), Some(1_709_210_096));
        assert_eq!(format(t), "2022-01-01T00:00:00Z");
        for t in [0, 951_782_400, 1_709_210_096, 4_102_444_800] {
            assert_eq!(parse(&format(t)), Some(t), "round trip {t}");
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for s in [
            "",
            "2022-01-01",
            "2022-01-01T00:00:00",
            "2022-01-01 00:00:00Z",
            "2022-13-01T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2022-01-01T24:00:00Z",
            "2022-01-01T00:00:00.Z",
            "2022-01-01T00:00:00+0000",
            "2022-01-01T00:00:00Zjunk",
            "1640995200",
            "２022-01-01T00:00:00Z",
        ] {
            assert_eq!(parse(s), None, "{s:?}");
        }
    }
}
