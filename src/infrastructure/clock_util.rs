//! Wall-clock helpers for "today" / "tomorrow" boundaries in LOCAL TZ
//! (Story 7.5; Dev Notes §"Today boundary"). All helpers go through
//! `chrono::Local` — UTC is intentionally NOT used because the user thinks of
//! "today's spend" in their local day.

use chrono::{Local, TimeZone};

/// Local-TZ midnight today, as a unix-millisecond timestamp.
///
/// Used for "today" aggregation in the usage panel (AC3) and daily-budget
/// recompute (AC5).
pub fn today_start_unix_ms() -> i64 {
    let today = Local::now().date_naive();
    match today.and_hms_opt(0, 0, 0) {
        Some(naive) => match Local.from_local_datetime(&naive).single() {
            Some(dt) => dt.timestamp_millis(),
            None => Local::now().timestamp_millis(),
        },
        None => Local::now().timestamp_millis(),
    }
}

/// Local-TZ midnight TOMORROW, as a unix-second timestamp.
///
/// Used for AC7 `BudgetPause` — the warning is suppressed until this moment.
/// Returns seconds (not millis) to match `BudgetState.dismissed_until_unix`.
pub fn next_midnight_unix() -> i64 {
    let tomorrow = Local::now()
        .date_naive()
        .succ_opt()
        .unwrap_or(Local::now().date_naive());
    match tomorrow.and_hms_opt(0, 0, 0) {
        Some(naive) => match Local.from_local_datetime(&naive).single() {
            Some(dt) => dt.timestamp(),
            None => Local::now().timestamp() + 86_400,
        },
        None => Local::now().timestamp() + 86_400,
    }
}

/// Current unix-second timestamp (wall clock).
pub fn now_unix() -> i64 {
    Local::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_start_returns_millisecond_unix() {
        let t = today_start_unix_ms();
        assert!(
            t > 1_700_000_000_000,
            "today_start should be a recent ms ts: {t}"
        );
        // Today-start is no later than `now`
        let now_ms = Local::now().timestamp_millis();
        assert!(t <= now_ms, "today_start {t} should be <= now {now_ms}");
    }

    #[test]
    fn next_midnight_is_after_today_start_and_no_more_than_25h_away() {
        let today_ms = today_start_unix_ms();
        let tomorrow_s = next_midnight_unix();
        let today_s = today_ms / 1000;
        let diff = tomorrow_s - today_s;
        assert!(
            (23 * 3600..=25 * 3600).contains(&diff),
            "tomorrow-today gap should be ~24h; got {diff}s"
        );
    }
}
