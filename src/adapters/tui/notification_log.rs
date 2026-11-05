//! S-LOG: the Notification Log (Story 21.1 AC5, LR-8, A7/A8).
//!
//! One bounded payload with a saved caller focus, a shared widget and a shared
//! key transition used by BOTH hosts. The presentation reducer owns its frame
//! and payload lifetime; this module only edits selection/detail. It
//! never owns a source fact: `x` only hides the selected toast presentation,
//! `c` copies through the existing clipboard boundary (its result is itself a
//! notice) and `r` is offered only on records that carry an eligible action.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notifications::{
    NoticeAction, NoticeRecord, NoticeSeverity, NotificationStore, Presentation, format_hms,
    headline,
};
use crate::adapters::tui::presentation::inspector_search::InspectorSearch;
use crate::adapters::tui::presentation::{self, Surface, reducer};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::{ModalPart, fit_modal};
use crate::domain::models::FocusState;

/// The one open S-LOG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationLogState {
    /// The selected record. Rows are looked up by id on every key and every
    /// paint, so an arrival or an eviction while the log is open never moves
    /// the selection onto another record.
    pub selected_id: Option<u64>,
    /// The selected row, newest first (`0` = newest): the last resolved index,
    /// used as the nearest-row fallback when `selected_id` has left the store.
    pub selected: usize,
    /// `Enter` detail (full message) of the selected row.
    pub detail: bool,
    /// First visible wrapped line of the detail view.
    pub detail_scroll: usize,
    /// Caller focus saved at open time and restored on close.
    pub caller_focus: FocusState,
    /// Retained in-frame filter and its child editor.
    pub search: InspectorSearch,
}

/// Host facts the key handler and the widget need (retry eligibility, A8).
#[derive(Debug, Clone, Default)]
pub struct LogFacts {
    /// The conversation shown by the active tab.
    pub active_conversation_id: Option<String>,
    /// Messages in the active conversation now (a turn retry is bound to the
    /// message count at the failure it was raised for).
    pub active_messages: usize,
    /// A turn is streaming in the active conversation.
    pub turn_running: bool,
    /// The host owns a retry path (the attached client has none).
    pub retry_supported: bool,
    /// `(conversation id, title)` of every owned tab, for attribution.
    pub titles: Vec<(String, String)>,
    /// The host owns the interim decision pane (the attached client does not).
    pub decisions_supported: bool,
    /// The active conversation's context warning is still in force
    /// (`context_warn_level != None`).
    pub context_warning: bool,
}

impl LogFacts {
    /// Display name of an owned conversation (its title, else a short id).
    #[must_use]
    pub fn title_of(&self, conversation_id: &str) -> String {
        self.titles
            .iter()
            .find(|(id, _)| id == conversation_id)
            .map(|(_, title)| title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| conversation_id.chars().take(8).collect())
    }
}

/// True when `r` may act on `record` (A8): a turn-failure Error of the ACTIVE
/// conversation with no running turn whose failed turn is still the last one
/// (the conversation holds exactly the messages it held at the failure), or a
/// compaction failure of the active conversation.
#[must_use]
pub fn retry_eligible(record: &NoticeRecord, facts: &LogFacts) -> bool {
    if !facts.retry_supported {
        return false;
    }
    let Some(action) = record.action else {
        return false;
    };
    let active = facts.active_conversation_id.is_some()
        && record.conversation_id == facts.active_conversation_id;
    match action {
        NoticeAction::RetryTurn { messages } => {
            active && !facts.turn_running && messages == facts.active_messages
        }
        NoticeAction::RetryCompaction => active,
        // The context-warning record only raises decisions (`K`/`n`), never retries.
        NoticeAction::ContextDecision => false,
    }
}

