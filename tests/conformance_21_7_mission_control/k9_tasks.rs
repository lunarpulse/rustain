//! K9 / AC16-AC18 / A8 / A11 / A17 (21-7 slice 7-D): the Tasks tab, the task window and the
//! reorder sub-context. The plan is a real `Plan` in the active conversation; keys travel as
//! crossterm events through the local host's decoder; task actions reach the plan only through
//! the production effect functions the event loop calls (`task_panel_bridge::{pause, skip,
//! retry, cancel_plan, enter_reorder, move_reorder, commit_reorder, cancel_reorder}`),
//! observed on the event bus and in the plan; every assertion reads painted cells, the one
//! frame on the stack, the decision queue, the plan or the clipboard closure. No sleeps.
use super::support::*;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::clipboard::ClipboardResult;
use rustain::adapters::tui::decision_queue::DecisionKind;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::mission_control::tasks::TaskRef;
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::state::Direction;
use rustain::domain::models::plan::{
    Plan, PlanStatus, PlanSubTask, PlanTask, PlanTaskStatus, TaskResult,
};
use rustain::infrastructure::runtime::event_bus::EventBus;
use rustain::infrastructure::runtime::task_panel_bridge::{self as bridge, TaskCtx};
use std::sync::{Arc, Mutex};

fn task(number: u32, title: &str, status: PlanTaskStatus) -> PlanTask {
    PlanTask {
        number,
        title: title.to_owned(),
        description: String::new(),
        depends_on: vec![],
        status,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

/// 1 completed (a markdown result), 2 failed, 3 pending after 2 with two sub-tasks, 4 pending.
fn plan() -> Plan {
    let mut done = task(1, "Compile", PlanTaskStatus::Completed);
    done.result = Some(TaskResult {
        text: "**built** ok".to_owned(),
        tool_call_count: 3,
        token_count: Some(40),
    });
    let mut failed = task(2, "Test", PlanTaskStatus::Failed);
    failed.error = Some("boom".to_owned());
    let mut next = task(3, "Ship", PlanTaskStatus::Pending);
    next.depends_on = vec![2];
    let sub = |number: u32| PlanSubTask {
        number,
        title: format!("part {number}"),
        description: String::new(),
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        delegated_to: None,
    };
    next.sub_tasks = vec![sub(1), sub(2)];
    Plan {
        id: "p1".to_owned(),
        title: "Release".to_owned(),
        tasks: vec![
            done,
            failed,
            next,
            task(4, "Announce", PlanTaskStatus::Pending),
        ],
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1,
        resolved_at: None,
        host_message_id: None,
    }
}

async fn tasks_rig(width: u16, height: u16) -> Rig {
    let mut rig = Rig::plain(Workspace::new().await, width, height).await;
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .insert("p1".to_owned(), plan());
    rig.f.h.state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
    assert_eq!(rig.f.press(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(
        rig.f.press(Key::Char('t')),
        InputAction::OpenMissionControl(McTab::Tasks)
    );
    rig.f.paint();
    rig
}

fn conv_id(rig: &Rig) -> String {
    rig.f.conversation().id.clone()
}

fn task_ref(rig: &Rig, number: u32) -> TaskRef {
    TaskRef {
        conversation_id: conv_id(rig),
        plan_id: "p1".to_owned(),
        number,
    }
}

fn tref(conversation: &rustain::domain::models::Conversation, number: u32) -> TaskRef {
    TaskRef {
        conversation_id: conversation.id.clone(),
        plan_id: "p1".to_owned(),
        number,
    }
}

fn selected_rows(rig: &Rig) -> Vec<String> {
    body_lines(rig)
        .into_iter()
        .filter(|line| line.contains("▸ "))
        .collect()
}

fn warned(rig: &Rig, needle: &str) -> bool {
    rig.f
        .h
        .state
        .notifications
        .records()
        .any(|record| record.message.contains(needle))
}

fn statuses(conversation: &rustain::domain::models::Conversation) -> Vec<PlanTaskStatus> {
    conversation.plans["p1"]
        .tasks
        .iter()
        .map(|t| t.status)
        .collect()
}

#[tokio::test]
async fn the_tasks_tab_paints_every_row_it_can_reach_with_sub_tasks_at_every_size() {
    for (width, height) in [(60, 16), (80, 24), (130, 30)] {
        let mut rig = tasks_rig(width, height).await;
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
        assert_eq!(top_title(&rig).as_deref(), Some("Mission Control · Tasks"));
        // Five list rows: four tasks and two sub-tasks of task 3 are six; each painted row is
        // one line (DF-CR-10-6-2: height == paint).
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..8 {
            let marked = selected_rows(&rig);
            assert_eq!(
                marked.len(),
                1,
                "{width}x{height}: exactly one selected row: {marked:?}"
            );
            seen.insert(marked[0].clone());
            rig.f.press(Key::Char('j'));
            rig.f.paint();
        }
        let wanted = ["Compile", "Test", "Ship", "part 1", "part 2", "Announce"];
        for label in wanted {
            assert!(
                seen.iter().any(|row| row.contains(label)),
                "{width}x{height}: `{label}` was never reachable: {seen:?}"
            );
        }
    }
}

#[tokio::test]
async fn enter_p_s_and_x_carry_the_task_ref_of_the_selected_row() {
    let mut rig = tasks_rig(130, 30).await;
    // Rows: 1 Compile, 2 Test, 3 Ship, 3.1, 3.2, 4 Announce. Select `Ship`.
    for _ in 0..2 {
        rig.f.press(Key::Char('j'));
    }
    rig.f.paint();
    let ship = task_ref(&rig, 3);
    assert_eq!(
        rig.f.press(Key::Char('p')),
        InputAction::TaskPause(ship.clone())
    );
    assert_eq!(
        rig.f.press(Key::Char('s')),
        InputAction::TaskSkip(ship.clone())
    );
    assert_eq!(
        rig.f.press(Key::Char('x')),
        InputAction::TaskCancelPlan(ship.clone())
    );
    assert_eq!(
        rig.f.press(Key::Enter),
        InputAction::OpenTaskWindow(ship.clone())
    );
    // A sub-task row resolves to its parent task.
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    assert_eq!(rig.f.press(Key::Enter), InputAction::OpenTaskWindow(ship));
    // A completed task offers neither `p` nor `s` (the rail does not paint them: inert keys).
    rig.f.press(Key::Char('g'));
    rig.f.paint();
    assert_eq!(rig.f.press(Key::Char('p')), InputAction::Consumed);
    assert_eq!(rig.f.press(Key::Char('s')), InputAction::Consumed);
    assert_eq!(
        rig.f.press(Key::Enter),
        InputAction::OpenTaskWindow(task_ref(&rig, 1))
    );
}

#[tokio::test]
async fn task_action_after_a_tab_switch_is_refused_and_sends_nothing() {
    let mut rig = tasks_rig(80, 24).await;
    let before = rig.f.conversation().clone();
    let mut conversation = before.clone();
    let (bus, mut events) = EventBus::new(16);
    let elsewhere = TaskRef {
        conversation_id: "another-conversation".to_owned(),
        plan_id: "p1".to_owned(),
        number: 3,
    };
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        assert!(bridge::pause(&mut ctx, &elsewhere).is_none());
        assert!(bridge::skip(&mut ctx, &elsewhere).is_none());
        assert!(
            bridge::retry(
                &mut ctx,
                &TaskRef {
                    number: 2,
                    ..elsewhere.clone()
                }
            )
            .is_none()
        );
        bridge::cancel_plan(&mut ctx, &elsewhere);
        assert!(!bridge::enter_reorder(&mut ctx, &elsewhere));
    }
    assert!(
        warned(&rig, "to change its tasks."),
        "the refusal says which tab to open"
    );
    assert_eq!(
        statuses(&conversation),
        statuses(&before),
        "no task changed"
    );
    assert!(events.try_recv().is_err(), "nothing was sent");
    assert_eq!(
        rig.f.h.state.decisions.count_live(DecisionKind::CancelPlan),
        0
    );
    assert!(rig.f.h.state.task_panel_state.reorder.is_none());
    // Positive control: the same action for a task of THIS conversation runs.
    let own = task_ref(&rig, 3);
    let follow = {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        bridge::pause(&mut ctx, &own)
    };
    assert!(follow.is_some());
    assert_eq!(
        conversation.plans["p1"].tasks[2].status,
        PlanTaskStatus::Paused
    );
    assert!(events.try_recv().is_ok(), "the status change was sent");
}

#[tokio::test]
async fn read_only_tab_task_actions_refuse_and_send_nothing() {
    let mut rig = tasks_rig(80, 24).await;
    rig.f.tabs.active_tab_mut().read_only = true;
    let before = rig.f.conversation().clone();
    let mut conversation = before.clone();
    let (bus, mut events) = EventBus::new(16);
    let failed = task_ref(&rig, 2);
    let pending = task_ref(&rig, 3);
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        assert!(bridge::pause(&mut ctx, &pending).is_none());
        assert!(
            bridge::skip(&mut ctx, &pending).is_none(),
            "`s` Skip is guarded like the rest"
        );
        assert!(bridge::retry(&mut ctx, &failed).is_none());
        assert!(bridge::edit(&mut ctx, &failed).is_none());
        bridge::cancel_plan(&mut ctx, &pending);
        assert!(!bridge::enter_reorder(&mut ctx, &pending));
    }
    assert!(warned(
        &rig,
        "This tab shows subagent output and is read-only. Switch to a conversation tab to make changes."
    ));
    assert_eq!(statuses(&conversation), statuses(&before));
    assert!(events.try_recv().is_err());
    assert_eq!(
        rig.f.h.state.decisions.count_live(DecisionKind::CancelPlan),
        0
    );
    assert_eq!(
        rig.f
            .h
            .state
            .decisions
            .count_live(DecisionKind::SkipCascade),
        0
    );
}

#[tokio::test]
async fn x_and_s_raise_the_decisions_and_never_mutate_directly() {
    let mut rig = tasks_rig(80, 24).await;
    let mut conversation = rig.f.conversation().clone();
    let (bus, _events) = EventBus::new(16);
    let own = task_ref(&rig, 2);
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        bridge::cancel_plan(&mut ctx, &own);
    }
    assert_eq!(
        rig.f.h.state.decisions.count_live(DecisionKind::CancelPlan),
        1
    );
    assert_eq!(
        conversation.plans["p1"].status,
        PlanStatus::Executing,
        "nothing cancelled yet"
    );
    // Task 3 depends on the failed task 2: skipping it raises the cascade decision and the
    // plan does not advance until it is answered.
    let follow = {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        bridge::skip(&mut ctx, &own).expect("admitted")
    };
    assert!(!follow.advance);
    assert_eq!(
        rig.f
            .h
            .state
            .decisions
            .count_live(DecisionKind::SkipCascade),
        1
    );
}

#[tokio::test]
async fn story21_14_reordering_a_task_that_is_not_pending_says_what_to_select() {
    let mut rig = tasks_rig(80, 24).await;
    let mut conversation = rig.f.conversation().clone();
    let (bus, mut events) = EventBus::new(16);
    let failed = task_ref(&rig, 2);
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        assert!(!bridge::enter_reorder(&mut ctx, &failed));
    }
    assert!(rig.f.h.state.task_panel_state.reorder.is_none());
    match events.try_recv().expect("the refusal is announced") {
        rustain::domain::events::AppEvent::SystemNotice { level, message, .. } => {
            assert_eq!(level, rustain::domain::models::NoticeLevel::Warning);
            assert_eq!(
                message,
                "Could not start reordering because no pending task is selected. Select a pending task, then try again."
            );
        }
        other => panic!("expected a notice, got {other:?}"),
    }
}

