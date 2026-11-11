//! E2E tests for cross-conversation search (Story 4-4 AC5, AC6, AC7).
//!
//! Exercises the input-dispatch layer (/ key in the History window, j/k navigation,
//! Enter, Esc, Backspace) and the widget render path. The actual async
//! scan (`run_cross_search`) is covered by unit tests in
//! `domain/services/cross_search.rs`.

use crate::common;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::history_window;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout::compute_layout;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::occlusion::OpaqueRects;
use rustain::adapters::tui::presentation::{Shell, Surface, compose, is_open};
use rustain::adapters::tui::state::{CrossSearchState, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::cross_search;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::services::cross_search::CrossSearchResult;
use rustain::domain::services::session_index::SessionIndex;

// ── Helpers ────────────────────────────────────────────────────────────────

/// The History window (empty index) painted once, so its rail hints are recorded: the
/// state `/` is pressed in.
fn history_state() -> TuiState {
    let mut s = TuiState::new(130, 30);
    history_window::open(&mut s, &SessionIndex::new(), 1_700_000_000);
    let area = ratatui::layout::Rect::new(0, 0, 130, 30);
    let layout =
        compute_layout(area, &s.theme, &s.input_buffer, 1, false).expect("a real chat viewport");
    let mut terminal = Terminal::new(TestBackend::new(130, 30)).unwrap();
    terminal
        .draw(|frame| {
            let shell = Shell::new(frame.area(), &layout);
            compose(
                frame,
                &s,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    assert!(history_window::is_top(&s));
    s
}

fn cross_search_state() -> TuiState {
    let mut s = history_state();
    assert_eq!(
        handle_input(&mut s, &DomainInputEvent::KeyPress('/')),
        InputAction::OpenCrossSearch
    );
    assert_cross_search_frame(&s);
    s
}

/// CrossSearch is the top frame, above the History window it was opened from; the base focus
/// is untouched.
fn assert_cross_search_frame(state: &TuiState) {
    assert_eq!(state.focus, TuiState::new(130, 30).focus);
    assert!(is_open(state, Surface::CrossSearch));
    assert_eq!(state.modal_stack.len(), 2);
    assert!(is_open(state, Surface::History));
    let frame = state.modal_stack.top().expect("CrossSearch owns a frame");
    assert_eq!(frame.surface, Surface::CrossSearch);
    assert_eq!(frame.conversation_id, None);
}

fn claim_scan(state: &mut TuiState) -> (u64, u64, String) {
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchScanAction, apply_cross_search_query_change,
    };
    let CrossSearchScanAction::Spawn {
        query,
        frame_id,
        revision,
    } = apply_cross_search_query_change(state)
    else {
        panic!("the owned query must claim a real scan");
    };
    assert_cross_search_frame(state);
    assert_eq!(frame_id, state.modal_stack.top().unwrap().id);
    assert_eq!(revision, state.cross_search.request_revision);
    (frame_id, revision, query)
}

fn stub_result(id: &str, title: &str, excerpt: &str, msg_idx: usize) -> CrossSearchResult {
    CrossSearchResult {
        conversation_id: id.to_string(),
        title: title.to_string(),
        excerpt: excerpt.to_string(),
        timestamp: 1_700_000_000,
        first_match_message_index: msg_idx,
    }
}

// ── AC5: / key opens cross-search overlay ─────────────────────────────────

#[test]
fn test_e2e_slash_in_history_window_opens_cross_search() {
    let mut state = history_state();
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert_eq!(action, InputAction::OpenCrossSearch);
    assert_cross_search_frame(&state);
    assert_eq!(state.cross_search.query.as_str(), "");
}

#[test]
fn test_e2e_slash_outside_history_does_not_open_cross_search() {
    let mut state = TuiState::new(130, 30);
    state.focus = FocusState::Chat;
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert_eq!(state.focus, FocusState::Chat);
    assert!(!is_open(&state, Surface::CrossSearch));
    assert!(state.modal_stack.is_empty());
}

// ── AC5: Typing extends query ──────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_typing_extends_query() {
    let mut state = cross_search_state();
    for c in "deploy".chars() {
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        // Each char emits CrossSearchQueryChanged (event loop runs the scan).
        assert_eq!(action, InputAction::CrossSearchQueryChanged);
    }
    assert_eq!(state.cross_search.query.as_str(), "deploy");
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_backspace_pops_query() {
    let mut state = cross_search_state();
    state.cross_search.query = "deploy".into();
    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
    assert_eq!(action, InputAction::CrossSearchQueryChanged);
    assert_eq!(state.cross_search.query.as_str(), "deplo");
    assert_cross_search_frame(&state);
}

#[test]
fn cross_query_middle_insert_and_paste_use_the_real_local_front_door() {
    let mut state = cross_search_state();
    for ch in "ab".chars() {
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::KeyPress(ch)),
            InputAction::CrossSearchQueryChanged
        );
    }
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Left)),
        InputAction::Consumed
    );
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('界')),
        InputAction::CrossSearchQueryChanged
    );
    assert_eq!(state.cross_search.query.as_str(), "a界b");
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::Paste("e\u{301}👩‍💻".into())),
        InputAction::CrossSearchQueryChanged
    );
    assert_eq!(state.cross_search.query.as_str(), "a界e\u{301}👩‍💻b");
    for expected in ["a界e\u{301}b", "a界b", "ab"] {
        assert_eq!(
            handle_input(
                &mut state,
                &DomainInputEvent::SpecialKey(DomainKey::Backspace)
            ),
            InputAction::CrossSearchQueryChanged
        );
        assert_eq!(state.cross_search.query.as_str(), expected);
    }
    assert_cross_search_frame(&state);
    assert!(state.input_buffer.is_empty());
}

