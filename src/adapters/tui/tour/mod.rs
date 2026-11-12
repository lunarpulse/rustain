//! The onboarding tour (Story 21.14, slice 14-A): a short walk through the five things a person
//! actually does — send a message, inspect a tool call, answer a decision, open Mission Control
//! or the palette, and open help.
//!
//! The tour owns one frame, [`Surface::Tour`](crate::adapters::tui::presentation::Surface),
//! and one piece of session state, [`TourState`]. A step is *armed* when its card is closed with
//! `Enter`; it completes only when the person really performs its action, observed at the
//! production seams: the plain-text branches of `app::submit_message`, `reducer::open_frame`
//! (S-TOOL, Mission Control, the palette, help became the top frame) and
//! `decision_queue::keys::after_answer`. The tour never creates a tool call, a decision or a
//! message and never opens a frame for the person except its own card: it is not authority to
//! bypass any queue.
//!
//! A card opens only when nothing else owns the screen (the same typing, leader and frame
//! clauses as an arriving decision, but waiting decisions do not block it): [`tick`] pushes a
//! due card on the first tick after those clear. Leaving (`Esc`, `Ctrl+C`) and finishing both
//! write `tour_done` through [`TourPersist`]; a launch that ends with the offer unanswered
//! writes nothing, so the offer returns until it is answered.

pub mod copy;

use std::path::PathBuf;

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_tour::TourCmd;
use crate::adapters::tui::keymap::{self, CommandId, Context, Host, Key, Resolved};
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::{Surface, is_open, reducer};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::FocusState;

/// How the tour started: the first-launch offer, or the person's own `/tour`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Offer,
    Command,
}

/// The offer card, then the five steps in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TourStep {
    Offer,
    Send,
    Inspect,
    Decide,
    Hub,
    Help,
}

impl TourStep {
    /// Steps in the tour (the offer card is not one of them).
    pub const COUNT: u8 = 5;

    /// `1..=5` for a step, `0` for the offer card.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Offer => 0,
            Self::Send => 1,
            Self::Inspect => 2,
            Self::Decide => 3,
            Self::Hub => 4,
            Self::Help => 5,
        }
    }

    const fn next(self) -> Option<Self> {
        match self {
            Self::Offer => Some(Self::Send),
            Self::Send => Some(Self::Inspect),
            Self::Inspect => Some(Self::Decide),
            Self::Decide => Some(Self::Hub),
            Self::Hub => Some(Self::Help),
            Self::Help => None,
        }
    }
}

/// Where the current step is: its card is due, its card is open, or the step is armed and
/// waiting for the real action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    CardDue,
    Card,
    Armed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TourState {
    pub origin: Origin,
    pub step: TourStep,
    pub phase: Phase,
}

/// Where the "done" mark is written (`<config_dir>/state.toml`), or why that location could not
/// be resolved at startup; installed only by the local event loop. An unresolved location is kept
/// so that finishing the tour says it could not save, exactly like a failed write.
#[derive(Debug, Clone)]
pub struct TourPersist {
    path: Result<PathBuf, String>,
}

impl TourPersist {
    fn mark_done(&self) -> Result<(), String> {
        match &self.path {
            Ok(path) => crate::infrastructure::tui_state_file::mark_tour_done(path)
                .map_err(|error| error.to_string()),
            Err(why) => Err(why.clone()),
        }
    }
}

/// Startup of the local TUI: remember where "done" is written (or why it cannot be) and, when
/// the first-launch offer is due (`[onboarding] offer_tour` is on and the state file has no
/// `tour_done`), queue the offer card. A launch that ends with the offer unanswered writes
/// nothing, so it returns.
pub fn install_at_startup(
    state: &mut TuiState,
    offer_tour: bool,
    state_file: Result<PathBuf, String>,
    tour_done: bool,
) {
    state.tour_persist = Some(TourPersist { path: state_file });
    if offer_tour && !tour_done {
        state.tour = Some(TourState {
            origin: Origin::Offer,
            step: TourStep::Offer,
            phase: Phase::CardDue,
        });
    }
}