#[tokio::test]
async fn reorder_is_a_sub_context_of_the_tab_and_wave_keys_in_chat_are_unaffected() {
    let mut rig = tasks_rig(80, 24).await;
    let mut conversation = rig.f.conversation().clone();
    let (bus, _events) = EventBus::new(16);
    let announce = task_ref(&rig, 4);
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        assert!(bridge::enter_reorder(&mut ctx, &announce));
    }
    rig.f.paint();
    assert!(rustain::adapters::tui::mission_control::reordering(
        &rig.f.h.state
    ));
    assert!(rail_line(&rig).contains("Commit"), "{}", rail_line(&rig));
    assert!(
        body_lines(&rig).iter().any(|line| line.contains('↕')),
        "the moved row is marked"
    );
    // Only the reorder rows act; everything else is inert and the tab does not change.
    assert_eq!(rig.f.press(Key::Char('x')), InputAction::Consumed);
    assert_eq!(rig.f.press(Key::Char('p')), InputAction::Consumed);
    assert_eq!(rig.f.press(Key::Right), InputAction::Consumed);
    assert_eq!(rig.f.h.state.mission_control.tab, McTab::Tasks);
    assert_eq!(
        rig.f.press(Key::Char('k')),
        InputAction::TaskReorderMove(Direction::Up)
    );
    assert_eq!(
        rig.f.press(Key::Down),
        InputAction::TaskReorderMove(Direction::Down)
    );
    // Moving up over task 3 (no dependency between 3 and 4) applies; moving up over task 2
    // would break nothing for 4 either; cancel restores the original order exactly.
    assert!(bridge::move_reorder(
        &mut rig.f.h.state,
        &mut conversation,
        Direction::Up,
        &bus
    ));
    let order = |c: &rustain::domain::models::Conversation| -> Vec<u32> {
        c.plans["p1"].tasks.iter().map(|t| t.number).collect()
    };
    assert_eq!(order(&conversation), vec![1, 2, 4, 3]);
    assert_eq!(rig.f.press(Key::Esc), InputAction::TaskReorderCancel);
    bridge::cancel_reorder(&mut rig.f.h.state, &mut conversation);
    assert_eq!(order(&conversation), vec![1, 2, 3, 4]);
    assert!(!rustain::adapters::tui::mission_control::reordering(
        &rig.f.h.state
    ));
    // Enter commits.
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        assert!(bridge::enter_reorder(&mut ctx, &announce));
    }
    assert!(bridge::move_reorder(
        &mut rig.f.h.state,
        &mut conversation,
        Direction::Up,
        &bus
    ));
    assert_eq!(rig.f.press(Key::Enter), InputAction::TaskReorderCommit);
    bridge::commit_reorder(&mut rig.f.h.state, &conversation, &bus);
    assert_eq!(order(&conversation), vec![1, 2, 4, 3]);
    assert!(rig.f.h.state.task_panel_state.reorder.is_none());
    // The Chat wave keys are the registry's own rows: no reorder state exists to shadow them.
    use rustain::adapters::tui::keymap::{CommandId, Context, Host, Resolved, resolve};
    for key in ['d', 's', 'r'] {
        assert!(
            matches!(
                resolve(Context::ChatWave, Host::Local, &[Key::Char(key)]),
                Resolved::Hit(row) if matches!(row.command, CommandId::Panel(_))
            ),
            "{key}"
        );
    }
    assert!(matches!(
        resolve(Context::ChatWave, Host::Local, &[Key::Enter]),
        Resolved::Hit(_)
    ));
}

