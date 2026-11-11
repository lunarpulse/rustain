//! K4 / AC3 / A14 / RU-28: the Room tab is a read-only replay of the production fold, and its
//! head poll runs only while the Room is *presented* — the last completed draw painted the
//! body on a hub that is still the top frame. The counting seam is the production gate
//! `panel_bridge::room_head_poll_due`: every `true` it returns is one `refresh_head` the event
//! loop runs, at one second of ticks, with no timer or thread of its own.
use super::support::*;
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::Surface;
use rustain::adapters::tui::widgets::room_panel::node_row;
use rustain::domain::models::OrchestrationRoomId;
use rustain::infrastructure::runtime::panel_bridge::room_head_poll_due;
use rustain::infrastructure::runtime::room_bridge::fold_room_read;
use rustain::infrastructure::subagent::node_journal::WorkspaceJournalReader;

/// The production tick the event loop hands the gate (250 ms; the interval is 1,000 ms).
const TICK: u64 = 250;

/// Run `ticks` event-loop ticks and count the head reads they would trigger.
fn refreshes(rig: &mut Rig, ticks: usize) -> usize {
    (0..ticks)
        .filter(|_| room_head_poll_due(&mut rig.f.h.state, TICK))
        .count()
}

async fn opened() -> Rig {
    let mut rig = Rig::plain(room_workspace(&NODES).await, 80, 24).await;
    open_room(&mut rig).await;
    rig
}

#[tokio::test]
async fn the_room_body_equals_the_production_fold_of_the_journal() {
    let rig = opened().await;
    let entries = rustain::domain::ports::RoomJournalReader::load_entries(
        &WorkspaceJournalReader::open_workspace(rig.ws.path()),
    )
    .await
    .expect("read the journal");
    let (room, max_seq, unknown) = fold_room_read(
        entries,
        OrchestrationRoomId::parse("room-k4").expect("room id"),
        &rig.f.h.state.room_panel.host_id,
    );
    assert_eq!(max_seq, 6);
    assert_eq!(unknown, 0);
    let painted = body_lines(&rig).join("\n");
    assert_eq!(room.nodes().len(), 3);
    for view in room.nodes().values() {
        let row = node_row(view);
        assert!(
            painted.contains(&row),
            "the production row `{row}` is painted: {painted}"
        );
    }
    assert!(painted.contains("wave ✓0 ✗0 ▶3"), "{painted}");
    assert!(painted.contains("as of"), "never `live`: {painted}");
    assert!(
        !painted
            .to_ascii_lowercase()
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == "live"),
        "{painted}"
    );
}

#[tokio::test]
async fn room_head_poll_runs_once_a_second_while_the_room_is_presented() {
    let mut rig = opened().await;
    assert_eq!(refreshes(&mut rig, 3), 0, "750 ms is not yet a second");
    assert_eq!(
        refreshes(&mut rig, 1),
        1,
        "the fourth tick completes the second"
    );
    assert_eq!(
        refreshes(&mut rig, 8),
        2,
        "then one per second, no new timer"
    );
}

#[tokio::test]
async fn room_head_poll_stops_while_room_tab_is_covered() {
    let mut rig = opened().await;
    assert_eq!(refreshes(&mut rig, 3), 0);
    // Covered by Help: the hub is still *held*, but it is not presented.
    rig.f.press(Key::Ctrl('x'));
    rig.f.press(Key::Char('?'));
    assert_eq!(top_surface(&rig), Some(Surface::Help));
    // Before the next draw the held hub's last paint is stale: the gate already refuses.
    assert_eq!(
        refreshes(&mut rig, 4),
        0,
        "covered, before the covering frame painted"
    );
    rig.f.paint();
    assert_eq!(refreshes(&mut rig, 12), 0, "covered, after it painted");
    // Uncovered: the counter started over while covered (three ticks before + three after
    // would have been a second had it survived).
    rig.f.press(Key::Esc);
    rig.f.paint();
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(refreshes(&mut rig, 3), 0, "the counter reset while covered");
    assert_eq!(refreshes(&mut rig, 1), 1);
}

#[tokio::test]
async fn room_head_poll_stops_under_a_decision_a_palette_and_a_notification_log() {
    for open in [
        &[Key::Ctrl('p')][..],
        &[Key::Ctrl('x'), Key::Char('n')][..],
        &[Key::Ctrl('x'), Key::Char('?')][..],
    ] {
        let mut rig = opened().await;
        for key in open {
            rig.f.press(*key);
        }
        rig.f.paint();
        assert_eq!(refreshes(&mut rig, 12), 0, "{open:?}");
    }
    // A queued decision covers the hub the same way.
    let mut rig = opened().await;
    rustain::adapters::tui::decision_queue::raise(
        &mut rig.f.h.state,
        rustain::adapters::tui::decision_queue::DecisionDraft::new(
            rustain::adapters::tui::decision_queue::DecisionKind::Budget,
            "a decision above the hub",
        ),
        rustain::adapters::tui::decision_queue::Arrival::Automatic,
    );
    rig.f.press(Key::Ctrl('x'));
    rig.f.press(Key::Char('q'));
    rustain::adapters::tui::decision_queue::open_waiting(
        &mut rig.f.h.state,
        &rustain::adapters::tui::notification_log::LogFacts::default(),
    );
    rig.f.paint();
    assert_eq!(top_surface(&rig), Some(Surface::DecisionQueue));
    assert_eq!(refreshes(&mut rig, 12), 0, "decision above the Room");
}

