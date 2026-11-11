//! K10 / AC20 / A21 (21-7 slice 7-E): History as an L2 frame. The rows are a snapshot of the
//! in-memory session index (no storage read at draw); `Enter` names the selected conversation
//! by id, `d` raises the delete decision for that **id** even after the list changed underneath,
//! `/` pushes CrossSearch and `Esc` returns to History with its selection, a cancelled delete
//! leaves History on top again, the delete-all clear empties an open window, and the launchpad
//! key line names `^X h History` at every local width. Keys travel as crossterm events through
//! the local host's decoder; assertions read painted cells, the one frame on the stack and the
//! effects the event loop would run. No sleeps.
use super::support::*;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{self, INERT_AFTER_PAINT};
use rustain::adapters::tui::keymap::table::attached_mission_remedy;
use rustain::adapters::tui::keymap::{AvailCtx, Host, Key, LeaderStep, leader_step};
use rustain::adapters::tui::presentation::Surface;
use rustain::domain::models::{ConversationSummary, FocusState};
use rustain::domain::services::session_index::SessionIndex;
use rustain::infrastructure::runtime::history_bridge;
use std::time::Duration;

async fn rig(width: u16, height: u16) -> Rig {
    Rig::plain(Workspace::new().await, width, height).await
}

fn now() -> i64 {
    rustain::domain::models::session_meta::now_unix()
}

/// A session index holding `(id, title)` newest first.
fn index(entries: &[(&str, &str)]) -> SessionIndex {
    let now = now();
    SessionIndex::build(
        entries
            .iter()
            .enumerate()
            .map(|(n, (id, title))| ConversationSummary {
                id: (*id).to_owned(),
                title: (*title).to_owned(),
                created_at: now - 10_000,
                updated_at: now - i64::try_from(n).unwrap(),
                message_count: 3,
                has_fork_source: false,
            })
            .collect(),
    )
}

/// `Ctrl+X h` through the local front door, the effect the loop runs, a production draw.
fn open_history(rig: &mut Rig) {
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('h')), InputAction::OpenHistory);
    history_bridge::open(&mut rig.f.h.state, &rig.f.sessions);
    rig.paint();
}

fn window(rig: &Rig) -> &rustain::adapters::tui::history_window::HistoryWindowState {
    rig.f
        .h
        .state
        .history_window
        .as_ref()
        .expect("History window")
}

fn selected(rig: &Rig) -> Option<String> {
    window(rig).selected_id().map(str::to_owned)
}

fn painted(rig: &Rig) -> String {
    lines(rig).join("\n")
}

#[tokio::test]
async fn the_chord_opens_history_at_every_width_from_the_snapshot_and_a_repaint_reads_nothing() {
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = rig(width, height).await;
        rig.f.sessions = index(&[("a", "Alpha session"), ("b", "Beta plans")]);
        let composer = composer_rows(&rig);
        open_history(&mut rig);
        assert_eq!(
            top_surface(&rig),
            Some(Surface::History),
            "{width}x{height}"
        );
        assert_eq!(top_title(&rig).as_deref(), Some("History"));
        let all = painted(&rig);
        assert!(
            all.contains("Alpha session") && all.contains("Beta plans"),
            "{width}x{height}: {all}"
        );
        assert!(all.contains("3 msgs") && all.contains("just now"), "{all}");
        assert!(
            !all.contains("120 columns"),
            "no sidebar-width floor: {all}"
        );
        assert_eq!(
            composer_rows(&rig),
            composer,
            "the frame writes no composer cell"
        );
        assert!(rail_line(&rig).contains("Open"), "{}", rail_line(&rig));
        // Repaints re-read nothing: the snapshot is taken once (a minute boundary may add one
        // re-write of the ages, never one per repaint).
        for _ in 0..3 {
            rig.paint();
        }
        assert!(
            window(&rig).builds() <= 2,
            "{} snapshots after four draws",
            window(&rig).builds()
        );
        // The way out is always painted and works.
        assert_eq!(rig.key(Key::Esc), InputAction::Consumed);
        assert_eq!(top_surface(&rig), None);
    }
}

