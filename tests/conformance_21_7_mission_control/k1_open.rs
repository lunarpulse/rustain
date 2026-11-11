//! K1 / AC1 / AC4: the hub opens on the Room tab at every width through the chord and through
//! bare `/room`; the frame has a title, a tab row, at least one body row and a rail; the status
//! row and the composer bytes are untouched; a second open retargets the one frame; the frame
//! dims the base, the composer and nothing else; bare `/room` writes no class-V row and the
//! `/room role …` result still leaves at the next composer submission (21-17).
use super::support::*;
use ratatui::style::Modifier;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::presentation::{self, Surface};

const SIZES: [(u16, u16, bool); 4] = [
    (60, 16, false),
    (80, 24, false),
    (130, 30, false),
    (60, 16, true),
];

fn full_composer(rig: &mut Rig) {
    rig.f.h.state.input_buffer = ["draft"; 8].join("\n");
    rig.f.h.state.cursor_position = rig.f.h.state.input_buffer.len();
    rig.f.paint();
}

#[tokio::test]
async fn the_chord_opens_the_room_tab_at_every_size_and_leaves_status_and_composer_alone() {
    for (width, height, full) in SIZES {
        let label = format!("{width}x{height} full={full}");
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        if full {
            full_composer(&mut rig);
        }
        let composer_before = composer_rows(&rig);
        let status_before = status_row(&rig);
        let stage_cache = rig.f.cache();
        let draft = rig.f.h.state.input_buffer.clone();
        // The control is the frame's own title (`Mission Control · <Tab>`); the idle mission strip
        // says `^X s Mission Control` on its row and is not a frame.
        assert!(
            !lines(&rig)
                .iter()
                .any(|row| row.contains("Mission Control ·")),
            "{label}: control"
        );

        open_room(&mut rig).await;

        assert_eq!(top_surface(&rig), Some(Surface::MissionControl), "{label}");
        assert_eq!(
            top_title(&rig).as_deref(),
            Some("Mission Control · Room"),
            "{label}"
        );
        let rect = frame_rect(&rig);
        let all = lines(&rig);
        assert!(
            all[usize::from(rect.y)].contains("Mission Control · Room"),
            "{label}: the title is painted on the frame's top border: {:?}",
            all[usize::from(rect.y)]
        );
        assert!(
            all[usize::from(rect.y) + 1].contains("Room"),
            "{label}: the tab row follows the title: {:?}",
            all[usize::from(rect.y) + 1]
        );
        let body = body_lines(&rig);
        assert!(!body.is_empty(), "{label}: at least one body row");
        assert!(
            body[0].contains("Durable Room"),
            "{label}: the body is the room replay: {:?}",
            body[0]
        );
        assert!(
            rail_line(&rig).contains("[Esc] Back"),
            "{label}: the generated rail names the way out: {:?}",
            rail_line(&rig)
        );
        let layout = rig.f.layout();
        assert!(
            rect.bottom() <= layout.status_bar.y,
            "{label}: the frame ends above the status row"
        );
        assert_eq!(
            composer_rows(&rig),
            composer_before,
            "{label}: composer bytes"
        );
        assert_eq!(
            rig.f.h.state.input_buffer, draft,
            "{label}: the draft is untouched"
        );
        assert_eq!(rig.f.cache(), stage_cache, "{label}: Stage caches");
        assert!(
            !status_row(&rig).contains("Mission Control") && !status_before.is_empty(),
            "{label}: the status row is uncovered"
        );
        assert!(presentation::consistent(&rig.f.h.state), "{label}");
    }
}

#[tokio::test]
async fn bare_slash_room_opens_the_same_frame_and_writes_no_class_v_row() {
    for (width, height, full) in SIZES {
        let label = format!("{width}x{height} full={full}");
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        if full {
            full_composer(&mut rig);
        }
        let views = rig.f.h.state.feedback_blocks.clone();
        slash_room(&mut rig, "").await;
        assert_eq!(
            top_title(&rig).as_deref(),
            Some("Mission Control · Room"),
            "{label}"
        );
        assert_eq!(rig.f.h.state.modal_stack.len(), 1, "{label}");
        assert_eq!(
            rig.f.h.state.feedback_blocks, views,
            "{label}: the view is a frame, not a transcript row"
        );
        assert!(
            body_lines(&rig).len() < 5
                || body_lines(&rig)
                    .iter()
                    .any(|row| row.contains("spoke-alpha")),
            "{label}: the seeded node is a painted row: {:?}",
            body_lines(&rig)
        );
    }
}

