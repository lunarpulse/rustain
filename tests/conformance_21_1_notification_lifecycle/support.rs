//! Shared fixtures for the 21-1 notification-lifecycle conformance module.
//! Everything reads a real rendered `Buffer`; no expected row is built from
//! the formula under test.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::Paragraph;
use rustain::adapters::tui::layout::{AppLayout, compute_layout};
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::notifications::{
    self, NoticeSeverity, NoticeSource, NotificationStore,
};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::widgets::toast_layer;
use rustain::domain::clock::MockClock;
use rustain::domain::models::NotificationsConfig;
use rustain::domain::models::visual::DensityMode;
use unicode_width::UnicodeWidthStr;

/// A `TuiState` whose notification store runs on a `MockClock` with the
/// default effective lifetimes (4/4/10/60 s).
pub fn clocked(width: u16, height: u16) -> (TuiState, Arc<MockClock>) {
    clocked_with(width, height, NotificationsConfig::default())
}

pub fn clocked_with(
    width: u16,
    height: u16,
    config: NotificationsConfig,
) -> (TuiState, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = TuiState::new(width, height);
    state.notifications = NotificationStore::from_config(&config, clock.clone());
    let layout = layout_of(&state, width, height);
    state
        .notifications
        .set_capacity(toast_layer::capacity_for(layout.chat_pane.height));
    (state, clock)
}

pub fn layout_of(state: &TuiState, width: u16, height: u16) -> AppLayout {
    compute_layout(
        Rect::new(0, 0, width, height),
        &state.theme,
        "",
        1,
        false,
        DensityMode::Focus,
    )
    .expect("the terminal is at or above the 60x16 floor")
}

pub fn advance(clock: &MockClock, by: Duration) {
    clock.advance(by);
}

pub fn notify(state: &mut TuiState, severity: NoticeSeverity, message: &str) {
    notifications::notify(state, NoticeSource::System, severity, message);
}

/// One draw exactly the way both hosts do it: sweep first, stage, then the
/// toast layer in the chat pane. `with_toasts = false` paints the stage only.
pub fn draw(state: &mut TuiState, width: u16, height: u16, with_toasts: bool) -> Buffer {
    draw_with(state, width, height, with_toasts, &LogFacts::default())
}

/// [`draw`] for a host whose active conversation is `active` and whose tabs
/// are named by `titles` (background-conversation attribution).
pub fn draw_attributed(
    state: &mut TuiState,
    width: u16,
    height: u16,
    active: Option<&str>,
    titles: &[(String, String)],
) -> Buffer {
    let facts = LogFacts {
        active_conversation_id: active.map(str::to_owned),
        titles: titles.to_vec(),
        ..LogFacts::default()
    };
    draw_with(state, width, height, true, &facts)
}

pub fn draw_with(
    state: &mut TuiState,
    width: u16,
    height: u16,
    with_toasts: bool,
    facts: &LogFacts,
) -> Buffer {
    let layout = layout_of(state, width, height);
    notifications::begin_draw(state);
    state
        .notifications
        .set_capacity(toast_layer::capacity_for(layout.chat_pane.height));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = state.theme.clone();
    let store = &state.notifications;
    terminal
        .draw(|frame: &mut Frame| {
            paint_stage(frame, &layout);
            if with_toasts {
                toast_layer::render(frame, layout.chat_pane, store, &theme, facts);
            }
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Transcript filler: every chat-pane row carries distinct text so any toast
/// cell is a visible difference against the stage-only draw.
fn paint_stage(frame: &mut Frame, layout: &AppLayout) {
    let pane = layout.chat_pane;
    let lines: Vec<String> = (0..pane.height)
        .map(|row| {
            let tag = format!("stage row {row:02} ");
            tag.repeat(usize::from(pane.width) / tag.len() + 1)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines.join("\n")), pane);
    frame.render_widget(Paragraph::new("status-row-sentinel"), layout.status_bar);
    frame.render_widget(Paragraph::new("composer-sentinel"), layout.input_area);
}

pub fn row_text(buffer: &Buffer, y: u16) -> String {
    let mut out = String::new();
    let mut x = buffer.area.left();
    while x < buffer.area.right() {
        let Some(cell) = buffer.cell(Position::new(x, y)) else {
            break;
        };
        let symbol = cell.symbol();
        out.push_str(symbol);
        x = x.saturating_add(u16::try_from(symbol.width().max(1)).unwrap_or(1));
    }
    out
}

pub fn screen(buffer: &Buffer) -> String {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Coordinates whose symbol or style differs between two same-size buffers.
pub fn differing_cells(a: &Buffer, b: &Buffer) -> Vec<(u16, u16)> {
    assert_eq!(a.area, b.area);
    let mut out = Vec::new();
    for y in a.area.top()..a.area.bottom() {
        for x in a.area.left()..a.area.right() {
            let pos = Position::new(x, y);
            if a.cell(pos) != b.cell(pos) {
                out.push((x, y));
            }
        }
    }
    out
}
