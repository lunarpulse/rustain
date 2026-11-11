//! K11 (body half 7-E, toggle half 7-F) / AC21 / AC23 / A16: the read-only monitor column
//! (`Ctrl+X w`, default off, at least 120 columns) paints the current Mission Control tab body
//! under a muted `Monitor · <Tab>` title: no selection, no rail, no tab row, never a focus or a
//! key target.
//! A Room body painted there counts as presented (the head poll runs while it is on screen);
//! a Messages body painted there never commits a log visit. Every fixture goes through a
//! production door: the draw is the production `event_loop::render`, keys travel as crossterm
//! events through the local host's decoder. No sleeps.
use super::support::*;
use crate::conformance_21_8_thread_inspector::support::Item;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::keymap::table::{MONITOR_NARROW, attached_mission_remedy};
use rustain::adapters::tui::keymap::{AvailCtx, Host, LeaderStep, leader_step};
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::presentation::Surface;
use rustain::domain::models::SemanticMessageType;
use rustain::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};
use rustain::infrastructure::runtime::panel_bridge::room_head_poll_due;
use rustain::infrastructure::runtime::transparency_bridge;

/// The production tick the event loop hands the poll gate (250 ms; the interval is 1,000 ms).
const TICK: u64 = 250;

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

fn plan() -> Plan {
    Plan {
        id: "p1".to_owned(),
        title: "Release".to_owned(),
        tasks: vec![
            task(1, "Compile", PlanTaskStatus::Completed),
            task(2, "Test the build", PlanTaskStatus::Running),
            task(3, "Ship", PlanTaskStatus::Pending),
        ],
        estimated_effort: None,
        status: PlanStatus::Executing,
        created_at: 1,
        resolved_at: None,
        host_message_id: None,
    }
}

/// A 130x30 terminal with a plan in the active conversation, drawn by the production host,
/// the monitor still off.
async fn planned() -> Rig {
    let mut rig = Rig::plain(Workspace::new().await, 130, 30).await;
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .insert("p1".to_owned(), plan());
    rig.f.h.state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
    rig
}

/// [`planned`] with the monitor on, drawn once.
async fn monitored() -> Rig {
    let mut rig = planned().await;
    show_column(&mut rig);
    rig.paint();
    rig
}

fn show_column(rig: &mut Rig) {
    rig.f.h.state.monitor = true;
}

/// The rows painted inside the column rectangle (the layout the production draw used).
fn column(rig: &Rig) -> Vec<String> {
    let rect = rig
        .f
        .layout()
        .sidebar
        .expect("the column is laid out at 130 columns in Monitor density");
    let all = lines(rig);
    (rect.y..rect.bottom())
        .map(|y| {
            all[usize::from(y)]
                .chars()
                .skip(usize::from(rect.x))
                .collect::<String>()
        })
        .collect()
}

#[tokio::test]
async fn the_column_paints_the_tasks_body_by_default_read_only_under_a_muted_title() {
    let rig = monitored().await;
    let rows = column(&rig);
    assert!(rows[0].contains("Monitor · Tasks"), "{rows:?}");
    let all = rows.join("\n");
    // The body is the Tasks tab's own rows (the hub never opened: Tasks is the default).
    let list = rig
        .f
        .h
        .state
        .mission_control
        .list_for(McTab::Tasks)
        .expect("the host built the Tasks rows for the column");
    assert_eq!(list.all().len(), 3);
    for row in list.all() {
        let text = row.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let painted: String = all.split_whitespace().collect::<Vec<_>>().join(" ");
        let head: String = text.chars().take(14).collect();
        assert!(painted.contains(&head), "`{head}` of `{text}` in {all}");
    }
    // Read-only: no selection marker, no rail, no tab row, no frame title of the hub.
    assert!(!all.contains('▸'), "no selection: {all}");
    assert!(
        !all.contains("[Esc]") && !all.contains("Enter"),
        "no rail: {all}"
    );
    assert!(
        !all.contains("Messages") && !all.contains("Artifacts"),
        "no tab row: {all}"
    );
    assert_eq!(top_surface(&rig), None, "the column is not a frame");
    assert_eq!(rig.f.h.state.mission_control.selected_id(), None);
}

