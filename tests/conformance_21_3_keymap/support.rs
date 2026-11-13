//! Shared fixtures for the 21-3 keymap conformance module. Every leg drives a
//! key through the REAL front door: crossterm event -> `convert_crossterm_event`
//! -> `app::handle_input` (never a hand-built `DomainInputEvent`/`InputAction`).
#![allow(dead_code)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use rustain::adapters::tui::app::{InputAction, convert_crossterm_event, handle_input};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::DomainInputEvent;
use rustain::domain::models::MouseConfig;

/// The `KeyEvent` a terminal produces for `key` (crossterm 0.29 decoding of the
/// canonical PTY bytes: `0x08` is `Ctrl+H`, `0x0a` is `Ctrl+J`, ...).
pub fn key_event(key: Key) -> KeyEvent {
    let none = KeyModifiers::NONE;
    match key {
        Key::Char(c) => KeyEvent::new(
            KeyCode::Char(c),
            if c.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                none
            },
        ),
        Key::Ctrl(c) => KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
        Key::Alt(c) => KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT),
        Key::Enter => KeyEvent::new(KeyCode::Enter, none),
        Key::ShiftEnter => KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        Key::CtrlEnter => KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        Key::AltEnter => KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        Key::Esc => KeyEvent::new(KeyCode::Esc, none),
        Key::Tab => KeyEvent::new(KeyCode::Tab, none),
        Key::ShiftTab => KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        Key::Backspace => KeyEvent::new(KeyCode::Backspace, none),
        Key::Delete => KeyEvent::new(KeyCode::Delete, none),
        Key::Up => KeyEvent::new(KeyCode::Up, none),
        Key::Down => KeyEvent::new(KeyCode::Down, none),
        Key::Left => KeyEvent::new(KeyCode::Left, none),
        Key::Right => KeyEvent::new(KeyCode::Right, none),
        Key::Home => KeyEvent::new(KeyCode::Home, none),
        Key::End => KeyEvent::new(KeyCode::End, none),
        Key::PageUp => KeyEvent::new(KeyCode::PageUp, none),
        Key::PageDown => KeyEvent::new(KeyCode::PageDown, none),
        Key::F(n) => KeyEvent::new(KeyCode::F(n), none),
        Key::Other => KeyEvent::new(KeyCode::Insert, none),
        Key::Combo(c, bits) => {
            let held = [
                (1, KeyModifiers::CONTROL),
                (2, KeyModifiers::ALT),
                (4, KeyModifiers::SHIFT),
            ];
            let mods = held
                .iter()
                .filter(|(bit, _)| bits & bit != 0)
                .fold(KeyModifiers::NONE, |all, (_, m)| all | *m);
            KeyEvent::new(KeyCode::Char(c), mods)
        }
    }
}

/// Decode `key` exactly as the local host does (None = the host drops it).
pub fn decode(key: Key) -> Option<DomainInputEvent> {
    convert_crossterm_event(&Event::Key(key_event(key)), &MouseConfig::default())
}

/// Press `key` through the local front door and return the action it produced.
pub fn press(state: &mut TuiState, key: Key) -> InputAction {
    match decode(key) {
        Some(event) => handle_input(state, &event),
        None => InputAction::Ignored,
    }
}

/// Press a sequence of keys (`Ctrl+X` then `h`, `z` then `a`, ...).
pub fn press_all(state: &mut TuiState, keys: &[Key]) -> Vec<InputAction> {
    keys.iter().map(|key| press(state, *key)).collect()
}

/// A fresh 80x24 local state (Input focus).
pub fn fresh() -> TuiState {
    TuiState::new(80, 24)
}

/// Raise a real permission, paint its decision frame, and pass its inert window.
pub fn permission() -> TuiState {
    permission_with_input(serde_json::json!({"command": "ls"}))
}