/// True when `K` may raise the Compact decision (A7, AC11): a context-warning
/// record of the ACTIVE conversation while its warning is still in force.
#[must_use]
pub fn compact_eligible(record: &NoticeRecord, facts: &LogFacts) -> bool {
    facts.decisions_supported
        && facts.context_warning
        && record.action == Some(NoticeAction::ContextDecision)
        && facts.active_conversation_id.is_some()
        && record.conversation_id == facts.active_conversation_id
}

/// True when `n` may raise the Start-fresh (carryover) decision: the same
/// context-warning record, or a compaction failure of the active conversation.
#[must_use]
pub fn fresh_eligible(record: &NoticeRecord, facts: &LogFacts) -> bool {
    compact_eligible(record, facts)
        || (facts.decisions_supported
            && record.action == Some(NoticeAction::RetryCompaction)
            && facts.active_conversation_id.is_some()
            && record.conversation_id == facts.active_conversation_id)
}

/// Open S-LOG (idempotent). Presents every record: the unseen badge clears
/// here and nowhere else. Saves the caller focus; never changes the draft.
pub fn open(state: &mut TuiState) {
    reducer::open(state, Surface::NotificationLog);
}

/// Close S-LOG and restore the caller focus.
pub fn close(state: &mut TuiState) {
    reducer::close(state, Surface::NotificationLog);
}

/// What the host must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOutcome {
    /// Swallowed (selection/detail moved, or an ineligible key).
    Consumed,
    /// The log closed; the caller focus is restored.
    Closed,
    /// Copy this text through the host clipboard helper.
    Copy(String),
    /// `Ctrl+Q`: keep the log open; the host runs its guarded quit request above it.
    Quit,
    /// Run the record's retry exactly once.
    Retry {
        action: NoticeAction,
        conversation_id: String,
    },
}

fn newest_first<'a>(
    store: &'a NotificationStore,
    search: &InspectorSearch,
) -> Vec<&'a NoticeRecord> {
    store
        .records()
        .rev()
        .filter(|record| {
            search.matches(record.severity.word())
                || search.matches(record.source.label())
                || search.matches(&record.message)
        })
        .collect()
}

/// The selected row among `rows` (newest first): the selected record's row,
/// else the nearest surviving row to where it was.
fn resolve(log: &NotificationLogState, rows: &[&NoticeRecord]) -> usize {
    let nearest = log.selected.min(rows.len().saturating_sub(1));
    log.selected_id
        .and_then(|id| rows.iter().position(|r| r.id == id))
        .unwrap_or(nearest)
}

fn select(log: &mut NotificationLogState, rows: &[&NoticeRecord], index: usize) {
    log.selected = index;
    log.selected_id = rows.get(index).map(|r| r.id);
}

/// Reconcile a content edit without changing the identity of a retained match.
/// An empty result set keeps a still-retained id for clearing the filter later.
pub fn reconcile_search(state: &mut TuiState) {
    let Some(log) = state.notification_log.as_mut() else {
        return;
    };
    let rows = newest_first(&state.notifications, &log.search);
    let selected = resolve(log, &rows);
    sync_selection(log, &rows, selected, &state.notifications);
}

fn sync_selection(
    log: &mut NotificationLogState,
    rows: &[&NoticeRecord],
    selected: usize,
    store: &NotificationStore,
) {
    if rows.is_empty() {
        log.selected_id = log
            .selected_id
            .filter(|id| store.records().any(|record| record.id == *id));
    } else {
        select(log, rows, selected);
    }
}