#[tokio::test]
async fn the_task_window_shows_facts_and_a_rendered_result_and_acts_by_status() {
    let mut rig = tasks_rig(130, 30).await;
    let conversation = rig.f.conversation().clone();
    // Completed task: rendered result, copy only.
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 1)
    ));
    rig.f.paint();
    assert_eq!(top_surface(&rig), Some(Surface::TaskWindow));
    assert_eq!(top_title(&rig).as_deref(), Some("Task 1 · Compile"));
    let text = lines(&rig).join("\n");
    assert!(text.contains("Task 1. Compile"), "{text}");
    assert!(
        text.contains("built ok") && !text.contains("**built**"),
        "rendered: {text}"
    );
    assert!(rail_line(&rig).contains("Copy") && !rail_line(&rig).contains("Retry"));
    assert_eq!(rig.f.press(Key::Char('r')), InputAction::Consumed);
    assert_eq!(rig.f.press(Key::Char('s')), InputAction::Consumed);
    // Failed task: r, s and e are painted and act on the frame's TaskRef; the error is literal.
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 2)
    ));
    rig.f.paint();
    let rail = rail_line(&rig);
    for hint in ["Retry", "Skip", "Edit", "Copy"] {
        assert!(rail.contains(hint), "{rail}");
    }
    let failed = task_ref(&rig, 2);
    assert_eq!(
        rig.f.press(Key::Char('r')),
        InputAction::TaskRetry(failed.clone())
    );
    assert_eq!(
        rig.f.press(Key::Char('s')),
        InputAction::TaskSkip(failed.clone())
    );
    assert_eq!(rig.f.press(Key::Char('e')), InputAction::TaskEdit(failed));
    assert_eq!(rig.f.press(Key::Char('p')), InputAction::Consumed);
    // `c` copies the original error text with its own wording.
    let sink = Arc::new(Mutex::new(Vec::new()));
    let seen = sink.clone();
    rig.f.h.state.clipboard_copier = Some(Arc::new(move |text| {
        seen.lock().unwrap().push(text.to_owned());
        ClipboardResult::Osc52Success
    }));
    rig.f.press(Key::Char('c'));
    assert_eq!(*sink.lock().unwrap(), vec!["boom".to_owned()]);
    assert!(warned(&rig, "Copied error to clipboard"));
    rig.f.press(Key::Esc);
    rig.f.paint();
    assert_eq!(
        top_surface(&rig),
        Some(Surface::MissionControl),
        "Esc returns to the hub"
    );
}