#[test]
fn cross_query_single_multibyte_scalar_keeps_the_byte_painter_threshold() {
    let mut state = cross_search_state();
    assert_eq!(
        handle_input(&mut state, &DomainInputEvent::KeyPress('界')),
        InputAction::CrossSearchQueryChanged
    );
    assert_eq!(
        rustain::adapters::tui::handlers::export::apply_cross_search_query_change(&mut state),
        rustain::adapters::tui::handlers::export::CrossSearchScanAction::Cleared
    );
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state.cross_search, &state.theme, true);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    assert!(text.contains("No matches found"), "{text}");
    assert!(!text.contains("Type at least 2 characters"), "{text}");
}

// ── AC5: j/k navigate results ──────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_j_advances_selection_with_results() {
    let mut state = cross_search_state();
    state.cross_search.results = vec![
        stub_result("a", "alpha", "one", 0),
        stub_result("b", "beta", "two", 0),
        stub_result("c", "gamma", "three", 0),
    ];
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.cross_search.selected, 1);
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.cross_search.selected, 2);
    // Clamped at last.
    handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(state.cross_search.selected, 2);
    handle_input(&mut state, &DomainInputEvent::KeyPress('k'));
    assert_eq!(state.cross_search.selected, 1);
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_arrow_key_navigation() {
    let mut state = cross_search_state();
    state.cross_search.results = vec![
        stub_result("a", "alpha", "one", 0),
        stub_result("b", "beta", "two", 0),
    ];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.cross_search.selected, 1);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.cross_search.selected, 0);
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_j_with_no_results_is_noop() {
    // Typing 'j' into an empty-results overlay would otherwise extend the
    // query, but our dispatch treats 'j' as a navigation key in CrossSearch
    // overlay — it's consumed, query stays empty.
    let mut state = cross_search_state();
    assert!(state.cross_search.results.is_empty());
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.cross_search.query.as_str(), "");
    assert_cross_search_frame(&state);
}

// ── AC6: Enter opens result ────────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_enter_returns_open_result_when_results_exist() {
    let mut state = cross_search_state();
    state.cross_search.results = vec![stub_result("a", "alpha", "one", 0)];
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::OpenCrossSearchResult);
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_enter_with_no_results_is_consumed() {
    let mut state = cross_search_state();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::Consumed);
    assert_cross_search_frame(&state);
}

