//! K1 — the first-launch offer and hermeticity (AC6, D9, D22).
use super::support::*;
use rustain::adapters::builtin_commands::{self, CommandHost};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::Surface;
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::tour::{self, Origin, Phase, TourStep};
use rustain::domain::models::AppConfig;

#[test]
fn a_default_configuration_never_offers_the_tour() {
    assert!(!AppConfig::default().onboarding.offer_tour);
    let mut f = plain(80, 24);
    let config = Config::new();
    let off = AppConfig::default().onboarding.offer_tour;
    tour::install_at_startup(&mut f.h.state, off, Ok(config.file.clone()), false);
    assert!(f.h.state.tour.is_none());
    tick(&mut f);
    assert!(f.h.state.modal_stack.is_empty());
    assert!(!row_with(&f, "Tour").is_some_and(|row| row.contains("┌Tour")));
}

#[test]
fn the_offer_card_opens_on_the_first_idle_tick_and_enter_starts_step_one() {
    let mut f = plain(80, 24);
    let config = Config::new();
    launch_with_offer(&mut f, &config);
    let state = f.h.state.tour.clone().unwrap();
    assert_eq!(
        (state.origin, state.step, state.phase),
        (Origin::Offer, TourStep::Offer, Phase::CardDue)
    );
    assert!(
        f.h.state.modal_stack.is_empty(),
        "nothing opens before a tick"
    );
    card_opens(&mut f);
    assert_eq!(top_title(&f).as_deref(), Some("Tour"));
    assert_eq!(f.h.state.modal_stack.top().unwrap().surface, Surface::Tour);
    press(&mut f, Key::Enter);
    assert_eq!(top_title(&f).as_deref(), Some("Tour · 1 of 5"));
    assert_eq!(config.text(), "", "starting the tour writes nothing");
}

#[test]
fn escape_on_the_offer_skips_the_tour_for_good() {
    let mut f = plain(80, 24);
    let config = Config::new();
    launch_with_offer(&mut f, &config);
    card_opens(&mut f);
    press(&mut f, Key::Esc);
    assert!(f.h.state.tour.is_none() && f.h.state.modal_stack.is_empty());
    assert!(config.done());
    // The next launch reads the state file and offers nothing.
    let mut next = plain(80, 24);
    let done = rustain::infrastructure::tui_state_file::load(&config.file).tour_done;
    tour::install_at_startup(&mut next.h.state, true, Ok(config.file.clone()), done);
    assert!(next.h.state.tour.is_none());
    tick(&mut next);
    assert!(next.h.state.modal_stack.is_empty());
}

#[test]
fn quitting_with_the_offer_open_writes_nothing_and_the_offer_returns() {
    let mut f = plain(80, 24);
    let config = Config::new();
    std::fs::write(&config.file, "session_count = 3\n").unwrap();
    launch_with_offer(&mut f, &config);
    card_opens(&mut f);
    let before = config.text();
    // Quit over the card asks to quit and decides nothing about the tour.
    assert_eq!(
        press(&mut f, Key::Ctrl('q')),
        rustain::adapters::tui::app::InputAction::Quit
    );
    assert_eq!(config.text(), before, "the state file is byte-identical");
    assert!(!config.done());
    let mut next = plain(80, 24);
    let done = rustain::infrastructure::tui_state_file::load(&config.file).tour_done;
    tour::install_at_startup(&mut next.h.state, true, Ok(config.file.clone()), done);
    tick(&mut next);
    assert_eq!(top_title(&next).as_deref(), Some("Tour"), "offered again");
}

#[test]
fn the_offer_needs_the_switch_and_no_done_mark() {
    let config = Config::new();
    for (offer, done, expected) in [
        (true, false, true),
        (true, true, false),
        (false, false, false),
        (false, true, false),
    ] {
        let mut state = TuiState::new(80, 24);
        tour::install_at_startup(&mut state, offer, Ok(config.file.clone()), done);
        assert_eq!(state.tour.is_some(), expected, "offer={offer} done={done}");
    }
}

#[test]
fn a_plain_state_has_no_tour_and_no_persistence_seam() {
    let mut state = TuiState::new(80, 24);
    assert!(state.tour.is_none() && state.tour_persist.is_none());
    // A tour left in a plain test writes nowhere: leaving it needs no file and posts only Info.
    tour::command(&mut state);
    tour::tick(&mut state, false);
    assert!(state.tour_persist.is_none());
    let mut f = plain(80, 24);
    assert!(f.h.state.tour_persist.is_none());
    slash_tour(&mut f);
    press(&mut f, Key::Esc);
    assert!(f.h.state.tour.is_none());
    let (severity, _) = newest_notice(&f).unwrap();
    assert_eq!(
        severity,
        rustain::adapters::tui::notifications::NoticeSeverity::Info
    );
}

#[test]
fn the_attached_host_has_no_tour_command() {
    assert!(!builtin_commands::offered_in(CommandHost::Attached, "tour"));
    assert!(builtin_commands::offered_in(CommandHost::Local, "tour"));
    let notice = builtin_commands::unknown_command_notice(CommandHost::Attached, "tour");
    assert!(
        notice.starts_with("/tour is not available in an attached session. Detach ("),
        "{notice}"
    );
}
