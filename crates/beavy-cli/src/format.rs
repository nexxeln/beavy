//! Shared formatting for counts, sizes, and times.

use beavy::time;

/// A count with thousands separators: `388,848`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A size in decimal units: `812 B`, `4.2 KB`, `3.8 GB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A timestamp in a table: an age on a terminal, the timestamp itself otherwise.
pub fn when(timestamp: &str, terminal: bool) -> String {
    match time::parse(timestamp) {
        Some(secs) if terminal => time::age(secs, time::now_unix()),
        _ => timestamp.to_string(),
    }
}

/// A timestamp in a detail view: `2026-11-12 15:20 UTC (2h ago)`.
pub fn detail_time(timestamp: &str) -> String {
    match time::parse(timestamp) {
        Some(secs) => {
            let text = time::format(secs);
            let age = time::age(secs, time::now_unix());
            let age = if age.contains('-') {
                String::new()
            } else {
                format!(" ({age} ago)")
            };
            format!("{} {} UTC{age}", &text[..10], &text[11..16])
        }
        None => timestamp.to_string(),
    }
}

/// A count and a noun: `1 snapshot`, `3 snapshots`.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_sizes_read_naturally() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(388_848), "388,848");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(4_200), "4.2 KB");
        assert_eq!(bytes(3_800_000_000), "3.8 GB");
        assert_eq!(plural(1, "snapshot", "snapshots"), "1 snapshot");
        assert_eq!(plural(2, "snapshot", "snapshots"), "2 snapshots");
    }

    #[test]
    fn times_are_ages_on_terminals_and_timestamps_elsewhere() {
        assert_eq!(when("2026-11-12T15:20:01Z", false), "2026-11-12T15:20:01Z");
        assert_eq!(when("not a time", true), "not a time");
        assert!(detail_time("2001-01-02T03:04:05Z").starts_with("2001-01-02 03:04 UTC"));
    }
}