pub fn permission_with_input(input: serde_json::Value) -> TuiState {
    use rustain::adapters::tui::decision_queue::INERT_AFTER_PAINT;
    use rustain::adapters::tui::notifications::NotificationStore;
    use rustain::domain::clock::MockClock;
    use rustain::domain::models::tool_call::{ApprovalSource, RequestId};
    use rustain::domain::models::{NotificationsConfig, ToolRisk};
    use rustain::domain::services::approval_runtime::{ApprovalRuntimeEvent, PendingReview};
    use rustain::infrastructure::runtime::approval_bridge;
    use std::sync::Arc;
    use std::time::Instant;

    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = fresh();
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    let id = RequestId::new();
    let source = ApprovalSource::ForegroundTurn {
        conversation_id: "c1".into(),
    };
    let review = PendingReview {
        id: id.clone(),
        source: source.clone(),
        tool: "Bash".into(),
        risk: ToolRisk::Elevated,
        input: Arc::new(input),
    };
    approval_bridge::apply(
        &mut state,
        ApprovalRuntimeEvent::Requested {
            id,
            source,
            tool: "Bash".into(),
            risk: ToolRisk::Elevated,
            consent_card: None,
        },
        Some(review),
    );
    paint_decision(&mut state);
    clock.advance(INERT_AFTER_PAINT);
    state
}

