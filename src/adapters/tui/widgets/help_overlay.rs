use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::sync::LazyLock;

use super::modal::{ModalPart, fit_modal};
use crate::adapters::tui::keymap::render_help::{HelpGroup, help_groups, intro, tmux_conflicts};
use crate::adapters::tui::keymap::{self, Context, table_overlay::OverlayCmd};
use crate::adapters::tui::presentation::inspector_search::InspectorSearch;
use crate::adapters::tui::state::HelpOverlayState;
use crate::adapters::tui::theme::Theme;

// Generated, host-neutral build metadata: no query, frame or caller state.
// Navigation counts and paint share it without rebuilding the catalogue.
static HELP_GROUPS: LazyLock<Vec<HelpGroup>> = LazyLock::new(help_groups);

pub(crate) fn filtered_row_count(search: &InspectorSearch) -> usize {
    HELP_GROUPS
        .iter()
        .flat_map(|group| &group.rows)
        .filter(|binding| search.matches(&binding.key) || search.matches(&binding.description))
        .count()
        .max(1) // The empty-result message occupies one row too.
}

/// Render the help content into the frame rectangle supplied by compose.
// Covers: FR108, UX-DR94
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &HelpOverlayState,
    theme: &Theme,
    tmux_detected: bool,
    show_caret: bool,
) {
    let query_visible = state.search.editing || !state.search.query.is_empty();
    let footer = if state.search.editing {
        format!(
            " {} Keep filter  {} Clear ",
            keymap::render::rail(Context::InspectorSearch, OverlayCmd::InspectorAccept),
            keymap::render::rail(Context::InspectorSearch, OverlayCmd::InspectorBack),
        )
    } else {
        format!(
            " j/k scroll  ? close  {} Search ",
            keymap::render::rail(Context::Help, OverlayCmd::InspectorSearch),
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .title(Span::styled(
            " Help — Rustain Keybindings ",
            theme.typography.heading,
        ))
        .title_bottom(Span::styled(
            footer,
            Style::default().fg(theme.colors.fg_muted),
        ));
    let mut inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(
        block.style(Style::default().bg(theme.colors.bg_secondary)),
        area,
    );
    if query_visible && inner.height > 0 {
        frame.render_widget(
            Paragraph::new(
                state
                    .search
                    .query_line(usize::from(inner.width), theme, show_caret),
            ),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        inner.y += 1;
        inner.height -= 1;
    }
    let offset = if state.search.query.is_empty() {
        state.scroll_offset
    } else {
        state.filtered_scroll_offset
    };
    let content_area = Rect::new(area.x, area.y, area.width, inner.height.saturating_add(2));
    let content_lines =
        build_content_lines(theme, tmux_detected, offset, content_area, &state.search);
    let content_lines = fit_modal(
        vec![ModalPart::Rows {
            lines: content_lines,
            selected: (0, 0),
        }],
        usize::from(inner.height),
        usize::from(inner.width),
    );
    frame.render_widget(
        Paragraph::new(content_lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_secondary),
        ),
        inner,
    );
}

/// Build the text content for the overlay, applying scroll_offset to skip lines at the top.
fn build_content_lines(
    theme: &Theme,
    tmux_detected: bool,
    scroll_offset: usize,
    area: Rect,
    search: &InspectorSearch,
) -> Vec<Line<'static>> {
    let mut all_lines: Vec<Line> = Vec::new();

    let filtering = !search.query.is_empty();
    if !filtering {
        all_lines.push(Line::default());
        for intro_line in wrap_text(&intro(), area.width.saturating_sub(4) as usize) {
            all_lines.push(Line::from(Span::styled(
                intro_line,
                Style::default().fg(theme.colors.fg_secondary),
            )));
        }
        all_lines.push(Line::default());
    }

    // Keybinding categories
    for group in HELP_GROUPS.iter() {
        if !filtering {
            all_lines.push(Line::from(Span::styled(
                group.name,
                theme.typography.heading.fg(theme.colors.accent),
            )));
        }

        for binding in &group.rows {
            if !search.matches(&binding.key) && !search.matches(&binding.description) {
                continue;
            }
            let key_style = if binding.available {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::DIM)
            };
            let desc_style = if binding.available {
                Style::default().fg(theme.colors.fg_primary)
            } else {
                Style::default()
                    .fg(theme.colors.fg_muted)
                    .add_modifier(Modifier::DIM)
            };

            all_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<14}", binding.key), key_style),
                Span::styled(binding.description.as_str(), desc_style),
            ]));
        }
        if !filtering {
            all_lines.push(Line::default());
        }
    }

    // tmux conflict notice
    if tmux_detected && !filtering {
        all_lines.push(Line::from(Span::styled(
            "⚠ tmux detected — some shortcuts may conflict with your tmux prefix.",
            Style::default().fg(theme.colors.warning),
        )));
        all_lines.push(Line::from(Span::styled(
            "  Alternatives shown where available:",
            Style::default().fg(theme.colors.fg_secondary),
        )));
        for conflict in tmux_conflicts() {
            let alt = conflict.alternative.unwrap_or("—");
            all_lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("{:<12}", conflict.key),
                    Style::default().fg(theme.colors.warning),
                ),
                Span::styled(
                    format!("conflicts with {}  →  {}", conflict.conflict_with, alt),
                    Style::default().fg(theme.colors.fg_secondary),
                ),
            ]));
        }
        all_lines.push(Line::default());
    }
    if all_lines.is_empty() {
        all_lines.push(Line::from(Span::styled(
            "No matching keybindings.",
            Style::default().fg(theme.colors.fg_muted),
        )));
    }

    // Apply scroll offset
    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    let max_offset = all_lines.len().saturating_sub(inner_height);
    let actual_offset = scroll_offset.min(max_offset);
    all_lines.into_iter().skip(actual_offset).collect()
}

