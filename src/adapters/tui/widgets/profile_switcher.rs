use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::modal::{ModalPart, fit_modal, wrapped};
use crate::adapters::tui::state::ProfileSwitcherState;
use crate::adapters::tui::theme::Theme;
use crate::domain::services::swap_tier::SwapTier;

pub fn render(frame: &mut Frame, area: Rect, state: &ProfileSwitcherState, theme: &Theme) {
    frame.render_widget(Clear, area);

    if state.preview.is_some() {
        render_preview(frame, area, state, theme);
    } else {
        render_list(frame, area, state, theme);
    }
}

fn render_list(frame: &mut Frame, area: Rect, state: &ProfileSwitcherState, theme: &Theme) {
    let block = Block::default()
        .title(Span::styled(" Profile Switcher ", theme.typography.heading))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .style(Style::default().bg(theme.colors.bg_surface));

    let mut selected = (0, 0);
    let items = state
        .profiles
        .iter()
        .enumerate()
        .map(|(i, profile)| {
            let is_active = state.active_index == Some(i);
            let active_marker = if is_active { "● " } else { "  " };
            let name = if profile.preview {
                format!("{} (preview)", profile.name)
            } else {
                profile.name.clone()
            };
            let desc = profile.description.as_deref().unwrap_or("");

            let swatch_color = profile.identity_color.0;

            let line = Line::from(vec![
                Span::raw(if i == state.selected { "> " } else { "  " }),
                Span::raw(active_marker),
                Span::styled("█", Style::default().fg(Color::Indexed(swatch_color))),
                Span::raw(" "),
                Span::raw(name),
                Span::styled(
                    format!("  {}", desc),
                    Style::default().fg(theme.colors.fg_muted),
                ),
            ]);

            if i == state.selected {
                selected = (i, i + 1);
                line.style(
                    Style::default()
                        .fg(theme.colors.accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                line
            }
        })
        .collect();

    // Ports-that-differ summary for selected
    let footer = if let Some(selected_profile) = state.selected_profile() {
        if let Some(ref plan) = state.compute_diff_for_selected(selected_profile) {
            if plan.diffs.is_empty() {
                "No differences — already active".to_string()
            } else {
                let names: Vec<&str> = plan.diffs.iter().map(|d| d.port_name()).collect();
                format!(
                    "Δ {} — {}/7 ports change",
                    names.join(", "),
                    plan.diffs.len()
                )
            }
        } else {
            "Unable to compute diff".to_string()
        }
    } else {
        String::new()
    };

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = fit_modal(
        vec![
            ModalPart::Rows {
                lines: items,
                selected,
            },
            ModalPart::Footer(wrapped(
                &footer,
                Style::default().fg(theme.colors.fg_muted),
                inner.width as usize,
            )),
        ],
        inner.height as usize,
        inner.width as usize,
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_preview(frame: &mut Frame, area: Rect, state: &ProfileSwitcherState, theme: &Theme) {
    let plan = match state.preview.as_ref() {
        Some(p) => p,
        None => return,
    };

    let block = Block::default()
        .title(Span::styled(
            format!(" Preview: {} ", plan.profile_name),
            theme.typography.heading,
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.colors.accent))
        .style(Style::default().bg(theme.colors.bg_surface));

    let mut lines: Vec<Line> = Vec::new();

    // Section 1: Hot swaps
    let hot: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Hot)
        .collect();
    if !hot.is_empty() {
        lines.push(Line::from(Span::styled(
            "▸ Hot swaps (< 10ms — imperceptible):",
            Style::default().fg(theme.colors.warning),
        )));
        for diff in &hot {
            lines.push(Line::from(format!(
                "  {}: {} → {}",
                diff.port_name(),
                diff.from_adapter,
                diff.to_adapter
            )));
        }
        lines.push(Line::from(""));
    }

    // Section 2: Warm swaps
    let warm: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Warm)
        .collect();
    if !warm.is_empty() {
        lines.push(Line::from(Span::styled(
            "▸ Warm swaps (< 5s — brief pause):",
            Style::default().fg(theme.colors.info),
        )));
        for diff in &warm {
            lines.push(Line::from(format!(
                "  {}: {} → {}  [policy: {:?}]",
                diff.port_name(),
                diff.from_adapter,
                diff.to_adapter,
                diff.policy
            )));
        }
        lines.push(Line::from(""));
    }

    // Section 3: Cold swaps
    let cold: Vec<_> = plan
        .diffs
        .iter()
        .filter(|d| d.tier == SwapTier::Cold)
        .collect();
    if !cold.is_empty() {
        lines.push(Line::from(Span::styled(
            "▸ Cold swaps (< 2s — adapter loops restart):",
            Style::default().fg(theme.colors.profile_personal),
        )));
        for diff in &cold {
            lines.push(Line::from(format!(
                "  {}: {} → {}",
                diff.port_name(),
                diff.from_adapter,
                diff.to_adapter
            )));
        }
        lines.push(Line::from(""));
    }

    // Section 4: No changes
    if plan.diffs.is_empty() {
        lines.push(Line::from("No changes — already active"));
        lines.push(Line::from(""));
    }

    // Footer
    let footer = wrapped(
        &format!(
            "Estimated time: ~{}ms  |  [{}] confirm  [{}] cancel",
            plan.estimated_ms,
            crate::adapters::tui::keymap::render::key_text(
                crate::adapters::tui::keymap::table_overlay::OverlayCmd::ProfileYes
            ),
            crate::adapters::tui::keymap::render::key_text(
                crate::adapters::tui::keymap::table_overlay::OverlayCmd::ProfileBack
            )
        ),
        Style::default().fg(theme.colors.fg_muted),
        area.width.saturating_sub(2) as usize,
    );

    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = lines
        .into_iter()
        .flat_map(|line| {
            let style = line
                .spans
                .first()
                .map_or(Style::default(), |span| span.style);
            wrapped(&line.to_string(), style, inner.width as usize)
        })
        .collect();
    let lines = fit_modal(
        vec![
            ModalPart::Rows {
                lines,
                selected: (0, 1),
            },
            ModalPart::Footer(footer),
        ],
        inner.height as usize,
        inner.width as usize,
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        ProfileDescriptor, ProfileIdentityColor, ProfileSelection, ProfileSource,
    };
    use crate::domain::services::swap_tier::TransitionPlan;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn story21_4_profile_selection_and_preview_footer_fit_caller_sheet() {
        let mut state = ProfileSwitcherState::new();
        state.initialize(
            (0..8)
                .map(|i| ProfileDescriptor {
                    name: format!("profile-{i}"),
                    description: None,
                    preview: false,
                    identity_color: ProfileIdentityColor(4),
                    source: ProfileSource::Builtin,
                    source_origin: None,
                    selection: ProfileSelection::default(),
                })
                .collect(),
            "profile-7",
            Default::default(),
        );
        for (width, height) in [(60, 16), (130, 30)] {
            for preview in [false, true] {
                state.preview = preview.then(|| {
                    TransitionPlan::from_selections(
                        &Default::default(),
                        &Default::default(),
                        "profile-7",
                        4,
                        None,
                    )
                });
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let theme = Theme::dark();
                terminal
                    .draw(|frame| {
                        let layout = crate::adapters::tui::layout::compute_layout(
                            frame.area(),
                            &theme,
                            "1\n2\n3\n4\n5\n6\n7\n8",
                            1,
                            false,
                        )
                        .unwrap();
                        let region = crate::adapters::tui::presentation::geometry::modal_region(
                            &layout,
                            frame.area(),
                        );
                        let sheet =
                            crate::adapters::tui::presentation::geometry::frame_rect(region);
                        render(frame, sheet, &state, &theme);
                        assert_eq!(frame.buffer_mut()[(sheet.x, sheet.y)].symbol(), "┌");
                        assert_eq!(
                            frame.buffer_mut()[(sheet.right() - 1, sheet.bottom() - 1)].symbol(),
                            "┘"
                        );
                    })
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                if preview {
                    assert!(text.contains("No changes — already active"), "{text}");
                    assert!(text.contains("[y] confirm"), "{text}");
                    assert!(text.contains("[Esc] cancel"), "{text}");
                } else {
                    assert!(text.contains("> ● █ profile-7"), "{text}");
                    assert!(text.contains("Unable to compute diff"), "{text}");
                }
            }
        }
    }
}
