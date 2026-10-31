use ratatui::prelude::*;

use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::domain::models::tab::TabManager;

/// Maximum display cells per tab title (marker included) before truncation.
const MAX_TITLE_CELLS: usize = 20;

/// Render the tab bar showing all open tabs.
///
/// Active tab is highlighted with the theme accent color.
/// Individual titles are grapheme-boundary truncated to a cell budget (never
/// byte- or scalar-sliced) and painted with width-aware buffer operations.
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
            let base_title = if t.conversation.title.is_empty() {
                format!("Tab {}", i + 1)
            } else {
                truncate_to_cells(&t.conversation.title, MAX_TITLE_CELLS)
            };
            if t.read_only {
                format!("[\u{1F441} {}]", base_title)
            } else {
                format!("[{}]", base_title)
            }
        })
        .collect();

    let width = area.width as usize;

    // Check if everything fits (plus separator spaces)
    let total_len: usize = tab_strings.iter().map(|s| geom::cells(s) + 1).sum();

    let mut overflow_count = 0;
    if total_len > width {
        // We need to hide some tabs. Always keep active tab visible.
        // Strategy: greedily include tabs from active outward until we run out of space.
        // Reserve space for the "+N" indicator (up to 5 chars: " +99 ").
        let indicator_reserve = 5;
        let available = width.saturating_sub(indicator_reserve);

        // Compute which tabs fit around the active tab
        let active_tab_str = &tab_strings[active_index];
        let active_len = geom::cells(active_tab_str) + 1;

        if active_len >= available {
            // Even the active tab doesn't fit — just render it truncated
            let truncated =
                truncate_to_cells(&tab_strings[active_index], available.saturating_sub(1));
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
                    let len = geom::cells(&tab_strings[candidate]) + 1;
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
                    let len = geom::cells(&tab_strings[candidate]) + 1;
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

        // Width-aware paint: whole graphemes only, wide glyph continuation
        // cells reset, nothing painted past the row's right edge.
        let right = area.x + area.width;
        x = buf
            .set_stringn(x, y, s, usize::from(right.saturating_sub(x)), style)
            .0;
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
        let right = area.x + area.width;
        x = buf
            .set_stringn(
                x,
                y,
                &indicator,
                usize::from(right.saturating_sub(x)),
                style,
            )
            .0;
    }

    // Fill rest with spaces
    while x < area.x + area.width {
        buf[(x, y)].set_char(' ').set_style(Style::default());
        x += 1;
    }
}