#[tokio::test]
async fn copying_a_cancelled_task_has_its_own_wording() {
    let mut rig = tasks_rig(100, 30).await;
    {
        let tasks = &mut rig
            .f
            .tabs
            .active_tab_mut()
            .conversation
            .plans
            .get_mut("p1")
            .unwrap()
            .tasks;
        tasks[3].status = PlanTaskStatus::Cancelled;
        tasks[3].error = Some("stopped by the user".to_owned());
    }
    let conversation = rig.f.conversation().clone();
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 4)
    ));
    rig.f.paint();
    let sink = Arc::new(Mutex::new(Vec::new()));
    let seen = sink.clone();
    rig.f.h.state.clipboard_copier = Some(Arc::new(move |text| {
        seen.lock().unwrap().push(text.to_owned());
        ClipboardResult::Osc52Success
    }));
    rig.f.press(Key::Char('c'));
    assert_eq!(
        *sink.lock().unwrap(),
        vec!["stopped by the user".to_owned()]
    );
    assert!(warned(&rig, "Copied cancel reason to clipboard"));
}

#[tokio::test]
async fn a_vanished_task_shows_the_stale_copy_and_a_stage_reset_pops_the_window() {
    let mut rig = tasks_rig(100, 30).await;
    let conversation = rig.f.conversation().clone();
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 2)
    ));
    rig.f.paint();
    rig.f.tabs.active_tab_mut().conversation.plans.clear();
    rig.f.paint();
    assert_eq!(
        top_surface(&rig),
        Some(Surface::TaskWindow),
        "the window stays"
    );
    let text = lines(&rig).join("\n");
    assert!(text.contains("This task is no longer available."), "{text}");
    assert!(text.contains("boom"), "the last result is kept: {text}");
    assert!(
        !rail_line(&rig).contains("Retry"),
        "a gone task offers no action"
    );
    // Reset/close/fork/rewind of the conversation replace the Stage: the frame is popped.
    reducer::reconcile_stage(&mut rig.f.h.state);
    assert!(rig.f.h.state.task_window.is_none());
    assert!(!is_open(&rig, Surface::TaskWindow));
}

