//! K2 — the cards and their keys (AC1, AC3, AC4, D3, D4).
use super::support::*;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, Arrival, DecisionDraft, DecisionKind, Raised};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::tour::{Phase, TourStep, copy};

const RAIL: &str = "[Enter] Continue  [n] Skip step  [Esc] Skip tour";

fn started(width: u16, height: u16) -> Fixture {
    let mut f = plain(width, height);
    slash_tour(&mut f);
    f
}

#[test]
fn every_card_paints_its_title_body_and_generated_rail() {
    let mut f = started(80, 24);
    assert_eq!(top_title(&f).as_deref(), Some("Tour · 1 of 5"));
    let expected = [
        (
            TourStep::Send,
            "Type a question in the message box and send it with Enter.",
        ),
        (TourStep::Inspect, "No tool call yet? n skips this step."),
        (TourStep::Decide, "Nothing waiting? n skips this step."),
        (TourStep::Hub, "or the command palette with Ctrl+P."),
        (TourStep::Help, "It lists every key for where you are."),
    ];
    for (index, (step, phrase)) in expected.into_iter().enumerate() {
        assert_eq!(f.h.state.tour.as_ref().unwrap().step, step);
        assert_eq!(
            top_title(&f).unwrap(),
            format!("Tour · {} of 5", index + 1),
            "{step:?}"
        );
        let text = card_text(&f);
        assert!(
            text.contains(phrase),
            "{step:?}: {phrase}\n{}",
            rows(&f).join("\n")
        );
        for paragraph in copy::body(step, false) {
            assert!(text.contains(&paragraph), "{step:?}: {paragraph}\n{text}");
        }
        assert!(row_with(&f, RAIL).is_some(), "{step:?}: the generated rail");
        if step != TourStep::Help {
            press(&mut f, Key::Char('n'));
        }
    }
}

#[test]
fn the_offer_card_has_no_skip_step_and_names_the_command() {
    let mut f = plain(80, 24);
    launch_with_offer(&mut f, &Config::new());
    card_opens(&mut f);
    assert!(row_with(&f, "[Enter] Continue  [Esc] Skip tour").is_some());
    assert!(
        row_with(&f, "[n]").is_none(),
        "n is not painted on the offer"
    );
    assert!(row_with(&f, "Run /tour any time to take it again.").is_some());
    let before = f.h.state.tour.clone();
    press(&mut f, Key::Char('n'));
    assert_eq!(f.h.state.tour, before, "n is inert on the offer");
}

#[test]
fn enter_n_escape_and_ctrl_c_act_by_value() {
    // Enter arms the step and closes the card.
    let mut f = started(80, 24);
    press(&mut f, Key::Enter);
    assert_eq!(step(&f), Some((TourStep::Send, Phase::Armed)));
    assert!(f.h.state.modal_stack.is_empty());
    // n shows the next card in the same frame.
    let mut f = started(80, 24);
    let id = f.h.state.modal_stack.top().unwrap().id;
    press(&mut f, Key::Char('n'));
    assert_eq!(step(&f), Some((TourStep::Inspect, Phase::Card)));
    assert_eq!(f.h.state.modal_stack.top().unwrap().id, id);
    // Esc and Ctrl+C leave from any card and say how to come back.
    for key in [Key::Esc, Key::Ctrl('c')] {
        let mut f = started(80, 24);
        press_all(&mut f, &[Key::Char('n'), Key::Char('n')]);
        press(&mut f, key);
        assert!(f.h.state.tour.is_none() && f.h.state.modal_stack.is_empty());
        assert_eq!(
            newest_notice(&f),
            Some((
                NoticeSeverity::Info,
                "Tour closed. Run /tour to take it again.".to_owned()
            ))
        );
        assert!(f.h.state.input_buffer.is_empty());
    }
    // Skipping past the last step finishes the tour with the Success notice.
    let mut f = started(80, 24);
    for _ in 0..4 {
        press(&mut f, Key::Char('n'));
    }
    assert_eq!(top_title(&f).as_deref(), Some("Tour · 5 of 5"));
    press(&mut f, Key::Char('n'));
    let (severity, message) = newest_notice(&f).unwrap();
    assert_eq!(severity, NoticeSeverity::Success);
    assert_eq!(
        message,
        "Tour complete. Help is always one key away: Ctrl+X, ?."
    );
}