#[tokio::test]
async fn enter_names_the_selected_id_and_finishing_the_open_closes_the_frame() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta"), ("c", "Gamma")]);
    open_history(&mut rig);
    assert_eq!(
        selected(&rig).as_deref(),
        Some("a"),
        "the first row until moved"
    );
    rig.key(Key::Char('j'));
    rig.paint();
    assert_eq!(selected(&rig).as_deref(), Some("b"));
    assert!(painted(&rig).contains("▸ "), "the selected row is marked");
    assert_eq!(
        rig.key(Key::Enter),
        InputAction::OpenHistoryEntry("b".to_owned())
    );
    // The loop loaded (or switched to) the conversation: the window is done and the Stage owns
    // focus.
    history_bridge::finish_open(&mut rig.f.h.state);
    rig.paint();
    assert_eq!(top_surface(&rig), None);
    assert_eq!(rig.f.h.state.focus, FocusState::Chat);
}

#[tokio::test]
async fn raw_key_variants_and_unbound_keys_never_act() {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta")]);
    open_history(&mut rig);
    let draft = rig.f.h.state.input_buffer.clone();
    // Only a plain Press acts (RU-29): a Release, a Repeat and an Alt variant are dropped.
    for (code, modifiers, kind) in [
        (KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
        (
            KeyCode::Char('d'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ),
        (KeyCode::Char('d'), KeyModifiers::NONE, KeyEventKind::Repeat),
        (KeyCode::Char('d'), KeyModifiers::ALT, KeyEventKind::Press),
        (KeyCode::Char('/'), KeyModifiers::ALT, KeyEventKind::Press),
    ] {
        assert_eq!(
            raw(&mut rig, code, modifiers, kind),
            None,
            "{code:?} {kind:?}"
        );
    }
    // Unbound printables are swallowed: nothing reaches the composer, the frame stays.
    for key in [
        Key::Char('x'),
        Key::Char('q'),
        Key::Char('m'),
        Key::Char('c'),
    ] {
        assert_eq!(rig.key(key), InputAction::Consumed, "{key:?}");
        assert_eq!(top_surface(&rig), Some(Surface::History), "{key:?}");
        assert_eq!(rig.f.h.state.input_buffer, draft, "{key:?}");
    }
    // Positive control: the same plain Press of a bound key acts.
    assert_eq!(
        rig.key(Key::Char('d')),
        InputAction::DeleteHistoryEntry("a".to_owned())
    );
}

#[tokio::test]
async fn history_delete_targets_the_selected_id_after_the_list_changes() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta plans"), ("c", "Gamma")]);
    open_history(&mut rig);
    rig.key(Key::Char('j'));
    rig.paint();
    assert_eq!(selected(&rig).as_deref(), Some("b"));
    // The list changes underneath: a new conversation lands first, `b` is now the third row.
    rig.f
        .sessions
        .touch("new", Some("Newest".to_owned()), Some(1));
    rig.paint();
    assert_eq!(rig.f.sessions.entries()[0].conversation_id, "new");
    assert_eq!(
        selected(&rig).as_deref(),
        Some("b"),
        "the selection is the id"
    );
    // `d` names `b`, not whatever sits at the old row index, and the effect raises the delete
    // decision for exactly that conversation (its title is painted in the question).
    let action = rig.key(Key::Char('d'));
    assert_eq!(action, InputAction::DeleteHistoryEntry("b".to_owned()));
    let InputAction::DeleteHistoryEntry(id) = action else {
        unreachable!()
    };
    history_bridge::delete_entry(&mut rig.f.h.state, &rig.f.sessions, "other-active", 2, &id);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::Decision));
    let all = painted(&rig);
    assert!(all.contains("Delete \"Beta plans\"?"), "{all}");
    assert!(!all.contains("Delete \"Alpha\"?"));
}