#[tokio::test]
async fn room_head_poll_does_not_run_on_another_tab_closed_or_after_an_interrupted_draw() {
    // Another tab.
    let mut rig = opened().await;
    rig.f
        .h
        .state
        .mission_control
        .register_probe(probe_rows(&["a"]));
    rig.f.press(Key::Right);
    rig.f.paint();
    assert_eq!(refreshes(&mut rig, 12), 0, "the hub is on another tab");
    rig.f.press(Key::Left);
    rig.f.paint();
    assert_eq!(
        refreshes(&mut rig, 4),
        1,
        "positive control: back on the Room"
    );
    // Closed.
    rig.f.press(Key::Esc);
    rig.f.paint();
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert_eq!(refreshes(&mut rig, 12), 0, "closed");
    // An interrupted draw: the pre-draw hook ran and the draw never completed.
    let mut rig = opened().await;
    assert_eq!(refreshes(&mut rig, 4), 1, "control: presented");
    rig.f.h.state.restore_unflushed_log_visits();
    assert_eq!(
        refreshes(&mut rig, 12),
        0,
        "nothing is presented until a draw completes"
    );
    rig.f.paint();
    assert_eq!(
        refreshes(&mut rig, 4),
        1,
        "the next completed draw presents it again"
    );
}

#[tokio::test]
async fn room_head_poll_does_not_run_when_the_terminal_is_below_the_floor() {
    let mut rig = opened().await;
    assert_eq!(refreshes(&mut rig, 4), 1, "control");
    rig.f.h.terminal.backend_mut().resize(50, 14);
    rig.f.h.state.terminal_width = 50;
    rig.f.h.state.terminal_height = 14;
    rig.f.paint();
    assert_eq!(
        refreshes(&mut rig, 12),
        0,
        "too small to paint: not presented"
    );
}

#[tokio::test]
async fn the_hub_polls_the_head_and_marks_a_newer_entry_without_moving_the_viewport() {
    use rustain::infrastructure::runtime::room_bridge;
    let mut rig = opened().await;
    register(&rig.ws, "spoke-delta").await;
    room_bridge::refresh_head(&rig.app, &mut rig.f.h.state).await;
    rig.f.paint();
    let painted = body_lines(&rig).join("\n");
    assert!(
        painted.contains("newer"),
        "the arrival is counted, not folded: {painted}"
    );
    assert!(
        !painted.contains("spoke-delta"),
        "the anchored replay does not move: {painted}"
    );
}

// 7A-01 (patch round 1): the poll is eligible only while the LAST COMPLETED DRAW painted the
// Room as the top visible frame.
#[tokio::test]
async fn room_head_poll_is_not_eligible_while_a_pending_leader_menu_hides_the_room() {
    let mut rig = opened().await;
    assert_eq!(refreshes(&mut rig, 3), 0);
    rig.f.press(Key::Ctrl('x'));
    assert!(rig.f.h.state.pending_leader.is_some());
    assert_eq!(
        refreshes(&mut rig, 12),
        0,
        "the menu is not a frame, but it covers the Room: not presented, even before it paints"
    );
    rig.f.paint();
    assert_eq!(
        refreshes(&mut rig, 12),
        0,
        "after the menu painted over the Room"
    );
    // Dismissed: nothing is presented until a completed draw paints the Room again, and the
    // counter started over while hidden.
    rig.f.press(Key::Esc);
    assert!(rig.f.h.state.pending_leader.is_none());
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(
        refreshes(&mut rig, 12),
        0,
        "no draw has presented it since the menu"
    );
    rig.f.paint();
    assert_eq!(refreshes(&mut rig, 3), 0, "the counter reset while hidden");
    assert_eq!(refreshes(&mut rig, 1), 1);
}

#[tokio::test]
async fn room_head_poll_is_not_eligible_after_a_drawn_cover_closes_until_a_fresh_draw() {
    for open in [
        &[Key::Ctrl('x'), Key::Char('?')][..],
        &[Key::Ctrl('p')][..],
        &[Key::Ctrl('x'), Key::Char('n')][..],
    ] {
        let mut rig = opened().await;
        for key in open {
            rig.f.press(*key);
        }
        rig.f.paint(); // a completed draw with the cover on top
        rig.f.press(Key::Esc); // the cover closes; the loop ticks before the next draw
        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{open:?}");
        assert_eq!(
            refreshes(&mut rig, 12),
            0,
            "{open:?}: the latest completed draw did not present the Room"
        );
        rig.f.paint();
        assert_eq!(
            refreshes(&mut rig, 4),
            1,
            "{open:?}: eligible again after a fresh draw"
        );
    }
}
