//! Relative timestamps, pure and clock-free.
//!
//! One formatter serves the history sidebar and the Act 1 action board
//! (Story 19.21 `AC3(c)`): the caller supplies `now`, so an age is a
//! deterministic function of two numbers and never reads the wall clock here.

/// Format a Unix timestamp (seconds) relative to `now` (seconds).
///
/// - < 60s: "just now"
/// - < 60m: "Nm ago"
/// - < 24h: "Nh ago"
/// - This year: "Mon DD"
/// - Older: "YYYY-MM-DD"
#[must_use]
pub fn format_relative_time(timestamp: i64, now: i64) -> String {
    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else if is_same_year(timestamp, now) {
        // Same year: "Apr 9"
        format_timestamp_month_day(timestamp)
    } else {
        // Different year: "2023-04-09"
        format_timestamp_date(timestamp)
    }
}

/// Convert a Unix timestamp (seconds since 1970-01-01) to (year, month, day).
/// Algorithm from <http://howardhinnant.github.io/date_algorithms.html>.
fn unix_to_ymd(timestamp: i64) -> (i32, u32, u32) {
    let days = (timestamp / 86400) as i32; // days since 1970-01-01
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn is_same_year(timestamp: i64, now: i64) -> bool {
    let (y1, _, _) = unix_to_ymd(timestamp);
    let (y2, _, _) = unix_to_ymd(now);
    y1 == y2
}

fn format_timestamp_month_day(timestamp: i64) -> String {
    let (_, m, d) = unix_to_ymd(timestamp);
    let month_abbr = match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    };
    format!("{} {}", month_abbr, d)
}

fn format_timestamp_date(timestamp: i64) -> String {
    let (y, m, d) = unix_to_ymd(timestamp);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_relative_time_just_now() {
        let now = 1_700_000_000;
        assert_eq!(format_relative_time(now, now), "just now");
        assert_eq!(format_relative_time(now - 30, now), "just now");
    }

    #[test]
    fn test_format_relative_time_minutes() {
        let now = 1_700_000_000;
        assert_eq!(format_relative_time(now - 60, now), "1m ago");
        assert_eq!(format_relative_time(now - 300, now), "5m ago");
        assert_eq!(format_relative_time(now - 3599, now), "59m ago");
    }

    #[test]
    fn test_format_relative_time_hours() {
        let now = 1_700_000_000;
        assert_eq!(format_relative_time(now - 3600, now), "1h ago");
        assert_eq!(format_relative_time(now - 7200, now), "2h ago");
    }

    #[test]
    fn test_format_relative_time_same_year() {
        // 2023-11-14 (now) vs 2023-04-09
        let now = 1_700_000_000; // 2023-11-14
        let apr9 = 1_681_017_600; // 2023-04-09
        assert_eq!(format_relative_time(apr9, now), "Apr 9");
    }

    #[test]
    fn test_format_relative_time_different_year() {
        // 2024-01-15 (now) vs 2023-04-09
        let now = 1_705_276_800; // 2024-01-15
        let apr9_2023 = 1_681_017_600; // 2023-04-09
        assert_eq!(format_relative_time(apr9_2023, now), "2023-04-09");
    }

    #[test]
    fn test_unix_to_ymd_epoch() {
        assert_eq!(unix_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn test_unix_to_ymd_known_date() {
        // 2023-04-09 00:00:00 UTC = 1681017600
        assert_eq!(unix_to_ymd(1_681_017_600), (2023, 4, 9));
    }

    #[test]
    fn test_is_same_year() {
        let jan_2023 = 1_672_531_200; // 2023-01-01
        let dec_2023 = 1_703_980_800; // 2023-12-31
        let jan_2024 = 1_704_067_200; // 2024-01-01
        assert!(is_same_year(jan_2023, dec_2023));
        assert!(!is_same_year(dec_2023, jan_2024));
    }

    #[test]
    fn test_format_timestamp_month_day() {
        // 2023-04-09
        assert_eq!(format_timestamp_month_day(1_681_017_600), "Apr 9");
        // 2023-01-01
        assert_eq!(format_timestamp_month_day(1_672_531_200), "Jan 1");
        // 2023-12-31
        assert_eq!(format_timestamp_month_day(1_703_980_800), "Dec 31");
    }

    #[test]
    fn test_format_timestamp_date() {
        assert_eq!(format_timestamp_date(1_681_017_600), "2023-04-09");
        assert_eq!(format_timestamp_date(0), "1970-01-01");
    }
}
