//! K3 — each step completes only by its real action (AC2, AC3, D5, D6, D7).
use super::support::*;
use crate::conformance_21_13_block_viewer::support as block_support;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::decision_queue::{
    self, Arrival, DecisionDraft, DecisionKind, INERT_AFTER_PAINT, Raised, TYPING_GRACE,
};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::presentation::Surface;
use rustain::adapters::tui::tour::{Phase, TourStep};
use rustain::adapters::tui::{adapters_window, history_window};
use rustain::domain::models::{InvocationStatus, WriteDiffState};
use serde_json::json;

const STEPS: [TourStep; 5] = [
    TourStep::Send,
    TourStep::Inspect,
    TourStep::Decide,
    TourStep::Hub,
    TourStep::Help,
];

/// Start the tour with `/tour`, skip to step `number` and arm it with `Enter`.
fn armed_at(f: &mut Fixture, number: usize) {
    slash_tour(f);
    for _ in 1..number {
        press(f, Key::Char('n'));
    }
    press(f, Key::Enter);
    assert_eq!(step(f), Some((STEPS[number - 1], Phase::Armed)));
    assert!(f.h.state.modal_stack.is_empty());
}

fn tool_fixture() -> (Fixture, String) {
    Fixture::single(
        80,
        24,
        "Bash",
        json!({"command": "echo hi"}),
        InvocationStatus::Success,
        Some(("OUT", WriteDiffState::NotAWrite)),
    )
}

/// A decision arrives after the typing grace of the key that armed the step, is painted, and
/// is past its 500 ms inert window, so the next key can decide it.
fn raise_budget(f: &mut Fixture) {
    f.clock.advance(TYPING_GRACE);
    assert_eq!(
        decision_queue::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Automatic,
        ),
        Raised::Focused
    );
    f.paint();
    f.clock.advance(INERT_AFTER_PAINT);
}

/// The armed step did not move after `what`.
fn still(f: &Fixture, number: usize, what: &str) {
    assert_eq!(step(f), Some((STEPS[number - 1], Phase::Armed)), "{what}");
}

#[test]
fn step_one_completes_only_by_a_plain_text_submission() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 1);
    // Out of order: Help does not complete a send step.
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    still(&f, 1, "help");
    press(&mut f, Key::Esc);
    // A slash command and an `@Agents/` line are not messages.
    f.h.state.input_buffer = "/ml".into();
    assert!(matches!(
        submit_message_for_test(&mut f.h.state),
        InputAction::ExecuteCommand { .. }
    ));
    still(&f, 1, "slash command");
    f.h.state.input_buffer = "@Agents/default".into();
    assert!(matches!(
        submit_message_for_test(&mut f.h.state),
        InputAction::ClearActiveAgent { .. }
    ));
    still(&f, 1, "@Agents/");
    // A message the read-only tab would refuse does not count.
    f.h.state.input_buffer = "hello".into();
    f.h.state.local_read_only = true;
    assert!(matches!(
        submit_message_for_test(&mut f.h.state),
        InputAction::SubmitMessage(_)
    ));
    still(&f, 1, "read-only tab");
    f.h.state.local_read_only = false;
    // Positive control: the real keys send a plain message.
    f.h.state.focus = rustain::domain::models::FocusState::Input;
    type_text(&mut f, "hello");
    assert_eq!(
        press(&mut f, Key::Enter),
        InputAction::SubmitMessage("hello".into())
    );
    assert_eq!(step(&f), Some((TourStep::Inspect, Phase::CardDue)));
    f.clock.advance(TYPING_GRACE);
    tick(&mut f);
    assert_eq!(top_title(&f).as_deref(), Some("Tour · 2 of 5"));
}

#[test]
fn step_two_completes_only_when_a_tool_call_is_inspected() {
    let (mut f, id) = tool_fixture();
    armed_at(&mut f, 2);
    // Out of order and wrong surfaces: Help, the palette.
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    still(&f, 2, "help");
    press(&mut f, Key::Esc);
    press(&mut f, Key::Ctrl('p'));
    still(&f, 2, "palette");
    press(&mut f, Key::Esc);
    // Positive control: Enter on the focused tool line opens S-TOOL.
    f.focus_call(&id);
    f.open();
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::ToolInspector(_)
    ));
    assert_eq!(step(&f), Some((TourStep::Decide, Phase::CardDue)));
}

#[test]
fn the_block_viewer_does_not_complete_the_inspect_step() {
    let (turn, ids) = block_support::mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    armed_at(&mut f, 2);
    block_support::open(&mut f, &ids[0]);
    assert!(matches!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::BlockViewer(_)
    ));
    still(&f, 2, "S-BLOCK");
    press(&mut f, Key::Esc);
    block_support::open(&mut f, &ids[1]);
    assert_eq!(step(&f), Some((TourStep::Decide, Phase::CardDue)));
}