#[tokio::test]
async fn the_column_follows_the_last_tab_the_hub_was_on() {
    let mut rig = monitored().await;
    assert_eq!(rig.f.press(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(
        rig.f.press(Key::Char('r')),
        InputAction::OpenMissionControl(McTab::Room)
    );
    rig.paint();
    assert!(
        column(&rig)[0].contains("Monitor · Room"),
        "{:?}",
        column(&rig)
    );
    // Closing the hub leaves the tab where it was: the column keeps following it.
    assert_eq!(rig.key(Key::Esc), InputAction::Consumed);
    rig.paint();
    assert_eq!(top_surface(&rig), None);
    assert!(
        column(&rig)[0].contains("Monitor · Room"),
        "{:?}",
        column(&rig)
    );
}

#[tokio::test]
async fn monitor_is_never_a_focus_or_key_target() {
    use rustain::domain::models::FocusState;
    for focus in [FocusState::Input, FocusState::Chat] {
        let mut rig = monitored().await;
        rig.f.h.state.focus = focus.clone();
        let before = column(&rig);
        for key in [
            Key::Char('j'),
            Key::Char('k'),
            Key::Down,
            Key::Up,
            Key::Char('d'),
        ] {
            rig.key(key);
            rig.paint();
            assert_eq!(
                top_surface(&rig),
                None,
                "{focus:?} {key:?}: no frame opened"
            );
            assert_eq!(
                rig.f.h.state.mission_control.selected_id(),
                None,
                "{focus:?} {key:?}: the column selects nothing"
            );
        }
        assert_eq!(column(&rig), before, "{focus:?}: the column did not move");
    }
    // Positive control: with the column shown the same printable keys still reach their normal
    // owner (the composer in Input focus): the column swallows nothing.
    let mut rig = monitored().await;
    rig.f.h.state.focus = rustain::domain::models::FocusState::Input;
    rig.key(Key::Char('j'));
    rig.key(Key::Char('k'));
    assert_eq!(
        rig.f.h.state.input_buffer, "jk",
        "the composer owns printable keys"
    );
    // Positive control: with the hub on Tasks the same `j` moves its selection.
    let mut rig = monitored().await;
    assert_eq!(rig.f.press(Key::Ctrl('x')), InputAction::Consumed);
    rig.f.press(Key::Char('t'));
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    let first = rig
        .f
        .h
        .state
        .mission_control
        .selected_id()
        .map(str::to_owned);
    rig.key(Key::Char('j'));
    rig.paint();
    assert_ne!(
        rig.f
            .h
            .state
            .mission_control
            .selected_id()
            .map(str::to_owned),
        first
    );
}

#[tokio::test]
async fn a_room_painted_in_the_column_is_presented_and_polled_even_under_a_frame() {
    let mut rig = Rig::plain(room_workspace(&NODES).await, 130, 30).await;
    open_room(&mut rig).await;
    // Hub on top, no column: presented by the hub (the 7-A rule); one refresh per second.
    assert_eq!(
        (0..4)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        1
    );
    // The hub closes and the column (on the Room tab) takes over: it is on screen, so it counts.
    rig.key(Key::Esc);
    show_column(&mut rig);
    rig.paint();
    assert_eq!(top_surface(&rig), None);
    assert!(
        column(&rig)[0].contains("Monitor · Room"),
        "{:?}",
        column(&rig)
    );
    assert!(rustain::adapters::tui::mission_control::room_projection_presented(&rig.f.h.state));
    assert_eq!(
        (0..4)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        1
    );
    // 7E-01: a frame paints over the column (frames span the full terminal width), so the
    // painted-before-the-overlay column is not a presentation: nothing is polled.
    rig.f.press(Key::Ctrl('x'));
    rig.f.press(Key::Char('?'));
    assert_eq!(top_surface(&rig), Some(Surface::Help));
    // Before the covering frame's draw the earlier paint is already stale for the gate.
    assert_eq!(
        (0..4)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        0,
        "covered, before its draw"
    );
    rig.paint();
    assert_eq!(
        (0..8)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        0,
        "covered, after its draw"
    );
    // A pending leader menu covers the column's rows too.
    rig.f.press(Key::Esc);
    rig.paint();
    assert_eq!(top_surface(&rig), None);
    rig.f.press(Key::Ctrl('x'));
    assert!(rig.f.h.state.pending_leader.is_some());
    rig.paint();
    assert_eq!(
        (0..8)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        0,
        "leader menu"
    );
    // Positive control: uncovered again, the column is presented and polled.
    rig.f.press(Key::Esc);
    rig.paint();
    assert!(rig.f.h.state.pending_leader.is_none());
    assert!(rustain::adapters::tui::mission_control::room_projection_presented(&rig.f.h.state));
    assert_eq!(
        (0..4)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        1
    );
    // Negative control: the column hidden (and no hub): nothing is presented, the counter
    // starts over.
    rig.f.press(Key::Esc);
    rig.f.h.state.monitor = false;
    rig.paint();
    assert!(!rustain::adapters::tui::mission_control::room_projection_presented(&rig.f.h.state));
    assert_eq!(
        (0..8)
            .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
            .count(),
        0
    );
    // And a column on another tab does not present the Room either.
    show_column(&mut rig);
    rig.f.h.state.mission_control.tab = McTab::Tasks;
    rig.paint();
    assert!(!rustain::adapters::tui::mission_control::room_projection_presented(&rig.f.h.state));
}

#[tokio::test]
async fn messages_painted_in_the_column_never_commit_a_log_visit() {
    let items: Vec<Item> = (0..5)
        .map(|n| {
            Item::new(
                &format!("ri_{n:03}"),
                SemanticMessageType::Consultation,
                &format!("log marker {n:03}"),
            )
        })
        .collect();
    let mut rig = Rig::plain(Workspace::with(&items).await, 130, 40).await;
    attach_journal(&mut rig);
    show_column(&mut rig);
    transparency_bridge::team_command(&mut rig.f.h.state, "origin", Some("log"), &rig.app).await;
    let visit = rig
        .f
        .h
        .state
        .pending_log_visit
        .expect("an unfiltered visit");
    // The palette covers the hub; the column (on Messages) is painted by the completed draw.
    rig.f.press(Key::Ctrl('p'));
    assert_ne!(top_surface(&rig), Some(Surface::MissionControl));
    rig.paint();
    let rows = column(&rig);
    assert!(rows[0].contains("Monitor · Messages"), "{rows:?}");
    assert!(
        rows.join("\n").contains("item-received"),
        "the column paints the held page: {rows:?}"
    );
    assert_eq!(
        rig.f.h.state.pending_log_visit,
        Some(visit),
        "a column paint never commits"
    );
    assert!(rig.f.h.state.log_awareness.presented.is_empty());
    // Positive control: uncovered, the hub on top paints the body and the visit commits once.
    rig.f.press(Key::Esc);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(rig.f.h.state.pending_log_visit, None);
    assert_eq!(rig.f.h.state.log_awareness.presented.as_slice(), &[visit]);
}

#[tokio::test]
async fn untrusted_row_text_never_reaches_the_column_as_controls() {
    let mut rig = monitored().await;
    let mut hostile = plan();
    hostile.tasks[0].title = "evil\u{1b}]0;x\u{7}\u{202e}row\nforged".to_owned();
    rig.f
        .tabs
        .active_tab_mut()
        .conversation
        .plans
        .insert("p1".to_owned(), hostile);
    rig.paint();
    let all = column(&rig).join("\n");
    assert!(
        !all.chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
        "{all:?}"
    );
    assert!(all.contains("evil"), "{all}");
}

/// A16 / AC23: `Ctrl+X w` toggles the column at 130 columns: default off, then the body under
/// `Monitor · Tasks` (the default tab), then gone again. No toast: the toggle is silent.
#[tokio::test]
async fn ctrl_x_w_toggles_the_column_on_and_off_at_130_columns() {
    let mut rig = planned().await;
    rig.paint();
    assert!(!rig.f.h.state.monitor, "the monitor defaults to off");
    assert!(rig.f.layout().sidebar.is_none());
    assert!(!lines(&rig).iter().any(|row| row.contains("Monitor ·")));
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('w')), InputAction::Consumed);
    assert!(rig.f.h.state.monitor);
    assert_eq!(rig.f.h.state.notifications.records().count(), 0);
    rig.paint();
    let rows = column(&rig);
    assert!(rows[0].contains("Monitor · Tasks"), "{rows:?}");
    assert!(rows.join("\n").contains("Compile"), "{rows:?}");
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('w')), InputAction::Consumed);
    assert!(!rig.f.h.state.monitor);
    rig.paint();
    assert!(rig.f.layout().sidebar.is_none());
    assert!(!lines(&rig).iter().any(|row| row.contains("Monitor ·")));
}