// ── AC5: Esc closes ────────────────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_esc_returns_close() {
    let mut state = cross_search_state();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::CloseCrossSearch);
    assert_cross_search_frame(&state);
    rustain::adapters::tui::handlers::export::close_cross_search(&mut state);
    assert!(!is_open(&state, Surface::CrossSearch));
    // Focus returns through the stack: the History window it was searched from is on top
    // again (and nothing wrote the base focus).
    assert_eq!(state.modal_stack.len(), 1);
    assert!(history_window::is_top(&state));
    assert_eq!(state.focus, TuiState::new(130, 30).focus);
}

// ── AC5: Overlay widget render ─────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_widget_renders_query_prompt_when_short_query() {
    let backend = TestBackend::new(40, 15);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.query = "a".into();
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme, true);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    assert!(text.contains("Cross-Search"));
    assert!(text.contains("Type at least 2 characters"));
}

#[test]
fn test_e2e_cross_search_widget_renders_results_vertical_stack() {
    let backend = TestBackend::new(40, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.query = "hit".into();
    state.total = 2;
    state.scanned = 2;
    state.results = vec![
        stub_result("a", "First conversation", "match hit", 0),
        stub_result("b", "Second conversation", "another hit", 0),
    ];
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme, true);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    assert!(text.contains("First conversation"));
    assert!(text.contains("Second conversation"));
}

#[test]
fn test_e2e_cross_search_widget_shows_no_matches_when_empty_results() {
    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.query = "xyz".into();
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme, true);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    // AC7 spec-mandated wording — must be full phrase "No matches found",
    // not the substring "No matches" (which would also match a regression
    // to the old truncated copy). Party-mode second-audit Fix 9.
    assert!(
        text.contains("No matches found"),
        "Expected spec-mandated 'No matches found', got: {}",
        text
    );
}

// ── AC5: guard helpers (third-audit Fix R5 — tests call production code) ─

#[test]
fn test_e2e_cross_search_below_2_chars_no_scan() {
    // AC5: when the query is < 2 chars, `apply_cross_search_query_change`
    // returns `Cleared` and has already wiped the results in-place. Calling
    // the production helper directly ensures the test cannot drift from the
    // event loop's actual guard (third-audit Fix R5 — was previously a
    // tautological copy of the guard body).
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchScanAction, apply_cross_search_query_change,
    };

    let mut state = cross_search_state();
    // Pre-seed some stale results from a previous scan.
    state.cross_search.results = vec![
        stub_result("a", "alpha", "one", 0),
        stub_result("b", "beta", "two", 0),
    ];
    state.cross_search.truncated_by_count = true;
    state.cross_search.truncated_by_time = true;
    state.cross_search.running = true;

    // User types just one char — query length 1.
    state.cross_search.query = "a".into();
    let action = apply_cross_search_query_change(&mut state);

    assert_eq!(
        action,
        CrossSearchScanAction::Cleared,
        "1-char query must return Cleared, not Spawn"
    );
    assert!(
        state.cross_search.results.is_empty(),
        "1-char query must clear results in-place"
    );
    assert!(!state.cross_search.truncated_by_count);
    assert!(!state.cross_search.truncated_by_time);
    assert!(!state.cross_search.running);
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_query_at_2_chars_spawns_scan() {
    // AC5 positive case: at exactly 2 characters, the helper returns Spawn
    // with the query carried for the async task.
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchScanAction, apply_cross_search_query_change,
    };

    let mut state = cross_search_state();
    state.cross_search.query = "ab".into();
    let action = apply_cross_search_query_change(&mut state);

    match action {
        CrossSearchScanAction::Spawn {
            query,
            frame_id,
            revision,
        } => {
            assert_eq!(query, "ab");
            assert_cross_search_frame(&state);
            assert_eq!(frame_id, state.modal_stack.top().unwrap().id);
            assert_eq!(revision, state.cross_search.request_revision);
            assert!(
                state.cross_search.running,
                "running flag must be set while a scan is in flight"
            );
        }
        CrossSearchScanAction::Cleared => {
            panic!("2-char query must spawn a scan, not clear");
        }
    }
}

