//! Shared fixtures for the 21-6 decision-queue keystones. Every assertion reads a
//! real rendered `Buffer` (the shared `presentation::compose` boundary both hosts
//! call) or a real front door (`presentation::route`, `handle_input`, the
//! producers). Time is a `MockClock`: no sleeps.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::Paragraph;
use rustain::adapters::tui::decision_queue::{self, INERT_AFTER_PAINT};
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout::{AppLayout, compute_layout};
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::notifications::NotificationStore;
use rustain::adapters::tui::presentation::{self, RouteEffect, Shell};
use rustain::adapters::tui::state::{LogAwareness, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::{status_bar, toast_layer};
use rustain::domain::clock::MockClock;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{NotificationsConfig, PermissionMode, StatusState};
use unicode_width::UnicodeWidthStr;

/// Install a recording copier (never the real clipboard); the shared list holds every
/// text the state's `c` copy handed it.
pub fn record_copies(state: &mut TuiState) -> Arc<std::sync::Mutex<Vec<String>>> {
    let copies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = copies.clone();
    state.clipboard_copier = Some(Arc::new(move |text: &str| {
        sink.lock().unwrap().push(text.to_owned());
        rustain::adapters::tui::clipboard::ClipboardResult::Osc52Success
    }));
    copies
}

/// A `TuiState` whose notification store runs on a `MockClock` and whose clipboard
/// is a recorder (hermetic: no test writes the real clipboard or home).
pub fn clocked(width: u16, height: u16) -> (TuiState, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = TuiState::new(width, height);
    record_copies(&mut state);
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    let layout = layout_of(&state, width, height);
    state
        .notifications
        .set_capacity(toast_layer::capacity_for(layout.chat_pane.height));
    (state, clock)
}

/// The layout the hosts compute for the state's current composer text.
pub fn layout_of(state: &TuiState, width: u16, height: u16) -> AppLayout {
    compute_layout(
        Rect::new(0, 0, width, height),
        &state.theme,
        &state.input_buffer,
        1,
        false,
    )
    .expect("the terminal is at or above the 60x16 floor")
}

pub fn facts(active: &str) -> LogFacts {
    LogFacts {
        active_conversation_id: Some(active.to_owned()),
        decisions_supported: true,
        retry_supported: true,
        context_warning: true,
        ..LogFacts::default()
    }
}

pub fn advance(clock: &MockClock, by: Duration) {
    clock.advance(by);
}

/// The Stage rows (distinct text per row), the composer and status sentinels, then
/// the shared compose boundary: exactly the host's order. When compose reports the
/// decision frame painted on top the inert clock starts, as both hosts do.
pub fn paint(state: &mut TuiState, f: &LogFacts, width: u16, height: u16) -> Buffer {
    let layout = layout_of(state, width, height);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut painted = false;
    {
        let state: &TuiState = state;
        terminal
            .draw(|frame| {
                let pane = layout.chat_pane;
                let rows: Vec<String> = (0..pane.height)
                    .map(|row| {
                        let tag = format!("stage row {row:02} ");
                        tag.repeat(usize::from(pane.width) / tag.len() + 1)
                    })
                    .collect();
                frame.render_widget(Paragraph::new(rows.join("\n")), pane);
                frame.render_widget(Paragraph::new("composer-sentinel"), layout.input_area);
                let shell = Shell::new(frame.area(), &layout);
                painted = presentation::compose(
                    frame,
                    state,
                    &shell,
                    Host::Local,
                    f,
                    &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                    |frame| {
                        frame.render_widget(
                            Paragraph::new("status-row-sentinel"),
                            layout.status_bar,
                        );
                    },
                );
            })
            .unwrap();
    }
    if painted {
        decision_queue::note_painted(state);
    }
    terminal.backend().buffer().clone()
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

/// The index of the first row containing `needle`.
pub fn row_of(buffer: &Buffer, needle: &str) -> Option<u16> {
    (buffer.area.top()..buffer.area.bottom()).find(|y| row_text(buffer, *y).contains(needle))
}

/// First paint, then the inert window passes (the clock only moves; no sleep).
pub fn ready(state: &mut TuiState, clock: &MockClock) {
    decision_queue::note_painted(state);
    advance(clock, INERT_AFTER_PAINT);
}

pub fn key(c: char) -> DomainInputEvent {
    DomainInputEvent::KeyPress(c)
}

pub fn special(k: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(k)
}

/// A key through the local host's route (the 21-4 router the event loop calls
/// before `handle_input`), checking the stack stays consistent.
pub fn route(state: &mut TuiState, event: &DomainInputEvent, f: &LogFacts) -> Option<RouteEffect> {
    let effect = presentation::route(state, event, Host::Local, f);
    assert!(presentation::consistent(state));
    effect
}

/// The real status row, drawn the way both hosts call it.
pub fn status_row(state: &TuiState, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
    let theme = Theme::dark();
    terminal
        .draw(|frame| {
            status_bar::render(
                frame,
                frame.area(),
                "model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                10,
                PermissionMode::Normal,
                None,
                0,
                false,
                None,
                false,
                true,
                0,
                None,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                false,
                None,
                LogAwareness::Hidden,
                status_bar::NoticeBadges {
                    unseen: state.notifications.unseen(),
                    decisions: state.decisions.len(),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    row_text(terminal.backend().buffer(), 0)
}