/// `/tour`: start the tour at step 1, or bring the armed step's card back (progress is kept).
/// The person asked, so the typing grace does not delay the card.
pub fn command(state: &mut TuiState) {
    match state.tour.as_mut() {
        None => {
            state.tour = Some(TourState {
                origin: Origin::Command,
                step: TourStep::Send,
                phase: Phase::CardDue,
            });
        }
        Some(tour) if tour.phase == Phase::Armed => tour.phase = Phase::CardDue,
        Some(_) => {}
    }
    if may_open(state, false, false) {
        open_card(state);
    }
}

/// The event loop's tick: open a due card once nothing else owns the screen, and restore a card
/// that something other than the person's own key took off the stack.
pub fn tick(state: &mut TuiState, streaming: bool) {
    let Some(tour) = state.tour.as_ref() else {
        return;
    };
    match tour.phase {
        Phase::CardDue => {
            if may_open(state, streaming, true) {
                open_card(state);
            }
        }
        Phase::Card => {
            if !is_open(state, Surface::Tour)
                && let Some(tour) = state.tour.as_mut()
            {
                tour.phase = Phase::CardDue;
            }
        }
        Phase::Armed => {}
    }
}

/// A card opens over an idle base only: nothing on the stack, the composer empty, nobody
/// typing, no pending leader or reverse search, and no turn streaming. Waiting decisions do
/// not block it (they own their chip and the strip); `grace` adds the typing grace.
fn may_open(state: &TuiState, streaming: bool, grace: bool) -> bool {
    state.modal_stack.is_empty()
        && matches!(state.focus, FocusState::Input | FocusState::Chat)
        && state.input_buffer.is_empty()
        && state.pending_leader.is_none()
        && !state.reverse_search.is_open()
        && !streaming
        && !(grace
            && state.last_printable_at.is_some_and(|at| {
                state.notifications.now().saturating_duration_since(at)
                    < crate::adapters::tui::decision_queue::TYPING_GRACE
            }))
}

fn open_card(state: &mut TuiState) {
    let Some(tour) = state.tour.as_mut() else {
        return;
    };
    tour.phase = Phase::Card;
    let title = copy::card_title(tour.step);
    reducer::open_tour(state, title);
}

/// A frame became the top frame (`reducer::open_frame`): complete the armed step when it is
/// the frame that step asks for. S-BLOCK, the other Mission Control windows, History and a
/// card itself never complete anything.
pub(crate) fn observe_open(state: &mut TuiState, surface: Surface) {
    let Some(tour) = state.tour.as_ref() else {
        return;
    };
    let completes = tour.phase == Phase::Armed
        && match tour.step {
            TourStep::Inspect => matches!(surface, Surface::ToolInspector(_)),
            TourStep::Hub => matches!(surface, Surface::MissionControl | Surface::CommandPalette),
            TourStep::Help => surface == Surface::Help,
            TourStep::Offer | TourStep::Send | TourStep::Decide => false,
        };
    if completes {
        advance(state);
    }
}

/// A plain-text message was submitted (`app::submit_message`). A read-only tab refuses the
/// message later in the event loop, so it does not count.
pub(crate) fn observe_submit(state: &mut TuiState) {
    if state.local_read_only {
        return;
    }
    observe_armed(state, TourStep::Send);
}

/// A decision was answered through its rail (`decision_queue::keys::after_answer`): deferring
/// and closing a stale item never reach it.
pub(crate) fn observe_answer(state: &mut TuiState) {
    observe_armed(state, TourStep::Decide);
}

fn observe_armed(state: &mut TuiState, step: TourStep) {
    if state
        .tour
        .as_ref()
        .is_some_and(|tour| tour.phase == Phase::Armed && tour.step == step)
    {
        advance(state);
    }
}