#[tokio::test]
async fn a_removed_selected_task_shows_unavailable_and_is_not_rebound() {
    let mut rig = tasks_rig(80, 24).await;
    rig.f.press(Key::Char('j'));
    rig.f.paint();
    assert!(selected_rows(&rig)[0].contains("Test"));
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .get_mut("p1")
        .unwrap()
        .tasks
        .remove(1);
    rig.f.paint();
    let marked = selected_rows(&rig);
    assert_eq!(marked.len(), 1);
    assert!(marked[0].contains("no longer available"), "{marked:?}");
    assert_eq!(
        rig.f.press(Key::Enter),
        InputAction::Consumed,
        "nothing opens on a gone row"
    );
}

// ── patch round 1 (review-7d.md) ──────────────────────────────────────────────────────────

#[tokio::test]
async fn unchanged_repaints_rebuild_nothing_and_a_change_rebuilds_once() {
    // 7D-02: a 1 MiB result is neither re-listed nor re-cloned by a repaint of the open window.
    let mut rig = tasks_rig(100, 30).await;
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .get_mut("p1")
        .unwrap()
        .tasks[0]
        .result = Some(TaskResult {
        text: "x".repeat(1 << 20),
        tool_call_count: 1,
        token_count: None,
    });
    rig.f.paint();
    let conversation = rig.f.conversation().clone();
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 1)
    ));
    rig.f.paint();
    let builds = rig.f.h.state.mission_control.tasks_builds;
    let layouts = rig
        .f
        .h
        .state
        .task_window
        .as_ref()
        .unwrap()
        .text
        .layouts_built();
    for _ in 0..5 {
        rig.f.paint();
    }
    assert_eq!(
        rig.f.h.state.mission_control.tasks_builds, builds,
        "no list rebuild"
    );
    assert_eq!(
        rig.f
            .h
            .state
            .task_window
            .as_ref()
            .unwrap()
            .text
            .layouts_built(),
        layouts,
        "no re-layout of the unchanged body"
    );
    // A real change (the result replaced) is followed exactly once.
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .get_mut("p1")
        .unwrap()
        .tasks[0]
        .result = Some(TaskResult {
        text: "changed".to_owned(),
        tool_call_count: 1,
        token_count: None,
    });
    rig.f.paint();
    rig.f.paint();
    assert_eq!(rig.f.h.state.mission_control.tasks_builds, builds + 1);
    assert!(lines(&rig).join("\n").contains("changed"));
}