/// Apply one key to the open log: resolve it in `NotificationLog` and run the
/// command. An unbound key is swallowed. Without an open log it changes nothing.
pub fn handle_key(state: &mut TuiState, host: Host, key: Key, facts: &LogFacts) -> LogOutcome {
    if !presentation::is_open(state, Surface::NotificationLog) {
        return LogOutcome::Consumed;
    }
    let Some(log) = state.notification_log.as_ref() else {
        return LogOutcome::Consumed;
    };
    let detail = log.detail;
    let rows = newest_first(&state.notifications, &log.search);
    let total = rows.len();
    let selected = resolve(log, &rows);
    let current = rows.get(selected).copied();
    if let Some(log) = state.notification_log.as_mut() {
        sync_selection(log, &rows, selected, &state.notifications);
    }
    state.needs_redraw = true;
    let command = match keymap::resolve(Context::NotificationLog, host, &[key]) {
        Resolved::Hit(row) => Some(row.command),
        _ => None,
    };
    let Some(CommandId::Overlay(command)) = command else {
        return LogOutcome::Consumed;
    };
    match command {
        OverlayCmd::InspectorSearch => {
            let log = state.notification_log.as_mut().unwrap();
            log.search.open();
            log.detail = false;
            log.detail_scroll = 0;
            LogOutcome::Consumed
        }
        OverlayCmd::NotifClose => {
            reducer::back(state);
            LogOutcome::Closed
        }
        OverlayCmd::QuitRequest => LogOutcome::Quit,
        OverlayCmd::NotifNext => {
            if let Some(log) = state.notification_log.as_mut() {
                if detail {
                    log.detail_scroll = log.detail_scroll.saturating_add(1);
                } else if selected + 1 < total {
                    select(log, &rows, selected + 1);
                }
            }
            LogOutcome::Consumed
        }
        OverlayCmd::NotifPrev => {
            if let Some(log) = state.notification_log.as_mut() {
                if detail {
                    log.detail_scroll = log.detail_scroll.saturating_sub(1);
                } else if !rows.is_empty() {
                    select(log, &rows, selected.saturating_sub(1));
                }
            }
            LogOutcome::Consumed
        }
        OverlayCmd::NotifDetail => {
            if let Some(log) = state.notification_log.as_mut() {
                if current.is_some() {
                    log.detail = !log.detail;
                    log.detail_scroll = 0;
                }
            }
            LogOutcome::Consumed
        }
        OverlayCmd::NotifCopy => match current {
            Some(record) => LogOutcome::Copy(record.message.clone()),
            None => LogOutcome::Consumed,
        },
        OverlayCmd::NotifDismiss => {
            if let Some(id) = current.map(|r| r.id) {
                state.notifications.dismiss(id);
            }
            LogOutcome::Consumed
        }
        OverlayCmd::NotifRetry => {
            let Some(record) = current else {
                return LogOutcome::Consumed;
            };
            if !retry_eligible(record, facts) {
                return LogOutcome::Consumed;
            }
            let (id, conversation_id) = (record.id, record.conversation_id.clone());
            match (state.notifications.take_action(id), conversation_id) {
                (Some(action), Some(conversation_id)) => LogOutcome::Retry {
                    action,
                    conversation_id,
                },
                _ => LogOutcome::Consumed,
            }
        }
        OverlayCmd::NotifCompact => {
            let Some(record) = current.filter(|r| compact_eligible(r, facts)) else {
                return LogOutcome::Consumed;
            };
            let message = record.message.clone();
            let conversation_id = record.conversation_id.clone().unwrap_or_default();
            close(state);
            // Raises the decision; nothing is compacted until the pane's own `K`.
            crate::adapters::tui::decision_pane::raise(
                state,
                crate::adapters::tui::decision_pane::DecisionDraft::new(
                    crate::adapters::tui::decision_pane::DecisionKind::ContextWarning,
                    message,
                )
                .conversation(conversation_id),
                crate::adapters::tui::decision_pane::Arrival::Requested,
            );
            LogOutcome::Closed
        }
        OverlayCmd::NotifFresh => {
            let Some(record) = current.filter(|r| fresh_eligible(r, facts)) else {
                return LogOutcome::Consumed;
            };
            let conversation_id = record.conversation_id.clone().unwrap_or_default();
            close(state);
            crate::adapters::tui::decision_pane::request_carryover(state, Some(conversation_id));
            LogOutcome::Closed
        }
        _ => LogOutcome::Consumed,
    }
}