/// The armed step was done: the next card is due, or the tour is complete.
fn advance(state: &mut TuiState) {
    let Some(tour) = state.tour.as_mut() else {
        return;
    };
    match tour.step.next() {
        Some(next) => {
            tour.step = next;
            tour.phase = Phase::CardDue;
            state.needs_redraw = true;
        }
        None => finish(state, Ending::Complete),
    }
}

#[derive(Clone, Copy)]
enum Ending {
    Complete,
    Left,
}

/// The tour is over for good: the card closes, the state is dropped, "done" is written and the
/// person is told how to take it again (or where help lives).
fn finish(state: &mut TuiState, ending: Ending) {
    if is_open(state, Surface::Tour) {
        reducer::close(state, Surface::Tour);
    }
    state.tour = None;
    if let Some(persist) = state.tour_persist.as_ref()
        && let Err(error) = persist.mark_done()
    {
        notifications::warning(
            state,
            NoticeSource::Help,
            format!(
                "Could not save the tour as finished. It will be offered again next time.\n{error}"
            ),
        );
    }
    match ending {
        Ending::Complete => {
            notifications::success(state, NoticeSource::Help, copy::complete_notice());
        }
        Ending::Left => notifications::info(state, NoticeSource::Help, copy::closed_notice()),
    }
    state.needs_redraw = true;
}

/// The Tour card is the top frame.
#[must_use]
pub fn is_top(state: &TuiState) -> bool {
    state
        .modal_stack
        .top()
        .is_some_and(|frame| frame.surface == Surface::Tour)
}

/// The card's keys. `Enter` begins the tour or arms the step; `n` skips a step (not on the offer
/// card); `Esc` and `Ctrl+C` leave. Any other key is swallowed here, never reaching the composer.
pub(crate) fn on_key(state: &mut TuiState, key: Key, host: Host) -> InputAction {
    if !is_top(state) {
        return InputAction::Consumed;
    }
    let Resolved::Hit(row) = keymap::resolve(Context::Tour, host, &[key]) else {
        return InputAction::Consumed;
    };
    let CommandId::Tour(command) = row.command else {
        return InputAction::Consumed;
    };
    let Some(step) = state.tour.as_ref().map(|tour| tour.step) else {
        return InputAction::Consumed;
    };
    match (command, step) {
        (TourCmd::Continue, TourStep::Offer) => show_step(state, TourStep::Send),
        (TourCmd::Continue, _) => {
            if let Some(tour) = state.tour.as_mut() {
                tour.phase = Phase::Armed;
            }
            reducer::close(state, Surface::Tour);
        }
        (TourCmd::SkipStep, TourStep::Offer) => {}
        (TourCmd::SkipStep, step) => match step.next() {
            Some(next) => show_step(state, next),
            None => finish(state, Ending::Complete),
        },
        (TourCmd::Leave, _) => finish(state, Ending::Left),
    }
    state.needs_redraw = true;
    InputAction::Consumed
}

/// Show `step`'s card in the frame that is already open.
fn show_step(state: &mut TuiState, step: TourStep) {
    if let Some(tour) = state.tour.as_mut() {
        tour.step = step;
        tour.phase = Phase::Card;
    }
    reducer::set_tour_title(state, copy::card_title(step));
}