/// Split `text` into lines of at most `max_width` characters (simple word-wrap).
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= max_width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current.clone());
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::adapters::tui::state::HelpOverlayState;
    use crate::adapters::tui::theme::Theme;

    fn make_theme() -> Theme {
        Theme::dark()
    }

    fn make_state() -> HelpOverlayState {
        HelpOverlayState::new()
    }

    fn inspector_press(
        state: &mut crate::adapters::tui::state::TuiState,
        key: crate::domain::events::DomainKey,
    ) {
        crate::adapters::tui::app::handle_input(
            state,
            &crate::domain::events::DomainInputEvent::SpecialKey(key),
        );
    }

    fn inspector_paint(state: &crate::adapters::tui::state::TuiState) -> String {
        use crate::adapters::tui::{
            keymap::Host, layout::compute_layout, notification_log::LogFacts, presentation,
        };
        use crate::domain::models::visual::DensityMode;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let layout = compute_layout(
                    frame.area(),
                    &state.theme,
                    &state.input_buffer,
                    1,
                    false,
                    DensityMode::Focus,
                )
                .unwrap();
                presentation::compose(
                    frame,
                    state,
                    &presentation::Shell::new(frame.area(), &layout),
                    Host::Local,
                    &LogFacts::default(),
                    &mut presentation::occlusion::OpaqueRects::new(layout.chat_pane),
                    |_| {},
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn story21_4_help_inspector_real_input_filters_without_losing_unfiltered_scroll() {
        use crate::adapters::tui::{app::handle_input, presentation::Surface, state::TuiState};
        use crate::domain::events::{DomainInputEvent, DomainKey};
        let mut state = TuiState::new(80, 24);
        inspector_press(&mut state, DomainKey::CtrlX);
        handle_input(&mut state, &DomainInputEvent::KeyPress('?'));
        for _ in 0..4 {
            handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
        }
        state.help_overlay.selected_category = 2;
        let saved_scroll = state.help_overlay.scroll_offset;
        assert!(saved_scroll > 0);
        let unfiltered = inspector_paint(&state);
        handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
        for ch in "wheel_lines".chars() {
            handle_input(&mut state, &DomainInputEvent::KeyPress(ch));
        }
        let filtered = inspector_paint(&state);
        assert!(filtered.contains("Scroll by wheel_lines"));
        assert!(!filtered.contains("Rustain is a composable"));
        assert_ne!(filtered, unfiltered);
        assert_eq!(state.help_overlay.scroll_offset, saved_scroll);
        assert_eq!(state.help_overlay.selected_category, 2);
        inspector_press(&mut state, DomainKey::CtrlU);
        handle_input(
            &mut state,
            &DomainInputEvent::Paste("unmatched-help-needle".into()),
        );
        assert!(inspector_paint(&state).contains("No matching keybindings"));
        inspector_press(&mut state, DomainKey::Esc);
        assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
        assert_eq!(state.help_overlay.scroll_offset, saved_scroll);
        assert_eq!(state.help_overlay.selected_category, 2);
        assert_eq!(inspector_paint(&state), unfiltered);
        inspector_press(&mut state, DomainKey::Esc);
        assert!(state.modal_stack.is_empty());
    }

    #[test]
    fn test_render_no_crash_when_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state();
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, false, false);
            })
            .unwrap();
    }

    #[test]
    fn test_render_no_crash_with_tmux() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state();
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, true, false);
            })
            .unwrap();
    }

    #[test]
    fn test_render_no_crash_small_terminal() {
        // Minimum terminal size: 80x24
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = make_state();
        let theme = make_theme();
        terminal
            .draw(|frame| {
                render(frame, frame.area(), &state, &theme, false, false);
            })
            .unwrap();
    }

    #[test]
    fn test_render_uses_caller_area_without_ownership_flags() {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let area = Rect::new(7, 3, 80, 20);
        terminal
            .draw(|frame| render(frame, area, &make_state(), &make_theme(), false, false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(area.x, area.y)].symbol(), "┌");
        assert_eq!(buffer[(area.right() - 1, area.bottom() - 1)].symbol(), "┘");
        assert_eq!(buffer[(0, 0)].symbol(), " ");
    }

    #[test]
    fn test_wrap_text_short() {
        let lines = wrap_text("hello world", 80);
        assert_eq!(lines, vec!["hello world"]);
    }

    #[test]
    fn test_wrap_text_wraps() {
        let lines = wrap_text("a b c d e", 5);
        // each word is 1 char; "a b c" = 5, "d e" = 3
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_shared_geometry_small_and_wide_help_fit() {
        use crate::adapters::tui::presentation::geometry;

        for region in [Rect::new(0, 0, 60, 5), Rect::new(0, 0, 130, 30)] {
            let area = geometry::frame_rect(region);
            let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
            terminal
                .draw(|frame| render(frame, area, &make_state(), &make_theme(), false, false))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            assert!(row(area.y).contains("Help — Rustain Keybindings"));
            assert!(row(area.bottom() - 1).contains("j/k scroll  ? close"));
            assert!(
                (area.y + 1..area.bottom() - 1)
                    .any(|y| { row(y).chars().any(|c| c.is_alphanumeric()) })
            );
        }
    }

    #[test]
    fn inspector_help_matches_keys_and_descriptions_but_not_category_or_intro() {
        let theme = make_theme();
        let area = Rect::new(0, 0, 80, 24);
        let mut search = InspectorSearch::default();
        for query in ["Mouse wheel", "WHEEL_LINES"] {
            search.query.set(query);
            let lines = build_content_lines(&theme, true, 0, area, &search);
            let text = lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Scroll by wheel_lines"));
            assert!(!text.contains("Rustain is a composable"));
            assert!(!text.contains("tmux detected"));
            assert!(lines.iter().all(|line| {
                let text = line.to_string();
                text.contains("wheel") || text.contains("Wheel")
            }));
        }
        search.query.set("Rustain is a composable");
        let lines = build_content_lines(&theme, false, 0, area, &search);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "No matching keybindings.");
    }

    #[test]
    fn inspector_help_filtered_scroll_is_separate_and_narrow_paint_starts_with_a_match() {
        let theme = make_theme();
        let mut state = make_state();
        state.scroll_offset = 17;
        state.selected_category = 3;
        state.search.query.set("notification");
        state.search.open();
        let mut terminal = Terminal::new(TestBackend::new(60, 5)).unwrap();
        for show_caret in [true, false] {
            terminal
                .draw(|frame| render(frame, frame.area(), &state, &theme, false, show_caret))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            assert!(row(1).contains("/ notification"));
            assert!(row(2).to_lowercase().contains("notification"));
            assert!(row(4).contains("Keep filter"));
            assert!(row(4).contains("Clear"));
            assert!(!row(4).contains("j/k scroll"));
            assert_eq!(
                buffer[(15, 1)].modifier.contains(Modifier::REVERSED),
                show_caret
            );
        }
        assert_eq!(state.scroll_offset, 17);
        assert_eq!(state.selected_category, 3);
        state.search.query.set("scroll");
        let all = build_content_lines(&theme, false, 0, Rect::new(0, 0, 60, 5), &state.search);
        assert!(all.len() > 4);
        state.filtered_scroll_offset = 2;
        let scrolled = build_content_lines(
            &theme,
            false,
            state.filtered_scroll_offset,
            Rect::new(0, 0, 60, 5),
            &state.search,
        );
        assert_eq!(scrolled[0], all[2]);
        assert_eq!(state.scroll_offset, 17);
        state.search.back();
        assert_eq!(state.scroll_offset, 17);
        assert_eq!(state.selected_category, 3);
    }
}