#[test]
fn deferring_a_decision_does_not_complete_step_three() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 3);
    raise_budget(&mut f);
    press(&mut f, Key::Esc);
    assert!(f.h.state.modal_stack.is_empty(), "the frame popped");
    still(&f, 3, "Esc defer");
    // Positive control: answering through the rail completes it.
    let mut f = plain(80, 24);
    armed_at(&mut f, 3);
    raise_budget(&mut f);
    press(&mut f, Key::Char('y'));
    assert_eq!(step(&f), Some((TourStep::Hub, Phase::CardDue)));
}

#[test]
fn an_answer_before_the_step_is_armed_does_not_count() {
    let mut f = plain(80, 24);
    slash_tour(&mut f);
    press_all(&mut f, &[Key::Char('n'), Key::Char('n')]);
    assert_eq!(step(&f), Some((TourStep::Decide, Phase::Card)));
    raise_budget(&mut f);
    press(&mut f, Key::Char('y'));
    assert_eq!(
        step(&f),
        Some((TourStep::Decide, Phase::Card)),
        "the card phase ignores it"
    );
}

#[test]
fn step_four_completes_by_any_hub_opener_or_the_palette() {
    for (label, keys) in [
        ("Ctrl+X s", vec![Key::Ctrl('x'), Key::Char('s')]),
        ("Ctrl+X t", vec![Key::Ctrl('x'), Key::Char('t')]),
        ("Ctrl+P", vec![Key::Ctrl('p')]),
    ] {
        let mut f = plain(80, 24);
        armed_at(&mut f, 4);
        press_all(&mut f, &keys);
        assert_eq!(step(&f), Some((TourStep::Help, Phase::CardDue)), "{label}");
        // The card waits for the hub to close and for the typing grace.
        tick(&mut f);
        assert_ne!(
            top_title(&f).as_deref(),
            Some("Tour · 5 of 5"),
            "{label}: a frame is open"
        );
        press(&mut f, Key::Esc);
        f.clock.advance(TYPING_GRACE);
        tick(&mut f);
        assert_eq!(top_title(&f).as_deref(), Some("Tour · 5 of 5"), "{label}");
    }
}

#[test]
fn the_other_windows_and_help_do_not_complete_step_four() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 4);
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    still(&f, 4, "help");
    press(&mut f, Key::Esc);
    adapters_window::open(&mut f.h.state, Vec::new());
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::Adapters
    );
    still(&f, 4, "Adapters");
    press(&mut f, Key::Esc);
    history_window::open(&mut f.h.state, &f.sessions, 0);
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::History
    );
    still(&f, 4, "History");
    press(&mut f, Key::Esc);
    // Positive control: the hub completes it.
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('s')]);
    assert_eq!(step(&f), Some((TourStep::Help, Phase::CardDue)));
}

#[test]
fn step_five_completes_by_opening_help_and_finishes_the_tour() {
    let mut f = plain(80, 24);
    let config = Config::new();
    launch_with_offer(&mut f, &config);
    card_opens(&mut f);
    press_all(&mut f, &[Key::Enter]);
    for _ in 1..5 {
        press(&mut f, Key::Char('n'));
    }
    press(&mut f, Key::Enter);
    assert_eq!(step(&f), Some((TourStep::Help, Phase::Armed)));
    press(&mut f, Key::Ctrl('p'));
    still(&f, 5, "palette");
    press(&mut f, Key::Esc);
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    assert!(
        f.h.state.tour.is_none(),
        "completing the last step ends the tour"
    );
    assert_eq!(f.h.state.modal_stack.top().unwrap().surface, Surface::Help);
    assert!(config.done());
    let (severity, message) = newest_notice(&f).unwrap();
    assert_eq!(severity, NoticeSeverity::Success);
    assert!(
        message.starts_with("Tour complete. Help is always one key away: "),
        "{message}"
    );
}

#[test]
fn the_tour_never_creates_a_decision_a_message_or_a_frame_of_its_own() {
    let (mut f, _) = tool_fixture();
    slash_tour(&mut f);
    for _ in 0..4 {
        press(&mut f, Key::Char('n'));
    }
    assert!(f.h.state.decisions.is_empty());
    assert!(f.h.state.input_buffer.is_empty());
    // Pressing an advertised key with nothing to act on behaves as in the base product:
    // the queue is empty and the step stays armed (step 3 here).
    let mut f = plain(80, 24);
    armed_at(&mut f, 3);
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('q')]);
    still(&f, 3, "an empty queue");
}