pub fn paint_decision(state: &mut TuiState) -> String {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::{Position, Rect},
    };
    use rustain::adapters::tui::decision_queue;
    use rustain::adapters::tui::keymap::Host;
    use rustain::adapters::tui::layout::compute_layout;
    use rustain::adapters::tui::notification_log::LogFacts;
    use rustain::adapters::tui::presentation::{self, Shell};
    let layout = compute_layout(
        Rect::new(0, 0, 80, 24),
        &state.theme,
        &state.input_buffer,
        1,
        false,
    )
    .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut painted = false;
    terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            painted = presentation::compose(
                frame,
                state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    assert!(painted, "permission frame painted");
    decision_queue::note_painted(state);
    let buffer = terminal.backend().buffer();
    (0..24)
        .map(|y| {
            (0..80)
                .map(|x| buffer.cell(Position::new(x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn permission_payload(
    state: &TuiState,
) -> &rustain::adapters::tui::decision_queue::PermissionPayload {
    use rustain::adapters::tui::decision_queue::DecisionSubject;
    match &state
        .decisions
        .items()
        .next()
        .expect("permission pending")
        .subject
    {
        DecisionSubject::Permission(payload) => payload,
        other => panic!("expected permission, got {other:?}"),
    }
}

pub fn feedback(state: &TuiState) -> &rustain::adapters::tui::presentation::text_field::TextField {
    permission_payload(state)
        .feedback
        .as_ref()
        .expect("feedback open")
}

/// True when the action is one a "no command fires" sweep tolerates.
pub fn is_inert(action: &InputAction) -> bool {
    matches!(action, InputAction::Consumed | InputAction::Ignored)
}

/// True when `key` reaches its command in `context` through the Global parent
/// (`Ctrl+X` leader, `Ctrl+Q` quit): a sweep asserts that effect instead of
/// skipping the key.
pub fn global_owned(context: rustain::adapters::tui::keymap::Context, key: Key) -> bool {
    use rustain::adapters::tui::keymap::{Context, Host, Resolved, resolve};
    matches!(resolve(context, Host::Local, &[key]),
        Resolved::Hit(row) if row.context == Context::Global)
}

/// The effect of a Global row on `state`: `Ctrl+X` opens the leader, `Ctrl+Q`
/// quits.
pub fn assert_global_effect(state: &mut TuiState, key: Key) {
    let action = press(state, key);
    match key {
        Key::Ctrl('x') => assert!(state.pending_leader.is_some(), "Ctrl+X opens the leader"),
        Key::Ctrl('q') => assert_eq!(action, InputAction::Quit, "Ctrl+Q quits"),
        other => panic!("{other:?} is not a Global key"),
    }
}

/// A retained fan-out wave of `.0` spokes, for the wave witnesses.
#[derive(Debug)]
pub struct StubWave(pub usize);

impl rustain::domain::ports::wave_handle::WaveHandle for StubWave {
    fn snapshot(&self) -> rustain::domain::ports::wave_handle::WaveSnapshot {
        use rustain::domain::models::orchestration::{ForkJoinOutcome, SynthesisView};
        rustain::domain::ports::wave_handle::WaveSnapshot {
            spoke_count: self.0,
            completed: 0,
            honest_empty: Some(self.0 == 0),
            cancelled: false,
            outcome: ForkJoinOutcome {
                spokes: Vec::new(),
                synthesis: SynthesisView::default(),
            },
            resolve_count: 0,
            slots: Vec::new(),
            rerun_counts: Vec::new(),
        }
    }
    fn cancel(&self) {}
    fn drill(&self, _slot: usize) -> Option<rustain::domain::ports::wave_handle::DrillBody> {
        None
    }
    fn drill_id(&self, _slot: usize) -> Option<rustain::domain::models::orchestration::DrillId> {
        None
    }
    fn slots(&self) -> Vec<rustain::domain::models::AgentId> {
        Vec::new()
    }
    fn rerun_count_for_slot(&self, _slot: usize) -> u8 {
        0
    }
    fn resolve_count(&self) -> usize {
        0
    }
}

// ── 21-7 7-D: the Tasks tab, the task window and the reorder sub-context ──

/// The conversation id of every task fixture.
pub const TASK_CONVERSATION: &str = "conv-1";
/// The plan id of every task fixture.
pub const TASK_PLAN: &str = "plan-1";

/// A conversation with one executing plan of three tasks: 1 completed, 2 pending, 3 failed.
pub fn plan_conversation() -> rustain::domain::models::Conversation {
    use rustain::domain::models::Conversation;
    use rustain::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus, TaskResult};
    let task = |number: u32, status: PlanTaskStatus| PlanTask {
        number,
        title: format!("Task {number}"),
        description: String::new(),
        depends_on: vec![],
        status,
        started_at_ms: None,
        completed_at_ms: None,
        result: (status == PlanTaskStatus::Completed).then(|| TaskResult {
            text: "all green".to_owned(),
            tool_call_count: 1,
            token_count: None,
        }),
        error: (status == PlanTaskStatus::Failed).then(|| "boom".to_owned()),
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    };
    let plan = Plan {
        id: TASK_PLAN.to_owned(),
        title: "Ship".to_owned(),
        tasks: vec![
            task(1, PlanTaskStatus::Completed),
            task(2, PlanTaskStatus::Pending),
            task(3, PlanTaskStatus::Failed),
        ],
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1,
        resolved_at: None,
        host_message_id: None,
        origin_restrictions: None,
    };
    Conversation {
        id: TASK_CONVERSATION.to_owned(),
        title: "Build".to_owned(),
        plans: std::collections::HashMap::from([(TASK_PLAN.to_owned(), plan)]),
        ..Conversation::default()
    }
}

/// The `TaskRef` of task `number` of the fixture plan.
pub fn task_ref(number: u32) -> rustain::adapters::tui::mission_control::tasks::TaskRef {
    rustain::adapters::tui::mission_control::tasks::TaskRef {
        conversation_id: TASK_CONVERSATION.to_owned(),
        plan_id: TASK_PLAN.to_owned(),
        number,
    }
}

/// Mission Control open on the Tasks tab over the fixture plan, rows synced from the
/// conversation (what the host does before a paint), the second task selected and every
/// row action painted whole (what a wide paint of a pending task offers).
pub fn tasks_hub() -> TuiState {
    use rustain::adapters::tui::mission_control::{Fit, McTab, tasks};
    let mut state = fresh();
    rustain::adapters::tui::presentation::reducer::open_mission_control(&mut state, McTab::Tasks);
    state.task_panel_state.last_executed_plan_id = Some(TASK_PLAN.to_owned());
    tasks::sync(&mut state, &plan_conversation());
    state.mission_control.set_fit(Fit {
        list_rows: 3,
        list_viewport: 3,
        open: true,
        pause: true,
        kill: true,
        skip: true,
        copy: true,
        ..Fit::default()
    });
    assert_eq!(press(&mut state, Key::Down), InputAction::Consumed);
    assert_eq!(
        state.mission_control.selected_id(),
        Some("plan-1#2"),
        "the fixture selects the second task"
    );
    state
}

/// The fixture hub while task 2 is being moved.
pub fn reorder_hub() -> TuiState {
    let mut state = tasks_hub();
    state.task_panel_state.reorder = Some(rustain::adapters::tui::state::Reorder {
        conversation_id: TASK_CONVERSATION.to_owned(),
        plan_id: TASK_PLAN.to_owned(),
        number: 2,
        original_order: vec![1, 2, 3],
    });
    state
}

/// The task window open on task `number` of the fixture plan, every row action painted whole.
pub fn task_window_state(number: u32) -> TuiState {
    use rustain::adapters::tui::artifact_window::Live;
    let mut state = fresh();
    assert!(rustain::adapters::tui::task_window::open(
        &mut state,
        &plan_conversation(),
        &task_ref(number)
    ));
    state
        .task_window
        .as_ref()
        .expect("the window is open")
        .text
        .set_live(Live {
            search: true,
            copy: true,
            view: true,
            pause: true,
            kill: false,
            skip: true,
            retry: true,
            edit: true,
        });
    state
}