fn remedies(rig: &Rig) -> Vec<String> {
    rig.f
        .h
        .state
        .notifications
        .records()
        .map(|record| record.message.clone())
        .collect()
}

/// A16 / AC23: below 120 columns the chord answers the width remedy and paints nothing; the
/// preference survives a narrowing resize (the old P11 reset is gone), is not painted while
/// narrow, is not turned off by the refused chord, and paints again when the terminal widens.
#[tokio::test]
async fn below_120_columns_the_chord_is_refused_and_the_preference_survives_a_resize() {
    let mut rig = Rig::plain(Workspace::new().await, 80, 24).await;
    rig.paint();
    rig.key(Key::Ctrl('x'));
    assert_eq!(rig.key(Key::Char('w')), InputAction::Consumed);
    assert!(!rig.f.h.state.monitor, "refused: nothing toggled");
    assert!(remedies(&rig).iter().any(|m| m == MONITOR_NARROW));
    rig.paint();
    assert!(!lines(&rig).iter().any(|row| row.contains("Monitor ·")));

    let mut rig = monitored().await;
    for narrow in [119u16, 100, 80] {
        rig.resize(narrow, 30);
        rig.paint();
        assert!(rig.f.h.state.monitor, "{narrow}: the preference is kept");
        assert!(rig.f.layout().sidebar.is_none(), "{narrow}");
        assert!(
            !lines(&rig).iter().any(|row| row.contains("Monitor ·")),
            "{narrow}: not painted"
        );
        rig.key(Key::Ctrl('x'));
        assert_eq!(rig.key(Key::Char('w')), InputAction::Consumed);
        assert!(
            rig.f.h.state.monitor,
            "{narrow}: the refused chord leaves it on"
        );
    }
    assert!(remedies(&rig).iter().any(|m| m == MONITOR_NARROW));
    // Widening to exactly the floor paints the column again, still on Tasks.
    rig.resize(120, 30);
    rig.paint();
    let rows = column(&rig);
    assert!(rows[0].contains("Monitor · Tasks"), "{rows:?}");
    // And the chord acts again at the floor.
    rig.key(Key::Ctrl('x'));
    rig.key(Key::Char('w'));
    assert!(!rig.f.h.state.monitor);
}

/// A16 / A20 / AC23: the attached host refuses the chord at every width with the one A20 remedy
/// (the local host runs it from 120 columns: the controls).
#[test]
fn the_attached_host_refuses_the_monitor_chord_with_the_one_a20_remedy() {
    for width in [60u16, 80, 119, 120, 130] {
        let attached = AvailCtx::attached(width, false);
        match leader_step(Host::Attached, Key::Char('w'), &attached, "Input") {
            LeaderStep::Unavailable(_, remedy) => {
                assert_eq!(remedy, attached_mission_remedy(), "{width}");
            }
            other => panic!("attached `w` must be refused: {other:?}"),
        }
        let local = AvailCtx::local(width);
        match leader_step(Host::Local, Key::Char('w'), &local, "Input") {
            LeaderStep::Run(_) => assert!(width >= 120, "{width}: runs only from 120"),
            LeaderStep::Unavailable(_, remedy) => {
                assert!(width < 120, "{width}: refused only below 120");
                assert_eq!(remedy, MONITOR_NARROW, "{width}");
            }
            other => panic!("local `w` must run or be refused: {other:?}"),
        }
    }
}