#[test]
fn test_e2e_cross_search_stale_result_discard() {
    // AC5 stale-result guard: when a scan in flight for query "foo" returns
    // results AFTER the user has typed "foobar", the event loop must discard
    // the stale result. Verified by calling the production helper
    // `apply_cross_search_results` directly (third-audit Fix R5).
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchResultsOutcome, apply_cross_search_results,
    };

    let mut state = cross_search_state();
    state.cross_search.query = "foo".into();
    let (frame_id, revision, query) = claim_scan(&mut state);
    // The real request remains in flight while the current query changes.
    state.cross_search.query = "foobar".into();

    // Stale scan completes for query "foo".
    let outcome = apply_cross_search_results(
        &mut state,
        frame_id,
        revision,
        query,
        vec![stub_result("stale", "Stale Match", "foo here", 0)],
        false,
        false,
    );

    assert_eq!(
        outcome,
        CrossSearchResultsOutcome::DiscardedStale,
        "stale results must be discarded"
    );
    assert!(
        state.cross_search.results.is_empty(),
        "stale results must not land in state.cross_search.results"
    );
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_fresh_result_applied() {
    // AC5 positive case: when the result's query matches the current query,
    // the helper writes the results into state and returns Applied.
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchResultsOutcome, apply_cross_search_results,
    };

    let mut state = cross_search_state();
    state.cross_search.query = "foobar".into();
    let (frame_id, revision, query) = claim_scan(&mut state);

    let outcome = apply_cross_search_results(
        &mut state,
        frame_id,
        revision,
        query,
        vec![stub_result("fresh", "Fresh Match", "foobar now", 0)],
        false,
        false,
    );

    assert_eq!(outcome, CrossSearchResultsOutcome::Applied);
    assert_eq!(state.cross_search.results.len(), 1);
    assert_eq!(state.cross_search.results[0].conversation_id, "fresh");
    assert!(
        !state.cross_search.running,
        "running flag must clear on apply"
    );
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_applied_results_clamp_selection() {
    // AC5 Dev Notes: when applying results, `selected` must clamp to
    // `results.len() - 1` so a stale out-of-range selection is recovered.
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchResultsOutcome, apply_cross_search_results,
    };

    let mut state = cross_search_state();
    state.cross_search.query = "qq".into();
    let (frame_id, revision, query) = claim_scan(&mut state);
    state.cross_search.selected = 15;

    let outcome = apply_cross_search_results(
        &mut state,
        frame_id,
        revision,
        query,
        vec![
            stub_result("a", "alpha", "q", 0),
            stub_result("b", "beta", "q", 0),
        ],
        false,
        false,
    );
    assert_eq!(outcome, CrossSearchResultsOutcome::Applied);
    assert_eq!(
        state.cross_search.selected, 1,
        "selected must clamp to results.len() - 1"
    );

    // Apply empty results — selected resets to 0.
    let (frame_id, revision, query) = claim_scan(&mut state);
    let outcome =
        apply_cross_search_results(&mut state, frame_id, revision, query, vec![], false, false);
    assert_eq!(outcome, CrossSearchResultsOutcome::Applied);
    assert_eq!(state.cross_search.selected, 0);
    assert_cross_search_frame(&state);
}

#[test]
fn test_e2e_cross_search_widget_shows_truncation_by_count_hint() {
    let backend = TestBackend::new(50, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.query = "hit".into();
    state.truncated_by_count = true;
    state.total = 40;
    state.scanned = 40;
    state.results = vec![stub_result("a", "first", "hit", 0)];
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme, true);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    assert!(text.contains("Showing"));
    assert!(text.contains("most recent"));
}

#[test]
fn test_e2e_cross_search_widget_shows_truncation_by_time_hint() {
    let backend = TestBackend::new(50, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.query = "hit".into();
    state.truncated_by_time = true;
    state.total = 100;
    state.scanned = 40;
    state.results = vec![stub_result("a", "first", "hit", 0)];
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme, true);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    // AC5 (party-mode Fix 9): spec-mandated hint wording uses "200 ms" with
    // a space and specifies "refine query for full coverage".
    assert!(text.contains("Scan stopped after 200 ms"));
    assert!(text.contains("refine query"));
}
