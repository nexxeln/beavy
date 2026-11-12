//! UTC timestamps in RFC 3339 (`2026-11-12T15:20:01Z`) and compact ages, without a
//! date library.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The current time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn now() -> String {
    format(now_unix())
}

/// Format seconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Parse a timestamp written by [`format`].
pub fn parse(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let shape = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    if !shape {
        return None;
    }
    let field = |from: usize, to: usize| -> Option<i64> {
        let digits = &text[from..to];
        if digits.bytes().all(|b| b.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// A compact age for tables, such as `42s`, `5m`, `3h`, or `2d`. Ages of 7 days or
/// more, and times in the future, show the date instead (`2026-11-12`).
pub fn age(then: i64, now: i64) -> String {
    let elapsed = now - then;
    match elapsed {
        0..60 => format!("{elapsed}s"),
        60..3600 => format!("{}m", elapsed / 60),
        3600..86_400 => format!("{}h", elapsed / 3600),
        86_400..604_800 => format!("{}d", elapsed / 86_400),
        _ => format(then)[..10].to_string(),
    }
}

// Howard Hinnant's algorithms for the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_round_trip() {
        assert_eq!(format(0), "1970-01-01T00:00:00Z");
        assert_eq!(format(1_794_496_801), "2026-11-12T15:20:01Z");
        assert_eq!(format(951_782_400), "2000-02-29T00:00:00Z");
        for secs in [0, 951_782_400, 1_794_496_801, 4_102_444_799] {
            assert_eq!(parse(&format(secs)), Some(secs));
        }
        assert_eq!(parse("2026-11-12 15:20:01Z"), None);
        assert_eq!(parse("2026-13-12T15:20:01Z"), None);
        assert_eq!(parse("+026-11-12T15:20:01Z"), None);
    }

    #[test]
    fn ages_are_compact_and_fall_back_to_the_date() {
        let now = 1_794_496_801;
        assert_eq!(age(now - 42, now), "42s");
        assert_eq!(age(now - 300, now), "5m");
        assert_eq!(age(now - 3 * 3600, now), "3h");
        assert_eq!(age(now - 2 * 86_400, now), "2d");
        assert_eq!(age(now - 8 * 86_400, now), "2026-11-04");
        assert_eq!(age(now + 10, now), "2026-11-12");
    }
}
