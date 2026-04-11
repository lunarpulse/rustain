use ratatui::prelude::*;

use crate::adapters::tui::theme::Theme;
use crate::domain::models::tab::TabManager;

/// Maximum characters per tab title before truncation.
const MAX_TITLE_CHARS: usize = 20;

/// Render the tab bar showing all open tabs.
///
/// Active tab is highlighted with the theme accent color.
/// Individual titles are char-boundary-safe truncated (never byte-sliced).
/// When tabs overflow the terminal width, a "+N" count indicator is shown.
/// The active tab is always visible.
pub fn render_tab_bar(
    tab_manager: &TabManager,
    active_index: usize,
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Build tab strings
    let tabs = tab_manager.tabs();
    let mut tab_strings: Vec<String> = tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let title = if t.conversation.title.is_empty() {
                format!("Tab {}", i + 1)
            } else {
                truncate_to_chars(&t.conversation.title, MAX_TITLE_CHARS)
            };
            format!("[{}]", title)
        })
        .collect();

    let width = area.width as usize;

    // Check if everything fits (plus separator spaces)
    let total_len: usize = tab_strings.iter().map(|s| s.chars().count() + 1).sum();

    let mut overflow_count = 0;
    if total_len > width {
        // We need to hide some tabs. Always keep active tab visible.
        // Strategy: greedily include tabs from active outward until we run out of space.
        // Reserve space for the "+N" indicator (up to 5 chars: " +99 ").
        let indicator_reserve = 5;
        let available = width.saturating_sub(indicator_reserve);

        // Compute which tabs fit around the active tab
        let active_tab_str = &tab_strings[active_index];
        let active_len = active_tab_str.chars().count() + 1;

        if active_len >= available {
            // Even the active tab doesn't fit — just render it truncated
            let truncated =
                truncate_to_chars(&tab_strings[active_index], available.saturating_sub(1));
            tab_strings[active_index] = truncated;
            overflow_count = tabs.len() - 1;
        } else {
            let mut used = active_len;
            let mut visible: Vec<bool> = vec![false; tabs.len()];
            visible[active_index] = true;

            // Expand outward from active tab
            let mut left = active_index;
            let mut right = active_index;
            loop {
                let expanded = if left > 0 {
                    let candidate = left - 1;
                    let len = tab_strings[candidate].chars().count() + 1;
                    if used + len <= available {
                        left = candidate;
                        visible[left] = true;
                        used += len;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };

                let expanded_r = if right + 1 < tabs.len() {
                    let candidate = right + 1;
                    let len = tab_strings[candidate].chars().count() + 1;
                    if used + len <= available {
                        right = candidate;
                        visible[right] = true;
                        used += len;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };

                if !expanded && !expanded_r {
                    break;
                }
            }

            overflow_count = visible.iter().filter(|&&v| !v).count();
            // Zero out invisible tabs so we skip them in rendering
            for (i, v) in visible.iter().enumerate() {
                if !*v {
                    tab_strings[i] = String::new();
                }
            }
        }
    }

    // Render tabs into the buffer row
    let y = area.y;
    let mut x = area.x;

    for (i, s) in tab_strings.iter().enumerate() {
        if s.is_empty() {
            continue;
        }

        let style = if i == active_index {
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.colors.fg_secondary)
        };

        for ch in s.chars() {
            if x >= area.x + area.width {
                break;
            }
            buf[(x, y)].set_char(ch).set_style(style);
            x += 1;
        }
        // Space separator
        if x < area.x + area.width {
            buf[(x, y)].set_char(' ').set_style(Style::default());
            x += 1;
        }
    }

    // Render overflow indicator
    if overflow_count > 0 {
        let indicator = format!("+{}", overflow_count);
        let style = Style::default().fg(theme.colors.fg_muted);
        // Right-align within any remaining space
        for ch in indicator.chars() {
            if x >= area.x + area.width {
                break;
            }
            buf[(x, y)].set_char(ch).set_style(style);
            x += 1;
        }
    }

    // Fill rest with spaces
    while x < area.x + area.width {
        buf[(x, y)].set_char(' ').set_style(Style::default());
        x += 1;
    }
}

/// Truncate a string to at most `max_chars` Unicode scalar values.
/// Never splits a multi-byte character.
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    // Truncate and add ellipsis
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tab::TabManager;

    #[test]
    fn test_truncate_to_chars_short() {
        assert_eq!(truncate_to_chars("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_to_chars_exact() {
        assert_eq!(truncate_to_chars("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_to_chars_long() {
        let s = "hello world this is a long title";
        let result = truncate_to_chars(s, 10);
        assert!(result.chars().count() <= 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_multibyte_safe() {
        // Unicode multi-byte chars — should not panic
        let s = "日本語のタイトル";
        let result = truncate_to_chars(s, 5);
        assert!(result.chars().count() <= 5);
    }

    #[test]
    fn test_tab_bar_single_tab() {
        let tm = TabManager::new();
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        let theme = crate::adapters::tui::theme::Theme::for_capability(
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        // Should not panic with single tab
        render_tab_bar(&tm, 0, area, &mut buf, &theme);
    }

    #[test]
    fn test_tab_manager_creates_unique_ids() {
        let mut tm = TabManager::new();
        let id1 = tm.active_tab_id();
        let id2 = tm.create_tab();
        assert_ne!(id1, id2);
    }
}
