//! Shared fixtures for the 21-7 hub keystones: a real workspace journal that node events are
//! appended to through the production `NodeJournal` writer, the 21-8 `Rig` (the 21-5 `Fixture`:
//! owning manager, runtime renderer, compositor; plus the `AppState` the production effect
//! functions take), the front doors (`Ctrl+X r` and the `/room` command parser) and readers of
//! the painted frame. No sleeps: ticks are values, the clock is mocked.
#![allow(dead_code)]

use ratatui::layout::Rect;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::{McRow, McTab};
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::{AgentId, HostBinding, NodeOrigin, NodeState, RoomEvent};
use rustain::infrastructure::runtime::room_bridge;

pub use crate::conformance_21_8_thread_inspector::support::{Rig, Workspace};

/// The conversation id the scratch composer commands run under.
pub const CONVERSATION: &str = "mission-control";

pub const NODES: [&str; 3] = ["spoke-alpha", "spoke-bravo", "spoke-charlie"];

/// A real workspace whose durable room journal holds `names` as running nodes of host-A.
pub async fn room_workspace(names: &[&str]) -> Workspace {
    let workspace = Workspace::new().await;
    for name in names {
        register(&workspace, name).await;
    }
    workspace
}

pub async fn register(workspace: &Workspace, name: &str) {
    let node = AgentId::parse(name).expect("agent id");
    workspace
        .node
        .append_room(RoomEvent::NodeRegistered {
            node: node.clone(),
            origin: NodeOrigin::Interactive,
            host: HostBinding::new("host-A", "ws"),
        })
        .await
        .expect("append the registration");
    workspace
        .node
        .append_room(RoomEvent::NodeStateChanged {
            node,
            from: NodeState::Created,
            to: NodeState::Running,
        })
        .await
        .expect("append the state change");
}

/// Compose the production transparency service over the workspace journal (the room bridge
/// reads through it): what `startup` does for a session with a journal.
pub fn attach_journal(rig: &mut Rig) {
    use rustain::infrastructure::{
        subagent::node_journal::WorkspaceJournalReader, transparency::TransparencyService,
    };
    if rig.app.transparency.is_none() {
        let workspace = rig.ws.path().to_path_buf();
        rig.app.transparency = Some(std::sync::Arc::new(TransparencyService::new(
            std::sync::Arc::new(WorkspaceJournalReader::open_workspace(&workspace)),
            workspace,
        )));
    }
}

/// `Ctrl+X r` through the local front door, then the effect the event loop runs for the
/// action it returns, then one completed draw of the production host.
pub async fn open_room(rig: &mut Rig) {
    attach_journal(rig);
    assert_eq!(rig.f.press(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(
        rig.f.press(Key::Char('r')),
        InputAction::OpenMissionControl(McTab::Room)
    );
    room_bridge::refresh_mission_tab(&rig.app, &mut rig.f.h.state, McTab::Room).await;
    rig.f.paint();
}

/// A `/room …` line through the production command parser (on a scratch composer, so the
/// operator's own draft is untouched) and the production `/room` arm.
pub async fn slash_room(rig: &mut Rig, arguments: &str) {
    attach_journal(rig);
    let mut scratch = TuiState::new(130, 30);
    scratch.input_buffer = format!("/room{arguments}");
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut scratch) else {
        panic!("/room must use the production command parser");
    };
    assert_eq!(name, "room");
    room_bridge::room_command(&mut rig.f.h.state, CONVERSATION, args.as_deref(), &rig.app).await;
    rig.f.paint();
}

/// The list rows a probe tab shows: `row-<id>` with a detail text per row.
pub fn probe_rows(ids: &[&str]) -> Vec<McRow> {
    ids.iter()
        .map(|id| {
            McRow::new(
                id,
                &format!("row {id}"),
                &format!("detail of {id}\nsecond line"),
            )
        })
        .collect()
}

pub fn top_surface(rig: &Rig) -> Option<Surface> {
    rig.f.h.state.modal_stack.top().map(|frame| frame.surface)
}

pub fn top_title(rig: &Rig) -> Option<String> {
    rig.f
        .h
        .state
        .modal_stack
        .top()
        .map(|frame| frame.title().to_owned())
}

/// The frame rectangle the compositor derived for the current size and composer.
pub fn frame_rect(rig: &Rig) -> Rect {
    rig.f.frame_rect()
}

/// The painted rows, read cell by cell (the harness text helper splits wide glyphs by bytes).
pub fn lines(rig: &Rig) -> Vec<String> {
    let buffer = rig.f.h.terminal.backend().buffer();
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

/// The painted rows of the frame body below the tab row and above the rail.
pub fn body_lines(rig: &Rig) -> Vec<String> {
    let rect = frame_rect(rig);
    let all = lines(rig);
    (rect.y + 2..rect.bottom().saturating_sub(2))
        .map(|y| all[usize::from(y)].clone())
        .collect()
}

/// The rail row (the last row inside the frame).
pub fn rail_line(rig: &Rig) -> String {
    let rect = frame_rect(rig);
    lines(rig)[usize::from(rect.bottom() - 2)].clone()
}

/// The composer rows of the current layout (what the frame must never write).
pub fn composer_rows(rig: &Rig) -> Vec<String> {
    let layout = rig.f.layout();
    let all = lines(rig);
    (layout.input_area.y..layout.input_area.bottom())
        .map(|y| all[usize::from(y)].clone())
        .collect()
}

pub fn status_row(rig: &Rig) -> String {
    let layout = rig.f.layout();
    lines(rig)[usize::from(layout.status_bar.y)].clone()
}

pub fn is_open(rig: &Rig, surface: Surface) -> bool {
    presentation::is_open(&rig.f.h.state, surface)
}

/// A key as a raw crossterm event of the given kind and modifiers, through the local host's
/// raw pre-filter (`handle_operator_input`): `None` is a dropped event.
pub fn raw(
    rig: &mut Rig,
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    kind: crossterm::event::KeyEventKind,
) -> Option<InputAction> {
    use crossterm::event::{Event, KeyEvent};
    let mut key = KeyEvent::new(code, modifiers);
    key.kind = kind;
    let event = Event::Key(key);
    let domain = rustain::adapters::tui::app::convert_crossterm_event(
        &event,
        &rustain::domain::models::MouseConfig::default(),
    )?;
    rustain::adapters::tui::app::handle_operator_input(&mut rig.f.h.state, &event, &domain)
}