/// Apply a local domain event. User keys, paste and mouse are consumed;
/// resize/focus events return `None` so the caller handles them normally.
pub fn handle_domain_event(
    state: &mut TuiState,
    event: &crate::domain::events::DomainInputEvent,
    facts: &LogFacts,
) -> Option<LogOutcome> {
    use crate::domain::events::DomainInputEvent;
    if matches!(
        event,
        DomainInputEvent::Resize(..) | DomainInputEvent::FocusGained | DomainInputEvent::FocusLost
    ) {
        return None;
    }
    let key = Key::from_domain(event).unwrap_or(Key::Other);
    Some(handle_key(state, Host::Local, key, facts))
}

/// Apply a raw crossterm key (the attached client's ingress).
pub fn handle_raw_key(
    state: &mut TuiState,
    key: crossterm::event::KeyEvent,
    facts: &LogFacts,
) -> LogOutcome {
    handle_key(state, Host::Attached, Key::from_crossterm(&key), facts)
}

/// The record row: `severity · HH:MM:SS · source · ×count · message`, with the
/// owning conversation named when it is not the active one.
fn row_text(record: &NoticeRecord, facts: &LogFacts) -> String {
    let mut text = format!(
        "{} · {} · {}",
        record.severity.word(),
        format_hms(record.last_wall_ms),
        record.source.label()
    );
    if record.count > 1 {
        text.push_str(&format!(" · ×{}", record.count));
    }
    if let Some(conversation_id) = &record.conversation_id
        && facts.active_conversation_id.as_deref() != Some(conversation_id.as_str())
    {
        text.push_str(&format!(" · [{}]", facts.title_of(conversation_id)));
    }
    text.push_str(" · ");
    text.push_str(&headline(&record.message));
    text
}

fn severity_color(theme: &Theme, severity: NoticeSeverity) -> Color {
    match severity {
        NoticeSeverity::Info => theme.colors.info,
        NoticeSeverity::Success => theme.colors.success,
        NoticeSeverity::Warning => theme.colors.warning,
        NoticeSeverity::Error => theme.colors.error,
    }
}