#[tokio::test]
async fn deleting_a_conversation_that_left_the_index_raises_nothing_and_says_so() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta")]);
    open_history(&mut rig);
    rig.f.sessions.remove("b");
    history_bridge::delete_entry(&mut rig.f.h.state, &rig.f.sessions, "a", 2, "b");
    assert!(
        !rig.f
            .h
            .state
            .decisions
            .holds(decision_queue::DecisionKind::DeleteConversations)
    );
    assert_eq!(
        top_surface(&rig),
        Some(Surface::History),
        "no decision frame"
    );
    let notices: Vec<String> = rig
        .f
        .h
        .state
        .notifications
        .records()
        .map(|record| record.message.clone())
        .collect();
    assert!(
        notices.iter().any(|m| m == history_bridge::GONE),
        "{notices:?}"
    );
    // The only open conversation is never offered for deletion (control: it is refused with
    // its own words and no decision either).
    history_bridge::delete_entry(&mut rig.f.h.state, &rig.f.sessions, "a", 1, "a");
    assert!(
        !rig.f
            .h
            .state
            .decisions
            .holds(decision_queue::DecisionKind::DeleteConversations)
    );
}

#[tokio::test]
async fn a_selected_conversation_that_leaves_shows_a_placeholder_and_is_never_rebound() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta"), ("c", "Gamma")]);
    open_history(&mut rig);
    rig.key(Key::Char('j'));
    rig.paint();
    rig.f.sessions.remove("b");
    rig.paint();
    let all = painted(&rig);
    assert!(all.contains("Beta is no longer available"), "{all}");
    assert!(all.contains("Alpha") && all.contains("Gamma"));
    assert_eq!(selected(&rig), None, "never rebound to the next row");
    // `Enter` and `d` are inert on the placeholder (their hints are not painted either).
    assert_eq!(rig.key(Key::Enter), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('d')), InputAction::Consumed);
    assert!(!rail_line(&rig).contains("Open"), "{}", rail_line(&rig));
    // Moving off the placeholder selects a real row again.
    rig.key(Key::Char('j'));
    rig.paint();
    assert_eq!(selected(&rig).as_deref(), Some("c"));
}

#[tokio::test]
async fn slash_pushes_cross_search_and_esc_returns_to_history_with_its_selection() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta"), ("c", "Gamma")]);
    open_history(&mut rig);
    rig.key(Key::Char('j'));
    rig.paint();
    assert_eq!(rig.key(Key::Char('/')), InputAction::OpenCrossSearch);
    assert_eq!(top_surface(&rig), Some(Surface::CrossSearch));
    assert_eq!(rig.f.h.state.modal_stack.len(), 2, "History stays beneath");
    // Esc asks the loop to close the search; the loop's effect leaves History on top, selected
    // where it was, and the base focus is not written.
    let focus = rig.f.h.state.focus.clone();
    assert_eq!(rig.key(Key::Esc), InputAction::CloseCrossSearch);
    rustain::adapters::tui::handlers::export::close_cross_search(&mut rig.f.h.state);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::History));
    assert_eq!(selected(&rig).as_deref(), Some("b"));
    assert_eq!(rig.f.h.state.focus, focus);
    assert!(painted(&rig).contains("Beta"));
}

#[tokio::test]
async fn a_cancelled_delete_restores_the_history_frame_as_the_top_frame() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta")]);
    open_history(&mut rig);
    rig.key(Key::Char('j'));
    rig.paint();
    let InputAction::DeleteHistoryEntry(id) = rig.key(Key::Char('d')) else {
        panic!("`d` must name the selected conversation");
    };
    history_bridge::delete_entry(&mut rig.f.h.state, &rig.f.sessions, "a", 2, &id);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::Decision));
    rig.f
        .clock
        .advance(INERT_AFTER_PAINT + Duration::from_millis(1));
    let action = rig.key(Key::Char('n'));
    assert_eq!(action, InputAction::CancelDelete);
    // The loop's effect: the cancelled delete settles and History is the top frame again with
    // its selection, the composer focus where it was.
    let focus = rig.f.h.state.focus.clone();
    decision_queue::apply_resolved_action(&mut rig.f.h.state, action);
    rig.paint();
    assert_eq!(top_surface(&rig), Some(Surface::History));
    assert_eq!(selected(&rig).as_deref(), Some("b"));
    assert_eq!(rig.f.h.state.focus, focus);
    assert!(
        !rig.f
            .h
            .state
            .decisions
            .holds(decision_queue::DecisionKind::DeleteConversations)
    );
}