#[test]
fn due_card_never_opens_over_a_draft_or_recent_typing() {
    let mut f = plain(80, 24);
    launch_with_offer(&mut f, &Config::new());
    // A draft that is old enough to be out of the typing grace still blocks the card.
    type_text(&mut f, "hi");
    f.clock.advance(TYPING_GRACE * 2);
    tick(&mut f);
    assert!(f.h.state.modal_stack.is_empty(), "a draft");
    // Emptied again, then one more printable key and its erasure: typed inside the last second.
    press_all(&mut f, &[Key::Backspace, Key::Backspace]);
    type_text(&mut f, "z");
    press(&mut f, Key::Backspace);
    assert!(f.h.state.input_buffer.is_empty());
    tick(&mut f);
    assert!(f.h.state.modal_stack.is_empty(), "typing within the grace");
    // A pending leader, a reverse search and a frame each hold it back, then release it.
    f.clock.advance(TYPING_GRACE);
    press(&mut f, Key::Ctrl('x'));
    tick(&mut f);
    assert!(f.h.state.modal_stack.is_empty(), "a pending leader");
    press(&mut f, Key::Esc);
    press(&mut f, Key::Ctrl('r'));
    tick(&mut f);
    assert!(f.h.state.modal_stack.is_empty(), "a reverse search");
    press(&mut f, Key::Esc);
    f.clock.advance(TYPING_GRACE);
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    tick(&mut f);
    assert_eq!(f.h.state.modal_stack.top().unwrap().surface, Surface::Help);
    press(&mut f, Key::Esc);
    f.clock.advance(TYPING_GRACE);
    // A streaming turn holds it back; the first quiet tick opens it.
    rustain::adapters::tui::tour::tick(&mut f.h.state, true);
    assert!(f.h.state.modal_stack.is_empty(), "a streaming turn");
    tick(&mut f);
    assert_eq!(top_title(&f).as_deref(), Some("Tour"));
}

#[test]
fn a_waiting_decision_does_not_block_a_card() {
    let mut f = plain(80, 24);
    f.h.state.input_buffer = "draft".into();
    raise_budget_waiting(&mut f);
    f.h.state.input_buffer.clear();
    launch_with_offer(&mut f, &Config::new());
    tick(&mut f);
    assert_eq!(top_title(&f).as_deref(), Some("Tour"));
}

/// A decision that waits (a draft kept it from taking focus).
fn raise_budget_waiting(f: &mut Fixture) {
    assert_eq!(
        decision_queue::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Automatic,
        ),
        Raised::Waiting
    );
}

/// Every armed step ignores each opener that is not its own.
#[test]
fn only_the_armed_steps_action_completes_it() {
    // (opener, keys, the step number it completes)
    let openers: [(&str, Vec<Key>, usize); 3] = [
        ("help", vec![Key::Ctrl('x'), Key::Char('?')], 5),
        ("palette", vec![Key::Ctrl('p')], 4),
        ("hub", vec![Key::Ctrl('x'), Key::Char('s')], 4),
    ];
    for armed in 1..=5 {
        for (label, keys, completes) in &openers {
            if *completes == armed {
                continue;
            }
            let mut f = plain(80, 24);
            armed_at(&mut f, armed);
            press_all(&mut f, keys);
            still(&f, armed, &format!("{label} while step {armed} waits"));
        }
    }
}

/// 14-A-05: an item that went stale under the open frame (its condition cleared) is closed with
/// `Esc`: nothing was answered, so the armed step does not move.
#[test]
fn closing_a_stale_decision_does_not_complete_step_three() {
    let mut f = plain(80, 24);
    armed_at(&mut f, 3);
    raise_budget(&mut f);
    decision_queue::withdraw(&mut f.h.state, DecisionKind::Budget);
    assert!(
        f.h.state.decisions.items().any(|item| item.is_stale()),
        "the open item turned stale"
    );
    press(&mut f, Key::Esc);
    assert!(f.h.state.modal_stack.is_empty(), "the stale frame closed");
    assert!(f.h.state.decisions.is_empty(), "Close removed it");
    still(&f, 3, "stale close");
}

/// 14-A-05: the one-slot windows that are not the hub do not complete step four (each opened
/// through its production opener with a real payload), and the hub still does afterwards.
#[test]
fn the_agent_task_and_artifact_windows_do_not_complete_step_four() {
    use crate::conformance_21_3_keymap::support as keymap_support;
    use rustain::adapters::tui::artifact_window::{ArtifactBody, ArtifactWindowState};
    use rustain::adapters::tui::presentation::reducer;
    let mut f = plain(80, 24);
    armed_at(&mut f, 4);
    reducer::open_artifact_window(
        &mut f.h.state,
        ArtifactWindowState::new(
            "a".repeat(64),
            "evidence aaaaaa\n".to_owned(),
            ArtifactBody::Source(b"body\n".to_vec()),
        ),
        "Artifact · evidence aaaaaa".to_owned(),
    );
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::ArtifactWindow
    );
    still(&f, 4, "artifact window");
    press(&mut f, Key::Esc);
    rustain::adapters::tui::agent_window::open(
        &mut f.h.state,
        rustain::domain::models::AgentId::parse("proof-node").unwrap(),
    );
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::AgentWindow
    );
    still(&f, 4, "agent window");
    press(&mut f, Key::Esc);
    assert!(rustain::adapters::tui::task_window::open(
        &mut f.h.state,
        &keymap_support::plan_conversation(),
        &keymap_support::task_ref(1),
    ));
    f.paint();
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::TaskWindow
    );
    still(&f, 4, "task window");
    press(&mut f, Key::Esc);
    // Positive control: the hub completes it.
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('s')]);
    assert_eq!(step(&f), Some((TourStep::Help, Phase::CardDue)));
}