#[tokio::test]
async fn a_reorder_armed_in_another_conversation_is_rolled_back_and_never_captures_keys() {
    // 7D-03: a decision switched tabs while A's reorder was armed.
    let mut rig = tasks_rig(80, 24).await;
    let (bus, _events) = EventBus::new(16);
    let a = rig.f.conversation().clone();
    let announce = tref(&a, 4);
    {
        let tab = rig.f.tabs.active_tab_mut();
        let mut ctx = TaskCtx {
            state: &mut rig.f.h.state,
            conversation: &mut tab.conversation,
            read_only: false,
            tabs: &[],
            event_bus: &bus,
        };
        assert!(bridge::enter_reorder(&mut ctx, &announce));
    }
    assert!(bridge::move_reorder(
        &mut rig.f.h.state,
        &mut rig.f.tabs.active_tab_mut().conversation,
        Direction::Up,
        &bus
    ));
    let order = |c: &rustain::domain::models::Conversation| -> Vec<u32> {
        c.plans["p1"].tasks.iter().map(|t| t.number).collect()
    };
    assert_eq!(
        order(rig.f.conversation()),
        vec![1, 2, 4, 3],
        "provisional order"
    );
    // B becomes the active conversation: its own Tasks tab is painted.
    let b = rustain::domain::models::Conversation {
        id: "conversation-b".to_owned(),
        ..rustain::domain::models::Conversation::default()
    };
    rustain::adapters::tui::mission_control::tasks::sync(&mut rig.f.h.state, &b);
    assert!(
        !rustain::adapters::tui::mission_control::reordering(&rig.f.h.state),
        "A's reorder must not capture B's keys"
    );
    assert_ne!(rig.f.press(Key::Enter), InputAction::TaskReorderCommit);
    assert_ne!(rig.f.press(Key::Esc), InputAction::TaskReorderCancel);
    // The host settles it before the next key: A's order is restored in A's own tab.
    rig.f
        .h
        .state
        .task_panel_state
        .reorder
        .as_ref()
        .expect("still armed until settled");
    let settled = bridge::settle_reorder_origin(&mut rig.f.h.state, &b, rig.f.tabs.tabs_mut());
    assert!(settled);
    assert!(rig.f.h.state.task_panel_state.reorder.is_none());
    assert_eq!(
        order(rig.f.conversation()),
        vec![1, 2, 3, 4],
        "A's order restored"
    );
    assert!(warned(&rig, "original task order was restored"));
    // Nothing armed, nothing to settle.
    assert!(!bridge::settle_reorder_origin(
        &mut rig.f.h.state,
        &b,
        rig.f.tabs.tabs_mut()
    ));
}