#[tokio::test]
async fn delete_all_empties_the_open_window_and_its_keys_go_inert() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta")]);
    open_history(&mut rig);
    assert!(painted(&rig).contains("Alpha"));
    // The confirmed bulk delete clears the index before it spawns the deletions.
    rig.f.sessions.clear();
    rig.paint();
    let all = painted(&rig);
    assert!(all.contains("No conversations"), "{all}");
    assert!(!all.contains("Alpha") && !all.contains("Beta"));
    assert_eq!(selected(&rig), None);
    assert_eq!(rig.key(Key::Enter), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('d')), InputAction::Consumed);
    assert!(!rail_line(&rig).contains("Open"), "{}", rail_line(&rig));
    assert_eq!(
        top_surface(&rig),
        Some(Surface::History),
        "the window stays open"
    );
}

#[tokio::test]
async fn titles_are_sanitized_and_the_age_and_count_survive_a_long_title() {
    let mut rig = rig(60, 16).await;
    let evil = format!("\u{1b}[31mred\u{202e}\nforged row {}", "x".repeat(120));
    rig.f.sessions = index(&[("a", &evil)]);
    open_history(&mut rig);
    let all = painted(&rig);
    assert!(
        !all.chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
        "{all:?}"
    );
    assert!(all.contains("red forged row"), "{all}");
    let row = lines(&rig)
        .into_iter()
        .find(|row| row.contains("red forged row"))
        .expect("the row");
    assert!(
        row.contains("just now · 3 msgs") || row.contains("· 3 msgs"),
        "{row}"
    );
    assert!(
        row.contains("..."),
        "the title is cut, not the facts: {row}"
    );
}

#[test]
fn the_attached_host_refuses_the_history_chord_with_the_one_a20_remedy() {
    for width in [60, 80, 130] {
        let avail = AvailCtx::attached(width, false);
        match leader_step(Host::Attached, Key::Char('h'), &avail, "Input") {
            LeaderStep::Unavailable(_, remedy) => {
                assert_eq!(remedy, attached_mission_remedy(), "{width}");
            }
            other => panic!("attached `h` must be refused: {other:?}"),
        }
        // Control: the same chord runs in the local host at the same width (no 120 floor).
        let local = AvailCtx::local(width, rustain::domain::models::visual::DensityMode::Focus);
        assert!(matches!(
            leader_step(Host::Local, Key::Char('h'), &local, "Input"),
            LeaderStep::Run(_)
        ));
    }
}

/// 7E-08: the bulk-delete effect (`history_bridge::clear_all`) empties an open window even when
/// a row was selected first: the selection names a conversation that no longer exists, and a
/// "no longer available" placeholder over an empty list would hide the empty state. An ordinary
/// single removal keeps A3's placeholder (see `a_selected_conversation_that_leaves_...`).
#[tokio::test]
async fn bulk_clear_with_an_explicit_selection_paints_the_empty_state_not_a_placeholder() {
    let mut rig = rig(80, 24).await;
    rig.f.sessions = index(&[("a", "Alpha"), ("b", "Beta")]);
    open_history(&mut rig);
    rig.key(Key::Char('j'));
    rig.paint();
    assert_eq!(selected(&rig).as_deref(), Some("b"));
    history_bridge::clear_all(&mut rig.f.h.state, &mut rig.f.sessions);
    rig.paint();
    let all = painted(&rig);
    assert!(all.contains("No conversations"), "{all}");
    assert!(!all.contains("no longer available"), "{all}");
    assert_eq!(selected(&rig), None);
    assert_eq!(rig.key(Key::Enter), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('d')), InputAction::Consumed);
}