/// Paint S-LOG into the caller's exact frame rectangle. Shared body fitting
/// preserves the header, at least one record row and the action rail.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    log: &NotificationLogState,
    store: &NotificationStore,
    theme: &Theme,
    facts: &LogFacts,
    show_caret: bool,
) {
    if area.width < 12 || area.height < 5 {
        return;
    }
    let inner_width = usize::from(area.width.saturating_sub(2));
    let normal = Style::default().fg(theme.colors.fg_primary);
    let muted = Style::default().fg(theme.colors.fg_muted);
    let accent = Style::default().fg(theme.colors.accent);
    let rows = newest_first(store, &log.search);
    let selected = resolve(log, &rows);
    let current = rows.get(selected).copied();
    let query_visible = log.search.editing || !log.search.query.is_empty();
    let compact = area.height <= 6;

    let mut parts = Vec::new();
    if !compact || !query_visible {
        parts.push(ModalPart::Header(vec![Line::from(Span::styled(
            format!(
                "Notifications ({}){}",
                rows.len(),
                if log.detail { " · detail" } else { "" },
            ),
            normal.add_modifier(Modifier::BOLD),
        ))]));
    }
    if rows.is_empty() {
        parts.push(ModalPart::Rows {
            lines: vec![Line::from(Span::styled(
                if log.search.query.is_empty() {
                    "No notifications yet."
                } else {
                    "No matching notifications."
                },
                muted,
            ))],
            selected: (0, 0),
        });
    } else if log.detail {
        let record = rows[selected];
        let mut lines = vec![Line::from(Span::styled(
            geom::ellipsize(&row_meta(record, facts), inner_width, "…").into_owned(),
            Style::default()
                .fg(severity_color(theme, record.severity))
                .add_modifier(Modifier::BOLD),
        ))];
        lines.extend(crate::adapters::tui::widgets::modal::wrapped(
            &record.message,
            normal,
            inner_width,
        ));
        let skip = log.detail_scroll.min(lines.len().saturating_sub(1));
        parts.push(ModalPart::Rows {
            lines: lines.into_iter().skip(skip).collect(),
            selected: (0, 0),
        });
    } else {
        let lines = rows
            .iter()
            .enumerate()
            .map(|(index, record)| {
                let text = geom::ellipsize(&row_text(record, facts), inner_width, "…").into_owned();
                let style = if index == selected {
                    Style::default()
                        .fg(severity_color(theme, record.severity))
                        .add_modifier(Modifier::REVERSED)
                } else if record.presentation == Presentation::Visible {
                    normal
                } else {
                    muted
                };
                Line::from(Span::styled(text, style))
            })
            .collect();
        parts.push(ModalPart::Rows {
            lines,
            selected: (selected, selected + 1),
        });
    }

    let mut bottom = None;
    let rail = if log.search.editing {
        vec![
            Span::styled(
                keymap::render::rail(Context::InspectorSearch, OverlayCmd::InspectorAccept),
                accent,
            ),
            Span::styled(" Keep filter ", normal),
            Span::styled(
                keymap::render::rail(Context::InspectorSearch, OverlayCmd::InspectorBack),
                accent,
            ),
            Span::styled(" Clear ", normal),
        ]
    } else {
        let key = |command: OverlayCmd| keymap::render::rail(Context::NotificationLog, command);
        let mut secondary = vec![
            Span::styled(
                format!(
                    "[{}]",
                    keymap::render::rail_pair(
                        Context::NotificationLog,
                        OverlayCmd::NotifNext,
                        OverlayCmd::NotifPrev,
                    )
                ),
                accent,
            ),
            Span::styled(" Move ", normal),
        ];
        let mut primary = vec![
            Span::styled(key(OverlayCmd::NotifDetail), accent),
            Span::styled(if log.detail { " List " } else { " Detail " }, normal),
            Span::styled(key(OverlayCmd::NotifCopy), accent),
            Span::styled(" Copy ", normal),
        ];
        if current.is_some_and(|record| retry_eligible(record, facts)) {
            primary.push(Span::styled(key(OverlayCmd::NotifRetry), accent));
            primary.push(Span::styled(" Retry ", normal));
        }
        if current.is_some_and(|record| compact_eligible(record, facts)) {
            primary.push(Span::styled(key(OverlayCmd::NotifCompact), accent));
            primary.push(Span::styled(" Compact… ", normal));
        }
        if current.is_some_and(|record| fresh_eligible(record, facts)) {
            primary.push(Span::styled(key(OverlayCmd::NotifFresh), accent));
            primary.push(Span::styled(" Start fresh… ", normal));
        }
        let trailing = vec![
            Span::styled(key(OverlayCmd::NotifDismiss), accent),
            Span::styled(" Dismiss ", normal),
            Span::styled(key(OverlayCmd::InspectorSearch), accent),
            Span::styled(" Search ", normal),
            Span::styled(key(OverlayCmd::NotifClose), accent),
            Span::styled(" Close", normal),
        ];
        if compact {
            // Keep every eligible primary action and one body row. Secondary
            // navigation/close hints use the otherwise unused closing border.
            secondary.extend(trailing);
            bottom = Some(Line::from(secondary));
            primary
        } else {
            secondary.extend(primary);
            secondary.extend(trailing);
            secondary
        }
    };
    parts.push(ModalPart::Footer(footer_lines(rail, inner_width)));

    frame.render_widget(Clear, area);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(accent)
        .title(" System · Notifications ")
        .style(Style::default().bg(theme.colors.bg_surface));
    if let Some(bottom) = bottom {
        block = block.title_bottom(bottom);
    }
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if query_visible {
        frame.render_widget(
            Paragraph::new(log.search.query_line(inner_width, theme, show_caret)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        inner.y += 1;
        inner.height -= 1;
    }
    let lines = fit_modal(parts, usize::from(inner.height), usize::from(inner.width));
    frame.render_widget(
        Paragraph::new(lines).style(
            Style::default()
                .fg(theme.colors.fg_primary)
                .bg(theme.colors.bg_surface),
        ),
        inner,
    );
}

fn row_meta(record: &NoticeRecord, facts: &LogFacts) -> String {
    let mut text = format!(
        "{} · {} · {}",
        record.severity.word(),
        format_hms(record.last_wall_ms),
        record.source.label()
    );
    if record.count > 1 {
        text.push_str(&format!(" · ×{}", record.count));
    }
    if let Some(conversation_id) = &record.conversation_id {
        text.push_str(&format!(" · [{}]", facts.title_of(conversation_id)));
    }
    text
}

/// Greedy wrap of the rail spans onto rows no wider than `width` cells (a span
/// pair — key plus label — is never split).
pub(crate) fn footer_lines(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for pair in spans.chunks(2) {
        let cells: usize = pair.iter().map(|s| geom::cells(&s.content)).sum();
        if used > 0 && used + cells > width {
            lines.push(Line::from(std::mem::take(&mut current)));
            used = 0;
        }
        used += cells;
        current.extend(pair.iter().cloned());
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::notifications::NoticeSource;
    use crate::adapters::tui::{app::handle_input, layout::compute_layout, notifications};
    use crate::domain::events::{DomainInputEvent, DomainKey};
    use crate::domain::models::visual::DensityMode;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(state: &mut TuiState, key: DomainKey) {
        handle_input(state, &DomainInputEvent::SpecialKey(key));
    }

    fn paint(state: &TuiState) -> String {
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
    fn story21_4_log_inspector_real_keys_filter_and_back_retains_matching_id() {
        let mut state = TuiState::new(80, 24);
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Error,
            "retained failure",
        );
        let retained = state.notifications.records().next_back().unwrap().id;
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Info,
            "ordinary arrival",
        );
        press(&mut state, DomainKey::CtrlX);
        handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
        handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
        assert_eq!(
            state.notification_log.as_ref().unwrap().selected_id,
            Some(retained)
        );
        let unfiltered = paint(&state);
        assert!(unfiltered.contains("ordinary arrival"));
        assert!(unfiltered.contains("retained failure"));
        handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
        for ch in "eRr".chars() {
            handle_input(&mut state, &DomainInputEvent::KeyPress(ch));
        }
        let filtered = paint(&state);
        assert!(filtered.contains("retained failure"));
        assert!(!filtered.contains("ordinary arrival"));
        assert_eq!(
            state.notification_log.as_ref().unwrap().selected_id,
            Some(retained)
        );
        handle_input(
            &mut state,
            &DomainInputEvent::Paste("no-record-matches".into()),
        );
        assert!(paint(&state).contains("No matching notifications"));
        assert_eq!(
            state.notification_log.as_ref().unwrap().selected_id,
            Some(retained)
        );
        press(&mut state, DomainKey::Esc);
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::NotificationLog
        );
        assert!(paint(&state).contains("ordinary arrival"));
        assert_eq!(
            state.notification_log.as_ref().unwrap().selected_id,
            Some(retained)
        );
        press(&mut state, DomainKey::Esc);
        assert!(!presentation::is_open(&state, Surface::NotificationLog));
    }

    #[test]
    fn story21_4_log_inspector_real_paste_empty_and_accept_reenter() {
        let mut state = TuiState::new(80, 24);
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Info,
            "visible baseline",
        );
        press(&mut state, DomainKey::CtrlX);
        handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
        assert!(paint(&state).contains("visible baseline"));
        handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
        handle_input(
            &mut state,
            &DomainInputEvent::Paste("no-record-matches".into()),
        );
        assert!(!paint(&state).contains("visible baseline"));
        assert!(paint(&state).contains("No matching notifications"));
        press(&mut state, DomainKey::Enter);
        assert!(paint(&state).contains("No matching notifications"));
        handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
        press(&mut state, DomainKey::Esc);
        assert!(paint(&state).contains("visible baseline"));
        assert_eq!(
            state.modal_stack.top().unwrap().surface,
            Surface::NotificationLog
        );
    }

    #[test]
    fn story21_4_log_inspector_from_detail_accept_returns_to_the_filtered_list() {
        let mut state = TuiState::new(80, 24);
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Error,
            "first failure",
        );
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Error,
            "second failure",
        );
        press(&mut state, DomainKey::CtrlX);
        handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
        press(&mut state, DomainKey::Enter);
        assert!(
            state.notification_log.as_ref().unwrap().detail,
            "detail is really reachable"
        );
        assert!(
            !paint(&state).contains("first failure"),
            "the second record alone is detailed"
        );
        handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
        handle_input(&mut state, &DomainInputEvent::Paste("eRr".into()));
        press(&mut state, DomainKey::Enter);
        let log = state.notification_log.as_ref().unwrap();
        assert!(!log.search.editing);
        assert_eq!(log.search.query.as_str(), "eRr");
        assert!(
            !log.detail,
            "accept leaves the field in its filtered list, not the old detail"
        );
        let screen = paint(&state);
        assert!(
            screen.contains("first failure") && screen.contains("second failure"),
            "{screen}"
        );
    }

    #[test]
    fn inspector_filter_matches_each_record_field_and_actions_only_target_visible_rows() {
        use crate::adapters::tui::notifications::NoticeDraft;
        let mut state = TuiState::new(80, 24);
        for (source, severity, message) in [
            (
                NoticeSource::System,
                NoticeSeverity::Error,
                "failed operation",
            ),
            (
                NoticeSource::Clipboard,
                NoticeSeverity::Info,
                "ordinary operation",
            ),
            (
                NoticeSource::System,
                NoticeSeverity::Success,
                "target message",
            ),
        ] {
            notifications::notify(&mut state, source, severity, message);
        }
        open(&mut state);
        for (query, expected) in [
            ("ERRor", "failed operation"),
            ("CLIPboard", "ordinary operation"),
            ("TARGET", "target message"),
        ] {
            state
                .notification_log
                .as_mut()
                .unwrap()
                .search
                .query
                .set(query);
            reconcile_search(&mut state);
            let log = state.notification_log.as_ref().unwrap();
            let rows = newest_first(&state.notifications, &log.search);
            assert_eq!(rows.len(), 1);
            assert_eq!(log.selected_id, Some(rows[0].id));
            for key in [Key::Char('j'), Key::Char('k')] {
                assert_eq!(
                    handle_key(&mut state, Host::Local, key, &LogFacts::default()),
                    LogOutcome::Consumed
                );
            }
            assert_eq!(
                handle_key(
                    &mut state,
                    Host::Local,
                    Key::Char('c'),
                    &LogFacts::default()
                ),
                LogOutcome::Copy(expected.into()),
            );
            handle_key(&mut state, Host::Local, Key::Enter, &LogFacts::default());
            assert!(state.notification_log.as_ref().unwrap().detail);
            assert!(paint(&state).contains(expected));
            handle_key(&mut state, Host::Local, Key::Enter, &LogFacts::default());
        }
        notifications::notify_draft(
            &mut state,
            NoticeDraft::new(
                NoticeSource::System,
                NoticeSeverity::Error,
                "retryable hidden record",
            )
            .conversation("active")
            .action(NoticeAction::RetryTurn { messages: 2 }),
        );
        let action_id = state.notifications.records().next_back().unwrap().id;
        state
            .notification_log
            .as_mut()
            .unwrap()
            .search
            .query
            .set("no-match");
        reconcile_search(&mut state);
        let facts = LogFacts {
            active_conversation_id: Some("active".into()),
            active_messages: 2,
            retry_supported: true,
            ..LogFacts::default()
        };
        for key in [
            Key::Char('c'),
            Key::Char('r'),
            Key::Char('x'),
            Key::Enter,
            Key::Char('k'),
        ] {
            assert_eq!(
                handle_key(&mut state, Host::Local, key, &facts),
                LogOutcome::Consumed
            );
        }
        assert!(
            state
                .notifications
                .records()
                .find(|record| record.id == action_id)
                .unwrap()
                .action
                .is_some()
        );
        state
            .notification_log
            .as_mut()
            .unwrap()
            .search
            .query
            .set("retryable");
        reconcile_search(&mut state);
        assert!(matches!(
            handle_key(&mut state, Host::Local, Key::Char('r'), &facts),
            LogOutcome::Retry {
                action: NoticeAction::RetryTurn { messages: 2 },
                ..
            }
        ));
    }

    #[test]
    fn inspector_filtered_selection_survives_arrival_then_falls_back_after_real_eviction() {
        use crate::adapters::tui::notifications::MAX_RECORDS;
        let mut state = TuiState::new(80, 24);
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Error,
            "retained error",
        );
        open(&mut state);
        let retained = state
            .notification_log
            .as_ref()
            .unwrap()
            .selected_id
            .unwrap();
        state
            .notification_log
            .as_mut()
            .unwrap()
            .search
            .query
            .set("err");
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Error,
            "later error",
        );
        reconcile_search(&mut state);
        assert_eq!(
            state.notification_log.as_ref().unwrap().selected_id,
            Some(retained)
        );
        state.notifications.clear_toasts();
        for index in 0..MAX_RECORDS {
            notifications::notify(
                &mut state,
                NoticeSource::System,
                NoticeSeverity::Error,
                format!("arrival error {index}"),
            );
        }
        assert!(
            !state
                .notifications
                .records()
                .any(|record| record.id == retained)
        );
        reconcile_search(&mut state);
        let log = state.notification_log.as_ref().unwrap();
        let rows = newest_first(&state.notifications, &log.search);
        assert_eq!(log.selected_id, Some(rows[log.selected].id));
        assert_ne!(log.selected_id, Some(retained));
    }

    #[test]
    fn inspector_five_row_sheet_keeps_query_body_and_truthful_primary_actions() {
        use crate::adapters::tui::notifications::NoticeDraft;
        let mut state = TuiState::new(60, 16);
        notifications::notify_draft(
            &mut state,
            NoticeDraft::new(
                NoticeSource::Context,
                NoticeSeverity::Warning,
                "context warning",
            )
            .conversation("active")
            .action(NoticeAction::ContextDecision),
        );
        open(&mut state);
        let facts = LogFacts {
            active_conversation_id: Some("active".into()),
            decisions_supported: true,
            context_warning: true,
            ..LogFacts::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 5)).unwrap();
        for editing in [false, true] {
            let log = state.notification_log.as_mut().unwrap();
            log.search.query.set("warning");
            log.search.editing = editing;
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        frame.area(),
                        state.notification_log.as_ref().unwrap(),
                        &state.notifications,
                        &state.theme,
                        &facts,
                        true,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            assert!(row(1).contains("/ warning"));
            assert!(row(2).contains("Warning"));
            assert_eq!(buffer[(0, 4)].symbol(), "└");
            assert_eq!(buffer[(59, 4)].symbol(), "┘");
            if editing {
                assert!(row(3).contains("Keep filter"));
                assert!(row(3).contains("Clear"));
                assert!(!row(4).contains("Close"));
                assert!(buffer[(10, 1)].modifier.contains(Modifier::REVERSED));
            } else {
                assert!(row(3).contains("Compact…"));
                assert!(row(3).contains("Start fresh…"));
                assert!(row(4).contains("[/] Search"));
                assert!(row(4).contains("Close"));
                assert!(!buffer[(10, 1)].modifier.contains(Modifier::REVERSED));
            }
        }
    }
}
