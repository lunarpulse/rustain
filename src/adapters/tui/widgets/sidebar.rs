//! History sidebar widget for conversation list display.
//!
//! Renders a scrollable list of conversations with:
//! - Title (truncated)
//! - Relative timestamp (e.g., "2m ago", "3h ago", "Apr 9")
//! - Message count
//! - Visual indicators for open/active status

use ratatui::{
    prelude::*,
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::models::session_meta::now_unix;
use crate::domain::services::session_index::SessionSummary;

/// Render the history sidebar panel.
///
/// # Arguments
/// * `area` - The rect to render in
/// * `buf` - The buffer to render to
/// * `entries` - The session summaries to display (sorted by updated_at desc)
/// * `selected_index` - The currently selected entry index
/// * `active_conversation_id` - The ID of the conversation in the active tab (for highlighting)
/// * `theme` - The current theme (for colors)
pub fn render_history_panel(
    area: Rect,
    buf: &mut Buffer,
    entries: &[SessionSummary],
    selected_index: usize,
    active_conversation_id: Option<&str>,
    theme: &crate::adapters::tui::theme::Theme,
) {
    // Clear the area first
    Clear.render(area, buf);

    // Create the block with title
    let block = Block::default()
        .title("History")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.fg_secondary));

    let inner_area = block.inner(area);
    block.render(area, buf);

    if entries.is_empty() {
        // Empty state
        let empty_text = Text::from("No conversations").style(
            Style::default()
                .fg(theme.colors.fg_muted)
                .add_modifier(Modifier::ITALIC),
        );
        let empty_paragraph =
            ratatui::widgets::Paragraph::new(empty_text).alignment(Alignment::Center);
        empty_paragraph.render(inner_area, buf);
        return;
    }

    // Build list items — compute now once for consistent timestamps
    let now = now_unix();
    let list_items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let is_selected = idx == selected_index;
            let is_active = active_conversation_id == Some(entry.conversation_id.as_str());

            // Format the line: [indicator] [fork?] Title (relative_time) [msg_count]
            let indicator = if entry.is_active {
                "● " // Active conversation indicator
            } else if entry.is_open {
                "○ " // Open but not active
            } else {
                "  "
            };
            // Story 4-3a.1 / DF-095: forked conversations get a prefix marker.
            let fork_marker = if entry.has_fork_source { "🔀 " } else { "" };
            let relative_time = format_relative_time(entry.updated_at, now);
            let msg_count = format!("[{}]", entry.message_count);

            // Calculate available width for title using display width
            let time_width = relative_time.width() + 1; // +1 for space
            let count_width = msg_count.width() + 1; // +1 for space
            let indicator_width = 2;
            let fork_marker_width = fork_marker.width();
            let available_width = (inner_area.width as usize)
                .saturating_sub(time_width + count_width + indicator_width + fork_marker_width + 2); // -2 for padding

            let title = if entry.title.is_empty() {
                "(Untitled)"
            } else {
                &entry.title
            };
            let truncated_title = truncate_to_width(title, available_width);

            let line_text = format!(
                "{}{}{} {} {}",
                indicator, fork_marker, truncated_title, relative_time, msg_count
            );

            // Determine style based on state
            let style = if is_active {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else if is_selected {
                Style::default()
                    .fg(theme.colors.fg_primary)
                    .bg(theme.colors.bg_secondary)
            } else {
                Style::default().fg(theme.colors.fg_primary)
            };

            ListItem::new(line_text).style(style)
        })
        .collect();

    // Create the list widget
    let mut list_state = ListState::default();
    list_state.select(Some(selected_index));

    let list = List::new(list_items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");

    // Render with state
    ratatui::widgets::StatefulWidget::render(list, inner_area, buf, &mut list_state);
}

/// Format a Unix timestamp as a relative time string.
///
/// - < 60s: "just now"
/// - < 60m: "Nm ago"
/// - < 24h: "Nh ago"  
/// - This year: "Mon DD"
/// - Older: "YYYY-MM-DD"
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

/// Truncate text to fit within a given display width.
/// Uses char_indices for UTF-8 safe truncation.
/// F18: Handles zero-width characters properly when max_width < 3.
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }

    // F18: If max_width can't even fit the ellipsis, return what fits
    // Handle zero-width characters by tracking actual display width added
    if max_width < 3 {
        let mut result = String::new();
        let mut display_width = 0;
        for ch in text.chars() {
            let cw = ch.width().unwrap_or(0);
            // F18: Skip zero-width chars but include them in result if there's room
            // They don't consume display width but need to be attached to base chars
            if cw == 0 {
                result.push(ch);
                continue;
            }
            if display_width + cw > max_width {
                break;
            }
            result.push(ch);
            display_width += cw;
        }
        return result;
    }

    let mut result = String::with_capacity(max_width);
    let mut current_width = 0;
    let ellipsis_width = 3; // "..." is 3 columns

    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        // F18: Handle zero-width characters - include them without counting toward width
        if ch_width == 0 {
            result.push(ch);
            continue;
        }
        if current_width + ch_width > max_width - ellipsis_width {
            result.push_str("...");
            break;
        }
        result.push(ch);
        current_width += ch_width;
    }

    result
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

    #[test]
    fn test_truncate_to_width() {
        assert_eq!(truncate_to_width("Hello", 10), "Hello");
        assert_eq!(truncate_to_width("Hello World", 8), "Hello...");
        assert_eq!(truncate_to_width("Test", 3), "...");
    }

    #[test]
    fn test_truncate_to_width_multibyte() {
        // CJK characters are 2 columns wide each
        let text = "こんにちは世界"; // 7 chars, 14 columns
        let result = truncate_to_width(text, 8);
        // max_width=8, minus 3 for "..." leaves 5 columns for CJK
        // 2 CJK chars = 4 columns (next would exceed 5), so "こん..."
        assert!(result.ends_with("..."));
        assert_eq!(result, "こん...");
    }
}
