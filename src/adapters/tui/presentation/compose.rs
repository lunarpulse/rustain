//! The common paint boundary: toasts, frames, routed popups, status.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use super::{Surface, geometry, occlusion::OpaqueRects, stage_search};
use crate::adapters::tui::{
    decision_pane,
    keymap::Host,
    layout::AppLayout,
    notification_log::{self, LogFacts},
    quit_confirm,
    state::TuiState,
    widgets::{
        autocomplete_popup, bookmark_list, command_palette, help_overlay, login_dialog,
        model_selector, profile_switcher, reverse_search, toast_layer, usage_panel, wave_overlay,
        which_key_bar,
    },
};

/// Snapshot the unreserved layout; frame paint never changes base rectangles.
#[derive(Debug, Clone, Copy)]
pub struct Shell<'a> {
    pub area: Rect,
    pub region: Rect,
    pub chat_pane: Rect,
    pub input_area: Rect,
    pub status_bar: Rect,
    pub supports_osc8: bool,
    sidebar: Option<Rect>,
    /// Live active conversation proxy and its owning tab's durable bookmarks.
    pub stage: Option<(&'a crate::domain::models::Conversation, &'a [usize])>,
}

impl<'a> Shell<'a> {
    #[must_use]
    pub fn new(area: Rect, layout: &AppLayout) -> Self {
        Self {
            area,
            region: geometry::modal_region(layout, area),
            chat_pane: layout.chat_pane,
            input_area: layout.input_area,
            status_bar: layout.status_bar,
            supports_osc8: crate::adapters::tui::widgets::hyperlink::terminal_supports_osc8(),
            sidebar: layout.sidebar,
            stage: None,
        }
    }
}

/// Hosts paint the base first and suppress its cursor whenever the stack owns
/// presence. Frame and popup painting belongs to this shared boundary.
pub fn compose(
    frame: &mut Frame,
    state: &TuiState,
    shell: &Shell<'_>,
    host: Host,
    facts: &LogFacts,
    opaque: &mut OpaqueRects,
    paint_status: impl FnOnce(&mut Frame),
) -> bool {
    let theme = &state.theme;
    toast_layer::render(frame, shell.chat_pane, &state.notifications, theme, facts);

    let standard_rect = geometry::frame_rect(shell.region);
    let mut pane_painted = false;
    for (depth, modal) in state.modal_stack.iter().enumerate() {
        let mut rect = standard_rect;
        let show_caret = state.pending_leader.is_none()
            && state
                .modal_stack
                .top()
                .is_some_and(|top| top.id == modal.id);
        for area in (modal.surface != Surface::Search)
            .then_some(shell.region)
            .into_iter()
            .chain([shell.input_area])
            .chain(shell.sidebar)
        {
            frame
                .buffer_mut()
                .set_style(area, Style::default().add_modifier(Modifier::DIM));
        }
        match modal.surface {
            Surface::ToolInspector(key) => crate::adapters::tui::widgets::tool_inspector::render(
                frame,
                rect,
                state
                    .tool_inspectors
                    .get(&key.0)
                    .expect("Tool frame owns its payload"),
                theme,
                modal.title(),
                show_caret,
            ),
            Surface::Search => {
                rect = Rect::new(
                    shell.chat_pane.x,
                    shell.chat_pane.y,
                    shell.chat_pane.width,
                    shell.chat_pane.height.min(1),
                );
                let mut breadcrumb = String::new();
                let label = if depth == 0 {
                    "Search: "
                } else {
                    for prior in state.modal_stack.iter().take(depth) {
                        breadcrumb.push_str(prior.title());
                        breadcrumb.push_str(" › ");
                    }
                    breadcrumb.push_str("Search: ");
                    &breadcrumb
                };
                stage_search::render(frame, rect, &state.search_state, theme, label, show_caret);
            }
            Surface::CrossSearch => crate::adapters::tui::widgets::cross_search::render(
                frame,
                rect,
                &state.cross_search,
                theme,
                show_caret,
            ),
            Surface::BookmarkList => {
                let (conversation, bookmarks) =
                    shell.stage.expect("Bookmark frame paints its owning tab");
                debug_assert_eq!(
                    modal.conversation_id.as_deref(),
                    Some(conversation.id.as_str())
                );
                bookmark_list::render(
                    frame,
                    rect,
                    conversation,
                    bookmarks,
                    state.bookmark_list_selected,
                    theme,
                );
            }
            Surface::WaveOverlay => {
                let snap = state
                    .wave_run
                    .as_ref()
                    .expect("Wave frame has its retained run")
                    .snapshot();
                wave_overlay::render_frame(frame, rect, state, &snap, theme);
            }
            Surface::DecisionPane => {
                let Some(pane_rect) =
                    decision_pane::render(frame, shell.chat_pane, state, facts, theme)
                else {
                    continue;
                };
                rect = pane_rect;
                pane_painted =
                    depth + 1 == state.modal_stack.len() && state.pending_leader.is_none();
            }
            Surface::Help => help_overlay::render(
                frame,
                rect,
                &state.help_overlay,
                theme,
                state.multiplexer_detected,
                show_caret,
            ),
            Surface::CommandPalette => {
                command_palette::render(frame, rect, &state.command_palette, theme, show_caret)
            }
            Surface::ModelSelector => {
                model_selector::render(frame, rect, &state.model_selector, theme)
            }
            Surface::ProfileSwitcher => {
                profile_switcher::render(frame, rect, &state.profile_switcher, theme)
            }
            Surface::UsagePanel => usage_panel::render(frame, rect, &state.usage_panel, theme),
            Surface::Login => {
                login_dialog::render(frame, rect, &state.login, theme, shell.supports_osc8)
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
                show_caret,
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
        opaque.frame(rect);
        if depth > 0 && modal.surface != Surface::Search {
            let ancestor = Style::default().fg(theme.colors.fg_secondary);
            let current = Style::default().fg(theme.colors.fg_primary);
            let mut title = Vec::with_capacity(depth * 2 + 3);
            title.push(Span::raw(" "));
            for prior in state.modal_stack.iter().take(depth) {
                title.push(Span::styled(prior.title(), ancestor));
                title.push(Span::styled(" › ", ancestor));
            }
            title.push(Span::styled(modal.title(), current));
            title.push(Span::raw(" "));
            let border = match modal.surface {
                Surface::NotificationLog
                | Surface::Help
                | Surface::CommandPalette
                | Surface::DecisionPane
                | Surface::ModelSelector
                | Surface::ProfileSwitcher
                | Surface::UsagePanel
                | Surface::Login
                | Surface::ToolInspector(_) => theme.colors.accent,
                Surface::BookmarkList | Surface::WaveOverlay | Surface::CrossSearch => {
                    theme.colors.accent
                }
                Surface::QuitConfirm => theme.colors.warning,
                Surface::Search => unreachable!("Search owns its inline breadcrumb"),
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
    if state.modal_stack.is_empty() && state.focus == crate::domain::models::FocusState::Input {
        opaque.host(autocomplete_popup::render(
            frame,
            shell.input_area,
            &state.autocomplete,
            theme,
        ));
        opaque.host(reverse_search::render(
            frame,
            shell.chat_pane,
            &state.reverse_search,
            theme,
            state.pending_leader.is_none() && !state.autocomplete.is_open(),
        ));
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
        opaque.host(which_key_bar::render(
            frame,
            shell.region,
            theme,
            &avail,
            state.leader_feedback.as_ref(),
        ));
    }
    paint_status(frame);
    pane_painted
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