/// Truncate a string to at most `max_cells` display cells, ending in `…`
/// (inside the budget) when it had to be cut. Never splits a grapheme cluster.
fn truncate_to_cells(s: &str, max_cells: usize) -> String {
    geom::ellipsize(s, max_cells, "…").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tab::TabManager;

    #[test]
    fn test_truncate_to_cells_short() {
        assert_eq!(truncate_to_cells("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_to_cells_exact() {
        assert_eq!(truncate_to_cells("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_to_cells_long() {
        let s = "hello world this is a long title";
        let result = truncate_to_cells(s, 10);
        assert!(geom::cells(&result) <= 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_truncate_multibyte_safe() {
        // Unicode multi-byte chars — should not panic
        let s = "日本語のタイトル";
        let result = truncate_to_cells(s, 5);
        assert!(geom::cells(&result) <= 5);
    }

    #[test]
    fn test_tab_bar_single_tab() {
        let tm = TabManager::default();
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
        let mut tm = TabManager::default();
        let id1 = tm.active_tab_id();
        let id2 = tm.create_tab();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_tab_bar_with_read_only_tab() {
        let mut tm = TabManager::default();
        tm.create_tab();
        tm.active_tab_mut().read_only = true;
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        let theme = crate::adapters::tui::theme::Theme::for_capability(
            crate::adapters::tui::color_detect::ColorCapability::TrueColor,
        );
        render_tab_bar(&tm, 1, area, &mut buf, &theme);
        let mut text = String::new();
        for x in 0..area.width {
            text.push_str(buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "));
        }
        assert!(
            text.contains("\u{1F441}"),
            "read-only tab should contain eye glyph: {text}"
        );
    }

    // ── Story 21.11 AC4/K4: display-width correctness ──────────────────────
    // ASCII is the positive control; C/G/A (2/2/1 cells under the pinned
    // string-width policy) exercise the cut, the painter and the totals.

    const C: &str = "界";
    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";

    fn theme() -> Theme {
        Theme::for_capability(crate::adapters::tui::color_detect::ColorCapability::TrueColor)
    }

    fn tabs_with(specs: &[(&str, bool)]) -> TabManager {
        let mut tm = TabManager::default();
        for _ in 1..specs.len() {
            tm.create_tab();
        }
        for (tab, (title, read_only)) in tm.tabs_mut().iter_mut().zip(specs) {
            tab.conversation.title = (*title).to_string();
            tab.read_only = *read_only;
        }
        tm
    }

    fn draw(tm: &TabManager, active: usize, width: u16) -> Buffer {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        render_tab_bar(tm, active, area, &mut buf, &theme());
        buf
    }

    /// The row as painted glyphs: wide-glyph continuation cells are skipped.
    fn glyph_row(buf: &Buffer) -> String {
        let mut out = String::new();
        let mut x = 0u16;
        while x < buf.area.width {
            let symbol = buf.cell((x, 0)).unwrap().symbol();
            out.push_str(symbol);
            x += u16::try_from(geom::cells(symbol).max(1)).unwrap();
        }
        out
    }

    #[test]
    fn display_width_tab_title_clips_at_cell_budget() {
        let cases: Vec<(String, String)> = vec![
            // ASCII control: 25 cells clip to 19 + marker inside the 20 budget.
            ("x".repeat(25), format!("[{}…]", "x".repeat(19))),
            // C: exact fit (20 cells), fit-minus-one, and one over.
            (C.repeat(10), format!("[{}]", C.repeat(10))),
            (format!("{}x", C.repeat(9)), format!("[{}x]", C.repeat(9))),
            (format!("{}x", C.repeat(10)), format!("[{}…]", C.repeat(9))),
            (C.repeat(20), format!("[{}…]", C.repeat(9))),
            // G: whole families only at the cut.
            (G.repeat(10), format!("[{}]", G.repeat(10))),
            (format!("{}x", G.repeat(10)), format!("[{}…]", G.repeat(9))),
            // A: one-cell clusters keep their accent.
            (A.repeat(20), format!("[{}]", A.repeat(20))),
            (A.repeat(21), format!("[{}…]", A.repeat(19))),
        ];
        for (title, expected) in cases {
            let tm = tabs_with(&[(title.as_str(), false)]);
            let buf = draw(&tm, 0, 60);
            assert_eq!(glyph_row(&buf).trim_end(), expected, "title {title:?}");
        }
    }

    #[test]
    fn display_width_tab_hidden_count_uses_cells_at_the_boundary() {
        // Each title clips to its 20-cell budget (marker inside), so the two
        // segments' total is measured from the unconstrained row.
        for title in [C.repeat(20), "x".repeat(25), G.repeat(12)] {
            let tm = tabs_with(&[(title.as_str(), false), (title.as_str(), false)]);
            let one = glyph_row(&draw(&tm, 1, 60));
            let both = one.trim_end();
            assert_eq!(both.matches('[').count(), 2, "{title:?}: {both}");
            assert!(!both.contains('+'), "{title:?}: {both}");
            // Every tab accounts for its trailing separator cell.
            let needed = u16::try_from(geom::cells(both) + 1).unwrap();
            // Exact fit: nothing hidden.
            let fit = glyph_row(&draw(&tm, 1, needed));
            assert!(!fit.contains('+'), "{title:?}: {fit}");
            assert_eq!(fit.matches('[').count(), 2, "{title:?}: {fit}");
            // One cell short: the inactive tab is hidden, the active title
            // stays, and the count says one.
            let short = glyph_row(&draw(&tm, 1, needed - 1));
            assert_eq!(short.matches('[').count(), 1, "{title:?}: {short}");
            assert!(short.trim_end().ends_with(" +1"), "{title:?}: {short}");
            assert!(short.starts_with('['), "{title:?}: {short}");
        }
    }
}