#[test]
fn the_command_reopens_the_armed_step_without_resetting_progress() {
    let mut f = started(80, 24);
    press_all(&mut f, &[Key::Char('n'), Key::Char('n'), Key::Enter]);
    assert_eq!(step(&f), Some((TourStep::Decide, Phase::Armed)));
    slash_tour(&mut f);
    assert_eq!(step(&f), Some((TourStep::Decide, Phase::Card)));
    assert_eq!(top_title(&f).as_deref(), Some("Tour · 3 of 5"));
    // Asking again while the card is open changes nothing.
    slash_tour(&mut f);
    assert_eq!(step(&f), Some((TourStep::Decide, Phase::Card)));
    assert_eq!(f.h.state.modal_stack.len(), 1);
}

#[test]
fn printable_keys_are_swallowed_by_the_card_and_type_without_it() {
    let mut f = started(80, 24);
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    let tour = f.h.state.tour.clone();
    let scroll = f.h.state.scroll_offset();
    for key in ['x', 'j', 'k', '/', '@', 'q', '?', ' ', 'N'] {
        press(&mut f, Key::Char(key));
        assert!(
            f.h.state.input_buffer.is_empty(),
            "{key:?} reached the composer"
        );
        assert_eq!(f.h.state.modal_stack.top(), Some(&frame), "{key:?}");
        assert_eq!(f.h.state.tour, tour, "{key:?}");
        assert_eq!(f.h.state.scroll_offset(), scroll, "{key:?}");
    }
    // Positive control: the same key types in the composer once the card is closed.
    press(&mut f, Key::Enter);
    press(&mut f, Key::Char('x'));
    assert_eq!(f.h.state.input_buffer, "x");
}

#[test]
fn only_a_plain_press_acts_on_a_card() {
    let mut f = started(80, 24);
    let tour = f.h.state.tour.clone();
    let frame = f.h.state.modal_stack.top().cloned();
    for (code, modifiers, kind) in [
        (KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
        (KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
        (KeyCode::Enter, KeyModifiers::ALT, KeyEventKind::Press),
        (KeyCode::Enter, KeyModifiers::CONTROL, KeyEventKind::Press),
        (
            KeyCode::Char('n'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ),
        (KeyCode::Char('n'), KeyModifiers::ALT, KeyEventKind::Press),
        (
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ),
        (KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release),
        (KeyCode::Esc, KeyModifiers::SHIFT, KeyEventKind::Press),
        (
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        ),
    ] {
        assert_eq!(
            raw(&mut f, key_event(code, modifiers, kind)),
            InputAction::Ignored,
            "{code:?} {modifiers:?} {kind:?}"
        );
        assert_eq!(f.h.state.tour, tour);
        assert_eq!(f.h.state.modal_stack.top().cloned(), frame);
    }
    // Positive control: the plain press acts.
    raw(
        &mut f,
        key_event(KeyCode::Char('n'), KeyModifiers::NONE, KeyEventKind::Press),
    );
    assert_eq!(step(&f), Some((TourStep::Inspect, Phase::Card)));
    raw(
        &mut f,
        key_event(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press),
    );
    assert_eq!(step(&f), Some((TourStep::Inspect, Phase::Armed)));
}

#[test]
fn the_leader_the_palette_and_quit_work_over_a_card_and_return_to_it() {
    let mut f = started(80, 24);
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    press(&mut f, Key::Ctrl('x'));
    assert!(f.h.state.pending_leader.is_some());
    press(&mut f, Key::Esc);
    assert!(f.h.state.pending_leader.is_none());
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    press(&mut f, Key::Ctrl('p'));
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::CommandPalette
    );
    assert_eq!(f.h.state.modal_stack.len(), 2);
    press(&mut f, Key::Esc);
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(
        step(&f),
        Some((TourStep::Send, Phase::Card)),
        "nothing was left or armed"
    );
    assert_eq!(press(&mut f, Key::Ctrl('q')), InputAction::Quit);
    assert_eq!(
        f.h.state.modal_stack.top(),
        Some(&frame),
        "quit decides nothing about the tour"
    );
}

#[test]
fn a_decision_covers_the_card_and_the_card_returns_when_it_closes() {
    let mut f = started(80, 24);
    let frame = f.h.state.modal_stack.top().unwrap().clone();
    assert_eq!(
        decision_queue::raise(
            &mut f.h.state,
            DecisionDraft::new(DecisionKind::Budget, "budget detail"),
            Arrival::Automatic,
        ),
        Raised::Focused
    );
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        Surface::Decision
    );
    assert_eq!(f.h.state.modal_stack.len(), 2);
    // Deferring the decision returns exactly the card; the answer is not the tour's step.
    press(&mut f, Key::Esc);
    assert_eq!(f.h.state.modal_stack.top(), Some(&frame));
    assert_eq!(step(&f), Some((TourStep::Send, Phase::Card)));
    assert!(presentation::consistent(&f.h.state));
}
