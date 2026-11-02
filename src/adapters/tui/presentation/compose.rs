//! The common paint boundary: toasts, unchanged legacy surfaces, frames, status.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::{Surface, geometry};
use crate::adapters::tui::{
    keymap::Host,
    layout::AppLayout,
    notification_log::{self, LogFacts},
    quit_confirm,
    state::TuiState,
    widgets::{command_palette, help_overlay, toast_layer, which_key_bar},
};

/// Snapshot the unreserved layout before legacy search/bookmark reservations.
/// Painting frames never changes any of the host's base rectangles.
#[derive(Debug, Clone, Copy)]
pub struct Shell {
    pub area: Rect,
    pub region: Rect,
    pub chat_pane: Rect,
    pub input_area: Rect,
    pub status_bar: Rect,
    sidebar: Option<Rect>,
}

impl Shell {
    #[must_use]
    pub fn new(area: Rect, layout: &AppLayout) -> Self {
        Self {
            area,
            region: geometry::modal_region(layout, area),
            chat_pane: layout.chat_pane,
            input_area: layout.input_area,
            status_bar: layout.status_bar,
            sidebar: layout.sidebar,
        }
    }
}

/// Hosts paint the base first and suppress its cursor whenever the stack owns
/// presence. Legacy surfaces keep their current geometry in `paint_legacy`.
pub fn compose(
    frame: &mut Frame,
    state: &TuiState,
    shell: &Shell,
    host: Host,
    facts: &LogFacts,
    paint_legacy: impl FnOnce(&mut Frame),
    paint_status: impl FnOnce(&mut Frame),
) {
    let theme = &state.theme;
    toast_layer::render(frame, shell.chat_pane, &state.notifications, theme, facts);
    paint_legacy(frame);

    let rect = geometry::frame_rect(shell.region);
    for (depth, modal) in state.modal_stack.iter().enumerate() {
        for area in [shell.region, shell.input_area]
            .into_iter()
            .chain(shell.sidebar)
        {
            frame
                .buffer_mut()
                .set_style(area, Style::default().add_modifier(Modifier::DIM));
        }
        match modal.surface {
            Surface::Help => help_overlay::render(
                frame,
                rect,
                &state.help_overlay,
                theme,
                state.multiplexer_detected,
            ),
            Surface::CommandPalette => {
                command_palette::render(frame, rect, &state.command_palette, theme)
            }
            Surface::NotificationLog => notification_log::render(
                frame,
                rect,
                state
                    .notification_log
                    .as_ref()
                    .expect("Log frame has its payload"),
                &state.notifications,
                theme,
                facts,
            ),
            Surface::QuitConfirm => quit_confirm::render(
                frame,
                rect,
                state
                    .quit_confirm
                    .as_ref()
                    .expect("Quit frame has its payload"),
                theme,
            ),
        }
        if depth > 0 {
            let ancestor = Style::default().fg(theme.colors.fg_secondary);
            let current = Style::default().fg(theme.colors.fg_primary);
            let mut title = Vec::with_capacity(depth * 2 + 3);
            title.push(Span::raw(" "));
            for prior in state.modal_stack.iter().take(depth) {
                title.push(Span::styled(prior.surface.title(), ancestor));
                title.push(Span::styled(" › ", ancestor));
            }
            title.push(Span::styled(modal.surface.title(), current));
            title.push(Span::raw(" "));
            let border = match modal.surface {
                Surface::NotificationLog | Surface::Help | Surface::CommandPalette => {
                    theme.colors.accent
                }
                Surface::QuitConfirm => theme.colors.warning,
            };
            // Replace only the top rail: redrawing every border would erase
            // widget-owned closing-border titles such as Help's scroll rail.
            let top = Rect::new(rect.x + 1, rect.y, rect.width.saturating_sub(2), 1);
            for x in top.x..top.right() {
                let cell = &mut frame.buffer_mut()[(x, top.y)];
                cell.reset();
                cell.set_symbol("─")
                    .set_style(Style::default().fg(border).bg(theme.colors.bg_surface));
            }
            frame.render_widget(Line::from(title), top);
        }
    }
    if state.pending_leader.is_some()
        && state
            .modal_stack
            .top()
            .is_none_or(|frame| frame.surface != Surface::QuitConfirm)
    {
        let mut avail = match host {
            Host::Local => crate::adapters::tui::app::local_avail_ctx(state),
            Host::Attached => {
                crate::adapters::tui::keymap::AvailCtx::attached(state.terminal_width, false)
            }
        };
        avail.frame_origin = !state.modal_stack.is_empty();
        which_key_bar::render(
            frame,
            shell.region,
            theme,
            &avail,
            state.leader_feedback.as_ref(),
        );
    }
    paint_status(frame);
}

/// Both hosts use exactly the same minimum-terminal message.
pub fn paint_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("Terminal too small (min 60x16)")
            .style(Style::default().fg(Color::Red))
            .alignment(Alignment::Center),
        area,
    );
}
