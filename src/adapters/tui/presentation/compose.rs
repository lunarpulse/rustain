//! The common paint boundary: toasts, unchanged legacy surfaces, frames, status.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{Surface, geometry};
use crate::adapters::tui::{
    keymap::Host,
    layout::AppLayout,
    notification_log::{self, LogFacts},
    quit_confirm,
    state::TuiState,
    widgets::toast_layer,
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
    _host: Host,
    facts: &LogFacts,
    paint_legacy: impl FnOnce(&mut Frame),
    paint_status: impl FnOnce(&mut Frame),
) {
    let theme = &state.theme;
    toast_layer::render(frame, shell.chat_pane, &state.notifications, theme, facts);
    paint_legacy(frame);

    // Both A1 frames clear the same rectangle. Dim the base once, not once per
    // frame; a lower frame is wholly covered by the next frame's own clear.
    if !state.modal_stack.is_empty() {
        for area in [shell.region, shell.input_area]
            .into_iter()
            .chain(shell.sidebar)
        {
            frame
                .buffer_mut()
                .set_style(area, Style::default().add_modifier(Modifier::DIM));
        }
    }
    let rect = geometry::frame_rect(shell.region);
    for (depth, modal) in state.modal_stack.iter().enumerate() {
        match modal.surface {
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
                Surface::NotificationLog => theme.colors.accent,
                Surface::QuitConfirm => theme.colors.warning,
            };
            // Block bounds the styled title to the border's available cells;
            // repainting the chrome leaves the fitted body untouched.
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border))
                    .title(Line::from(title))
                    .style(Style::default().bg(theme.colors.bg_surface)),
                rect,
            );
        }
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