/// The raw press-only rule of the card (RU-29): only a plain Press acts, and a Ctrl letter only
/// where the registry binds it.
pub(crate) fn ignores_raw_key(state: &TuiState, key: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if !is_top(state) {
        return false;
    }
    if key.kind != KeyEventKind::Press {
        return true;
    }
    match key.code {
        KeyCode::Char(c) => {
            let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
            !plain && !(key.modifiers == KeyModifiers::CONTROL && "xpqc".contains(c))
        }
        _ => !key.modifiers.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::presentation::consistent;
    use crate::domain::clock::MockClock;
    use crate::domain::models::NotificationsConfig;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn state() -> (TuiState, Arc<MockClock>) {
        let mut state = TuiState::new(80, 24);
        let clock = Arc::new(MockClock::new(Instant::now()));
        state.notifications = notifications::NotificationStore::from_config(
            &NotificationsConfig::default(),
            clock.clone(),
        );
        (state, clock)
    }

    fn started(state: &mut TuiState) {
        command(state);
        assert!(is_top(state));
    }

    fn press(state: &mut TuiState, key: Key) {
        assert_eq!(on_key(state, key, Host::Local), InputAction::Consumed);
    }

    fn step_of(state: &TuiState) -> Option<(TourStep, Phase)> {
        state.tour.as_ref().map(|tour| (tour.step, tour.phase))
    }

    /// A state file in a scratch directory, installed the way the event loop installs it.
    fn persisted(state: &mut TuiState) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        state.tour_persist = Some(TourPersist {
            path: Ok(path.clone()),
        });
        (dir, path)
    }

    #[test]
    fn story21_14_command_starts_at_step_one_and_never_creates_anything() {
        let (mut state, _) = state();
        assert!(state.tour.is_none(), "a plain state has no tour");
        started(&mut state);
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Card)));
        assert_eq!(state.tour.as_ref().unwrap().origin, Origin::Command);
        assert_eq!(state.modal_stack.top().unwrap().title(), "Tour · 1 of 5");
        assert!(state.decisions.is_empty());
        assert!(consistent(&state));
    }

    #[test]
    fn story21_14_enter_arms_the_step_and_the_command_reopens_the_same_card() {
        let (mut state, _) = state();
        started(&mut state);
        press(&mut state, Key::Enter);
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Armed)));
        assert!(state.modal_stack.is_empty());
        command(&mut state);
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Card)));
        assert_eq!(state.modal_stack.top().unwrap().title(), "Tour · 1 of 5");
    }

    #[test]
    fn story21_14_the_offer_card_continues_to_step_one_and_has_no_skip_step() {
        let (mut state, _) = state();
        install_at_startup(&mut state, true, Err("not saved".into()), false);
        assert_eq!(step_of(&state), Some((TourStep::Offer, Phase::CardDue)));
        tick(&mut state, false);
        assert_eq!(state.modal_stack.top().unwrap().title(), "Tour");
        press(&mut state, Key::Char('n'));
        assert_eq!(
            step_of(&state),
            Some((TourStep::Offer, Phase::Card)),
            "n is inert on the offer"
        );
        press(&mut state, Key::Enter);
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Card)));
        assert_eq!(state.modal_stack.top().unwrap().title(), "Tour · 1 of 5");
        assert_eq!(state.modal_stack.len(), 1, "the same frame is retitled");
    }

    #[test]
    fn story21_14_skipping_walks_the_steps_and_finishes_after_the_last() {
        let (mut state, _) = state();
        started(&mut state);
        for expected in [2, 3, 4, 5] {
            press(&mut state, Key::Char('n'));
            assert_eq!(
                state.modal_stack.top().unwrap().title(),
                format!("Tour · {expected} of 5")
            );
        }
        press(&mut state, Key::Char('n'));
        assert!(state.tour.is_none());
        assert!(state.modal_stack.is_empty());
        assert!(consistent(&state));
    }

    #[test]
    fn story21_14_leaving_and_finishing_write_done_and_say_so() {
        for (key, expected) in [
            (Key::Esc, "Tour closed. Run /tour to take it again."),
            (Key::Ctrl('c'), "Tour closed. Run /tour to take it again."),
        ] {
            let (mut state, _) = state();
            let (_dir, path) = persisted(&mut state);
            started(&mut state);
            press(&mut state, key);
            assert!(state.tour.is_none() && state.modal_stack.is_empty());
            assert!(crate::infrastructure::tui_state_file::load(&path).tour_done);
            let last = state.notifications.records().next_back().unwrap();
            assert_eq!(last.message, expected);
            assert_eq!(last.severity, notifications::NoticeSeverity::Info);
        }
        let (mut state, _) = state();
        let (_dir, path) = persisted(&mut state);
        started(&mut state);
        for _ in 0..5 {
            press(&mut state, Key::Char('n'));
        }
        assert!(crate::infrastructure::tui_state_file::load(&path).tour_done);
        let last = state.notifications.records().next_back().unwrap();
        assert_eq!(last.severity, notifications::NoticeSeverity::Success);
        assert!(
            last.message
                .starts_with("Tour complete. Help is always one key away: ")
        );
    }

    #[test]
    fn story21_14_a_failed_save_warns_and_the_session_continues() {
        let (mut state, _) = state();
        let dir = tempfile::tempdir().unwrap();
        state.tour_persist = Some(TourPersist {
            path: Ok(dir.path().join("missing").join("state.toml")),
        });
        started(&mut state);
        press(&mut state, Key::Esc);
        assert!(state.tour.is_none());
        let warning = state
            .notifications
            .records()
            .find(|record| record.severity == notifications::NoticeSeverity::Warning)
            .expect("a failed save is announced");
        assert!(warning.message.starts_with(
            "Could not save the tour as finished. It will be offered again next time.\n"
        ));
    }

    #[test]
    fn story21_14_a_due_card_waits_for_an_idle_base() {
        let (mut state, clock) = state();
        install_at_startup(&mut state, true, Err("not saved".into()), false);
        state.input_buffer = "draft".into();
        tick(&mut state, false);
        assert!(state.modal_stack.is_empty(), "never over a draft");
        state.input_buffer.clear();
        state.last_printable_at = Some(state.notifications.now());
        tick(&mut state, false);
        assert!(
            state.modal_stack.is_empty(),
            "never inside the typing grace"
        );
        clock.advance(crate::adapters::tui::decision_queue::TYPING_GRACE);
        tick(&mut state, true);
        assert!(state.modal_stack.is_empty(), "never while a turn streams");
        tick(&mut state, false);
        assert!(is_top(&state));
    }

    #[test]
    fn story21_14_only_the_armed_steps_action_completes_it() {
        let (mut state, _) = state();
        started(&mut state);
        press(&mut state, Key::Enter);
        // Out-of-order surfaces and the wrong phase do nothing.
        reducer::open(&mut state, Surface::Help);
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Armed)));
        reducer::close(&mut state, Surface::Help);
        observe_submit(&mut state);
        assert_eq!(step_of(&state), Some((TourStep::Inspect, Phase::CardDue)));
        observe_submit(&mut state);
        assert_eq!(step_of(&state), Some((TourStep::Inspect, Phase::CardDue)));
    }

    #[test]
    fn story21_14_a_read_only_tab_submission_does_not_count() {
        let (mut state, _) = state();
        started(&mut state);
        press(&mut state, Key::Enter);
        state.local_read_only = true;
        observe_submit(&mut state);
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Armed)));
    }

    #[test]
    fn story21_14_unbound_printable_keys_and_releases_are_swallowed() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let (mut state, _) = state();
        started(&mut state);
        press(&mut state, Key::Char('x'));
        assert_eq!(step_of(&state), Some((TourStep::Send, Phase::Card)));
        assert!(state.input_buffer.is_empty());
        let key = |code, modifiers, kind| KeyEvent::new_with_kind(code, modifiers, kind);
        for ignored in [
            key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
            key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
            key(KeyCode::Enter, KeyModifiers::ALT, KeyEventKind::Press),
            key(KeyCode::Char('n'), KeyModifiers::ALT, KeyEventKind::Press),
            key(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            ),
            key(KeyCode::Esc, KeyModifiers::SHIFT, KeyEventKind::Press),
        ] {
            assert!(ignores_raw_key(&state, &ignored), "{ignored:?}");
        }
        for acting in [
            key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press),
            key(KeyCode::Char('n'), KeyModifiers::NONE, KeyEventKind::Press),
            key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            ),
        ] {
            assert!(!ignores_raw_key(&state, &acting), "{acting:?}");
        }
    }
}
