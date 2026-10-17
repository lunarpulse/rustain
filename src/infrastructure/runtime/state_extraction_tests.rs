//! Behavioral regressions for the 19.21e data-flow extraction.
use super::{event_bus::EventBus, panel_bridge::tick_status, task_panel_bridge::*};
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
        },
    );
    conversation
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
    assert!(enter_reorder(&mut state, &conversation, 2, &bus));
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
    assert!(state.task_panel_state.reorder_mode_for.is_none());
}

#[test]
fn dependency_violation_refuses_move_without_reordering() {
    let mut conversation = conversation();
    conversation.plans.get_mut("plan").unwrap().tasks[1].depends_on = vec![1];
    let mut state = TuiState::new(160, 24);
    state.task_panel_state.last_executed_plan_id = Some("plan".into());
    let (bus, mut events) = EventBus::new(16);
    assert!(enter_reorder(&mut state, &conversation, 2, &bus));
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
    assert!(enter_reorder(&mut state, &conversation, 1, &bus));
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
fn finite_flash_expires_but_sticky_warning_survives_ticks() {
    let conversation = conversation();
    let mut state = TuiState::new(160, 24);
    state.status_before_flash = Some(StatusState::Idle);
    state.status = StatusState::Flash {
        message: "temporary".into(),
        remaining_ms: 200,
    };
    tick_status(&mut state, &conversation, 50);
    assert!(matches!(
        state.status,
        StatusState::Flash {
            remaining_ms: 150,
            ..
        }
    ));
    tick_status(&mut state, &conversation, 150);
    assert!(matches!(state.status, StatusState::Idle));
    state.status = StatusState::Flash {
        message: "sticky".into(),
        remaining_ms: u64::MAX,
    };
    tick_status(&mut state, &conversation, 250);
    assert!(matches!(
        state.status,
        StatusState::Flash {
            remaining_ms: u64::MAX,
            ..
        }
    ));
}
