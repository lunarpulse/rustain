//! Behavioral regressions for the 19.21e data-flow extraction.
use super::{event_bus::EventBus, panel_bridge::tick_status, task_panel_bridge::*};
use crate::adapters::tui::mission_control::tasks::TaskRef;
use crate::adapters::tui::state::{Direction, TuiState};
use crate::domain::events::AppEvent;
use crate::domain::models::{
    Conversation, NoticeLevel, Plan, PlanStatus, PlanTask, PlanTaskStatus, StatusState,
    generate_conversation_id,
};

fn conversation() -> Conversation {
    let mut conversation = Conversation {
        id: generate_conversation_id(),
        title: "Reorder".into(),
        messages: vec![],
        turns: vec![],
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: Default::default(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    };
    let tasks = (1..=3)
        .map(|number| PlanTask {
            number,
            title: format!("Task {number}"),
            description: String::new(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        })
        .collect();
    conversation.plans.insert(
        "plan".into(),
        Plan {
            id: "plan".into(),
            title: "Reorder".into(),
            tasks,
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1_700_000_000,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        },
    );
    conversation
}

/// `enter_reorder` on task `number` of the fixture plan, as the active tab's host would.
fn enter(
    state: &mut TuiState,
    conversation: &mut Conversation,
    bus: &EventBus,
    number: u32,
) -> bool {
    let task = TaskRef {
        conversation_id: conversation.id.clone(),
        plan_id: "plan".into(),
        number,
    };
    let mut ctx = TaskCtx {
        state,
        conversation,
        read_only: false,
        tabs: &[],
        event_bus: bus,
    };
    enter_reorder(&mut ctx, &task)
}

fn order(conversation: &Conversation) -> Vec<u32> {
    conversation.plans["plan"]
        .tasks
        .iter()
        .map(|task| task.number)
        .collect()
}

#[test]
fn edited_description_preserves_indented_comments_blank_lines_and_title_fallback() {
    let (title, lines) = parse_task_edit(
        "# editor guidance\nFallback title\ndescription: |\n  first\n  # task content\n\n  final\nend\n",
    );
    assert_eq!(title, "Fallback title");
    assert_eq!(lines, ["first", "# task content", "", "final"]);
    let (title, lines) = parse_task_edit("# comment only\n\n");
    assert!(title.is_empty());
    assert!(lines.is_empty());
}

#[test]
fn cancel_restores_original_order_after_multiple_moves() {
    let mut conversation = conversation();
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some("plan".into());
    let (bus, _events) = EventBus::new(16);
    assert!(enter(&mut state, &mut conversation, &bus, 2));
    assert!(move_reorder(
        &mut state,
        &mut conversation,
        Direction::Up,
        &bus
    ));
    assert_eq!(order(&conversation), [2, 1, 3]);
    assert!(move_reorder(
        &mut state,
        &mut conversation,
        Direction::Down,
        &bus
    ));
    assert!(move_reorder(
        &mut state,
        &mut conversation,
        Direction::Down,
        &bus
    ));
    assert_eq!(order(&conversation), [1, 3, 2]);
    cancel_reorder(&mut state, &mut conversation);
    assert_eq!(order(&conversation), [1, 2, 3]);
    assert!(state.task_panel_state.reorder.is_none());
}

#[test]
fn dependency_violation_refuses_move_without_reordering() {
    let mut conversation = conversation();
    conversation.plans.get_mut("plan").unwrap().tasks[1].depends_on = vec![1];
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some("plan".into());
    let (bus, mut events) = EventBus::new(16);
    assert!(enter(&mut state, &mut conversation, &bus, 2));
    assert!(move_reorder(
        &mut state,
        &mut conversation,
        Direction::Up,
        &bus
    ));
    assert_eq!(order(&conversation), [1, 2, 3]);
    assert!(
        matches!(events.try_recv().unwrap(), AppEvent::SystemNotice {
        level: NoticeLevel::Warning, conversation_id: Some(id), ..
    } if id == conversation.id)
    );
}

#[test]
fn boundary_move_does_not_change_order_and_commit_survives_later_cancel() {
    let mut conversation = conversation();
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some("plan".into());
    let (bus, _events) = EventBus::new(16);
    assert!(enter(&mut state, &mut conversation, &bus, 1));
    assert!(!move_reorder(
        &mut state,
        &mut conversation,
        Direction::Up,
        &bus
    ));
    assert_eq!(order(&conversation), [1, 2, 3]);
    assert!(move_reorder(
        &mut state,
        &mut conversation,
        Direction::Down,
        &bus
    ));
    commit_reorder(&mut state, &conversation, &bus);
    cancel_reorder(&mut state, &mut conversation);
    assert_eq!(order(&conversation), [2, 1, 3]);
}

#[test]
fn story21_1_tick_requests_a_redraw_only_when_a_toast_deadline_passed() {
    use crate::adapters::tui::notifications::{
        self, NoticeSeverity, NoticeSource, NotificationStore,
    };
    use crate::domain::clock::MockClock;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let conversation = conversation();
    let mut state = TuiState::new(160, 24);
    let clock = Arc::new(MockClock::new(Instant::now()));
    state.notifications = NotificationStore::new(
        clock.clone(),
        crate::domain::models::NotificationsConfig::default().clamped(),
    );
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Info,
        "temporary",
    );
    state.needs_redraw = false;

    // A tick before the deadline neither redraws nor changes the toast: no
    // per-tick decrement exists any more (A4).
    tick_status(&mut state, &conversation, 50);
    assert!(!state.needs_redraw);
    assert_eq!(state.notifications.visible().count(), 1);

    // Once the deadline has passed the tick asks for the redraw whose
    // `begin_draw` sweeps the toast; the record stays for the log.
    clock.advance(Duration::from_secs(60));
    tick_status(&mut state, &conversation, 150);
    assert!(state.needs_redraw);
    notifications::begin_draw(&mut state);
    assert_eq!(state.notifications.visible().count(), 0);
    assert_eq!(state.notifications.records().count(), 1);
    state.needs_redraw = false;
    tick_status(&mut state, &conversation, 150);
    assert!(
        !state.needs_redraw,
        "an expired toast asks for no more redraws"
    );
}

#[test]
fn story21_9_tick_requests_a_redraw_while_the_model_is_thinking() {
    // Review P4: the live `◇ Thinking… · <elapsed>` row must advance between
    // chunks, so a thinking buffer makes every tick ask for a draw.
    let mut state = TuiState::new(160, 24);
    let mut streaming = crate::domain::models::StreamingState::default();
    state.needs_redraw = false;
    super::panel_bridge::tick_live_reasoning(&mut state, &streaming);
    assert!(!state.needs_redraw, "nothing thinks: no redraw");
    streaming.thinking_buffer.push('x');
    super::panel_bridge::tick_live_reasoning(&mut state, &streaming);
    assert!(
        state.needs_redraw,
        "thinking: the elapsed segment must repaint"
    );
}

#[test]
fn story21_1_ticks_leave_the_typed_status_facts_alone() {
    // Old (S1): the delete-confirmation Flash was the one status flash left and
    // the tick left it alone. New (S2, AC13): `StatusState::Flash` is deleted and
    // the tick never rewrites a status fact either.
    let conversation = conversation();
    let mut state = TuiState::new(160, 24);
    for status in [
        StatusState::Idle,
        StatusState::Streaming,
        StatusState::Retrying {
            attempt: 1,
            max: 5,
            next_in_ms: 1_000,
        },
    ] {
        state.status = status.clone();
        tick_status(&mut state, &conversation, 250);
        assert_eq!(state.status, status);
    }
}

// ── Story 21.7: a task action is resolved in the conversation its ref names (A11, PC-3) ──

/// What the plan holds, for "unchanged" comparisons: `(number, status)` in order.
fn plan_shape(conversation: &Conversation) -> Vec<(u32, PlanTaskStatus)> {
    conversation.plans["plan"]
        .tasks
        .iter()
        .map(|task| (task.number, task.status))
        .collect()
}

/// Run every task action in `ctx` on a ref and assert each one refused: no follow-up, no
/// decision, no bus event (so no `PlanTaskStatusChanged`), the plan untouched, one Warning
/// whose text is `warning`.
fn assert_every_task_action_refused(
    conversation_id_of_ref: &str,
    read_only: bool,
    tabs: &[crate::domain::models::TabState],
    warning: &str,
) {
    let mut conversation = conversation();
    conversation.plans.get_mut("plan").unwrap().tasks[0].status = PlanTaskStatus::Failed;
    let before = plan_shape(&conversation);
    let mut state = TuiState::new(160, 24);
    let (bus, mut events) = EventBus::new(16);
    let task = TaskRef {
        conversation_id: conversation_id_of_ref.to_owned(),
        plan_id: "plan".into(),
        number: 1,
    };
    let mut ctx = TaskCtx {
        state: &mut state,
        conversation: &mut conversation,
        read_only,
        tabs,
        event_bus: &bus,
    };
    assert!(pause(&mut ctx, &task).is_none(), "pause refused");
    assert!(skip(&mut ctx, &task).is_none(), "skip refused");
    assert!(retry(&mut ctx, &task).is_none(), "retry refused");
    cancel_plan(&mut ctx, &task);

    assert_eq!(plan_shape(&conversation), before, "the plan is unchanged");
    assert_eq!(
        conversation.plans["plan"].status,
        PlanStatus::Executing,
        "cancel_plan did not cancel"
    );
    assert!(
        events.try_recv().is_err(),
        "a refused action sends no event (no PlanTaskStatusChanged)"
    );
    assert_eq!(
        state.decisions.items().count(),
        0,
        "cancel_plan raised no confirmation"
    );
    let warnings: Vec<_> = state
        .notifications
        .records()
        .filter(|record| record.message == warning)
        .collect();
    assert!(!warnings.is_empty(), "a Warning says: {warning}");
    assert!(warnings.iter().all(|record| {
        record.severity == crate::adapters::tui::notifications::NoticeSeverity::Warning
    }));
    assert_eq!(
        warnings.iter().map(|record| record.count).sum::<u32>(),
        4,
        "each of the four actions was refused"
    );
}

#[test]
fn story21_7_admit_refuses_a_ref_of_another_conversation_for_pause_skip_retry_and_cancel_plan() {
    use crate::domain::models::TabState;
    let mut other = TabState::new(2);
    other.conversation.id = "other-conversation".into();
    other.conversation.title = "Elsewhere".into();
    assert_every_task_action_refused(
        "other-conversation",
        false,
        &[other],
        "Open Elsewhere to change its tasks.",
    );
}

#[test]
fn story21_7_admit_names_an_unknown_conversation_generically() {
    assert_every_task_action_refused(
        "gone",
        false,
        &[],
        "Open that conversation to change its tasks.",
    );
}

#[test]
fn story21_7_admit_refuses_every_task_action_on_a_read_only_tab() {
    let own = conversation().id;
    assert_every_task_action_refused(&own, true, &[], super::mission_bridge::READ_ONLY);
}

#[test]
fn story21_7_admit_lets_the_active_conversations_own_ref_through() {
    let mut conversation = conversation();
    let mut state = TuiState::new(160, 24);
    let (bus, _events) = EventBus::new(16);
    let task = TaskRef {
        conversation_id: conversation.id.clone(),
        plan_id: "plan".into(),
        number: 1,
    };
    let mut ctx = TaskCtx {
        state: &mut state,
        conversation: &mut conversation,
        read_only: false,
        tabs: &[],
        event_bus: &bus,
    };
    assert!(ctx.admit(&task));
    assert_eq!(state.notifications.records().count(), 0);
}