#[tokio::test]
async fn a_second_open_retargets_the_one_frame() {
    let mut rig = Rig::plain(room_workspace(&NODES).await, 80, 24).await;
    open_room(&mut rig).await;
    let id = rig.f.h.state.modal_stack.top().unwrap().id;
    // The same open again: still one frame, the same identity.
    open_room(&mut rig).await;
    assert_eq!(rig.f.h.state.modal_stack.len(), 1);
    assert_eq!(rig.f.h.state.modal_stack.top().unwrap().id, id);
    // Retarget from another tab: the chord over the hub brings the Room back in place.
    rig.f
        .h
        .state
        .mission_control
        .register_probe(probe_rows(&["a", "b"]));
    for tab in [
        rustain::adapters::tui::mission_control::McTab::Artifacts,
        rustain::adapters::tui::mission_control::McTab::Probe,
    ] {
        assert_eq!(
            rig.f.press(Key::Right),
            InputAction::OpenMissionControl(tab)
        );
    }
    assert_eq!(top_title(&rig).as_deref(), Some("Mission Control · Probe"));
    open_room(&mut rig).await;
    assert_eq!(top_title(&rig).as_deref(), Some("Mission Control · Room"));
    assert_eq!(
        rig.f.h.state.modal_stack.len(),
        1,
        "the single frame was retargeted"
    );
    assert_eq!(rig.f.h.state.modal_stack.top().unwrap().id, id);
    // Over another frame the hub is pushed above it, and a second open truncates back to it.
    rig.f.press(Key::Ctrl('x'));
    rig.f.press(Key::Char('?'));
    assert!(is_open(&rig, Surface::Help));
    open_room(&mut rig).await;
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert!(
        !is_open(&rig, Surface::Help),
        "reopening truncates to the hub"
    );
    assert_eq!(rig.f.h.state.modal_stack.len(), 1);
    assert!(presentation::consistent(&rig.f.h.state));
}

#[tokio::test]
async fn the_frame_dims_the_base_and_the_composer_and_nothing_below_the_stage() {
    let mut rig = Rig::plain(room_workspace(&NODES).await, 130, 30).await;
    let layout = rig.f.layout();
    let composer = (layout.input_area.x + 1, layout.input_area.y + 1);
    let status = (layout.status_bar.x + 1, layout.status_bar.y);
    let stage = (layout.chat_pane.x, layout.chat_pane.y);
    let dim = |rig: &Rig, at: (u16, u16)| {
        rig.f.h.terminal.backend().buffer()[at]
            .modifier
            .contains(Modifier::DIM)
    };
    assert!(
        !dim(&rig, composer),
        "control: the composer is not dimmed before the open"
    );
    open_room(&mut rig).await;
    assert!(
        dim(&rig, composer),
        "the composer is dimmed under an L2 frame"
    );
    assert!(
        dim(&rig, stage),
        "the Stage region is dimmed outside the frame"
    );
    assert!(!dim(&rig, status), "the status row is never dimmed");
}

#[tokio::test]
async fn role_results_still_leave_at_the_next_submission() {
    let mut rig = Rig::plain(room_workspace(&NODES).await, 80, 24).await;
    slash_room(&mut rig, " role list").await;
    assert!(
        rig.f.h.state.feedback_blocks.contains_key("room-role"),
        "{:?}",
        rig.f.h.state.feedback_blocks
    );
    assert!(
        !presentation::is_open(&rig.f.h.state, Surface::MissionControl),
        "a role listing is a transcript row, not the hub"
    );
    rig.f.h.state.input_buffer = "hello".to_owned();
    let action = submit_message_for_test(&mut rig.f.h.state);
    assert!(
        matches!(action, InputAction::SubmitMessage(_)),
        "{action:?}"
    );
    assert!(
        !rig.f.h.state.feedback_blocks.contains_key("room-role"),
        "21-17: the result leaves with the next composer submission"
    );
    // The hub's own opening never touches that lifetime: a view shown, the hub opened, the
    // submission still retires it.
    slash_room(&mut rig, " role list").await;
    slash_room(&mut rig, "").await;
    assert!(rig.f.h.state.feedback_blocks.contains_key("room-role"));
    assert_eq!(top_surface(&rig), Some(Surface::MissionControl));
    assert_eq!(McTab::Room, rig.f.h.state.mission_control.tab);
}