#[tokio::test]
async fn error_skip_and_cancel_bodies_are_literal_and_never_offered_as_markdown() {
    // 7D-04: the failed task's error stays Source; `v` is neither painted nor live.
    let mut rig = tasks_rig(100, 30).await;
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .get_mut("p1")
        .unwrap()
        .tasks[1]
        .error = Some("**not markdown**".to_owned());
    let conversation = rig.f.conversation().clone();
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 2)
    ));
    rig.f.paint();
    assert!(!rail_line(&rig).contains("Rendered") && !rail_line(&rig).contains("Source"));
    rig.f.press(Key::Char('v'));
    rig.f.paint();
    assert!(
        lines(&rig).join("\n").contains("**not markdown**"),
        "still literal"
    );
    // Positive control: a completed markdown result keeps the toggle.
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &tref(&conversation, 1)
    ));
    rig.f.paint();
    assert!(rail_line(&rig).contains("Source"), "{}", rail_line(&rig));
    rig.f.press(Key::Char('v'));
    rig.f.paint();
    assert!(
        lines(&rig).join("\n").contains("**built**"),
        "Source shows the markers"
    );
}

#[tokio::test]
async fn the_palette_reorder_enters_the_selected_task_and_without_a_selection_says_how() {
    // 7D-09: the palette front door, through palette close and dispatch.
    use rustain::adapters::tui::keymap::Key as K;
    use rustain::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};
    let entry = || PaletteEntry {
        name: "!reorder-task".into(),
        description: String::new(),
        shortcut: None,
        scope: PaletteScope::All,
        action: PaletteAction::ExecuteCommand("reorder-task".into(), None),
    };
    let mut rig = tasks_rig(100, 30).await;
    // Rows: 1, 2, 3, 3.1, 3.2, 4: select `Announce` (row index 5).
    for _ in 0..5 {
        rig.f.press(K::Char('j'));
    }
    rig.f.paint();
    rig.f.press(K::Ctrl('p'));
    rig.f.h.state.command_palette.filtered_entries.push(entry());
    let action = rig.f.press(K::Enter);
    let announce = task_ref(&rig, 4);
    assert_eq!(action, InputAction::TaskReorderEnter(announce.clone()));
    let mut conversation = rig.f.conversation().clone();
    let (bus, _events) = EventBus::new(16);
    {
        let mut ctx = TaskCtx::for_host(&mut rig.f.h.state, &mut conversation, &rig.f.tabs, &bus);
        assert!(bridge::enter_reorder(&mut ctx, &announce));
    }
    let origin = rig
        .f
        .h
        .state
        .task_panel_state
        .reorder
        .clone()
        .expect("armed");
    assert_eq!(
        (origin.conversation_id, origin.plan_id, origin.number),
        (conv_id(&rig), "p1".to_owned(), 4),
        "the reorder is bound to its origin"
    );
    // Without the hub on top the palette opens the tab and says how to choose.
    let mut other = tasks_rig(100, 30).await;
    other.f.press(K::Esc);
    other.f.paint();
    assert!(!rustain::adapters::tui::mission_control::is_top(
        &other.f.h.state
    ));
    other.f.press(K::Ctrl('p'));
    other
        .f
        .h
        .state
        .command_palette
        .filtered_entries
        .push(entry());
    assert_eq!(
        other.f.press(K::Enter),
        InputAction::OpenMissionControl(McTab::Tasks)
    );
    assert!(warned(&other, "Select a pending task"));
}
