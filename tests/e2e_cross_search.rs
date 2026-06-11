//! E2E tests for cross-conversation search (Story 4-4 AC5, AC6, AC7).
//!
//! Exercises the input-dispatch layer (/ key in sidebar, j/k navigation,
//! Enter, Esc, Backspace) and the widget render path. The actual async
//! scan (`run_cross_search`) is covered by unit tests in
//! `domain/services/cross_search.rs`.

mod common;

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::state::{CrossSearchState, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::cross_search;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::{OverlayType, PanelType};
use rustain::domain::services::cross_search::CrossSearchResult;

// ── Helpers ────────────────────────────────────────────────────────────────

fn sidebar_focus_state() -> TuiState {
    let mut s = TuiState::new(120, 30);
    s.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };
    s
}

fn cross_search_overlay_state() -> TuiState {
    let mut s = TuiState::new(120, 30);
    s.focus = FocusState::Overlay(OverlayType::CrossSearch);
    s.cross_search = CrossSearchState::new();
    s.cross_search.active = true;
    s
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
fn test_e2e_slash_in_sidebar_opens_cross_search() {
    let mut state = sidebar_focus_state();
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert_eq!(action, InputAction::OpenCrossSearch);
    assert_eq!(state.focus, FocusState::Overlay(OverlayType::CrossSearch));
    assert!(state.cross_search.active);
    assert_eq!(state.cross_search.query, "");
}

#[test]
fn test_e2e_slash_outside_sidebar_does_not_open_cross_search() {
    let mut state = TuiState::new(120, 30);
    state.focus = FocusState::Chat;
    handle_input(&mut state, &DomainInputEvent::KeyPress('/'));
    assert_ne!(state.focus, FocusState::Overlay(OverlayType::CrossSearch));
    assert!(!state.cross_search.active);
}

// ── AC5: Typing extends query ──────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_typing_extends_query() {
    let mut state = cross_search_overlay_state();
    for c in "deploy".chars() {
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        // Each char emits CrossSearchQueryChanged (event loop runs the scan).
        assert_eq!(action, InputAction::CrossSearchQueryChanged);
    }
    assert_eq!(state.cross_search.query, "deploy");
}

#[test]
fn test_e2e_cross_search_backspace_pops_query() {
    let mut state = cross_search_overlay_state();
    state.cross_search.query = "deploy".to_string();
    let action = handle_input(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Backspace),
    );
    assert_eq!(action, InputAction::CrossSearchQueryChanged);
    assert_eq!(state.cross_search.query, "deplo");
}

// ── AC5: j/k navigate results ──────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_j_advances_selection_with_results() {
    let mut state = cross_search_overlay_state();
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
}

#[test]
fn test_e2e_cross_search_arrow_key_navigation() {
    let mut state = cross_search_overlay_state();
    state.cross_search.results = vec![
        stub_result("a", "alpha", "one", 0),
        stub_result("b", "beta", "two", 0),
    ];
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Down));
    assert_eq!(state.cross_search.selected, 1);
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Up));
    assert_eq!(state.cross_search.selected, 0);
}

#[test]
fn test_e2e_cross_search_j_with_no_results_is_noop() {
    // Typing 'j' into an empty-results overlay would otherwise extend the
    // query, but our dispatch treats 'j' as a navigation key in CrossSearch
    // overlay — it's consumed, query stays empty.
    let mut state = cross_search_overlay_state();
    assert!(state.cross_search.results.is_empty());
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('j'));
    assert_eq!(action, InputAction::Consumed);
    assert_eq!(state.cross_search.query, "");
}

// ── AC6: Enter opens result ────────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_enter_returns_open_result_when_results_exist() {
    let mut state = cross_search_overlay_state();
    state.cross_search.results = vec![stub_result("a", "alpha", "one", 0)];
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::OpenCrossSearchResult);
}

#[test]
fn test_e2e_cross_search_enter_with_no_results_is_consumed() {
    let mut state = cross_search_overlay_state();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::Consumed);
}

// ── AC5: Esc closes ────────────────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_esc_returns_close() {
    let mut state = cross_search_overlay_state();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::CloseCrossSearch);
}

// ── AC5: Overlay widget render ─────────────────────────────────────────────

#[test]
fn test_e2e_cross_search_widget_renders_query_prompt_when_short_query() {
    let backend = TestBackend::new(40, 15);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.active = true;
    state.query = "a".to_string();
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme);
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
    state.active = true;
    state.query = "hit".to_string();
    state.total = 2;
    state.scanned = 2;
    state.results = vec![
        stub_result("a", "First conversation", "match hit", 0),
        stub_result("b", "Second conversation", "another hit", 0),
    ];
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme);
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
    state.active = true;
    state.query = "xyz".to_string();
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme);
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

    let mut state = cross_search_overlay_state();
    // Pre-seed some stale results from a previous scan.
    state.cross_search.results = vec![
        stub_result("a", "alpha", "one", 0),
        stub_result("b", "beta", "two", 0),
    ];
    state.cross_search.truncated_by_count = true;
    state.cross_search.truncated_by_time = true;
    state.cross_search.running = true;

    // User types just one char — query length 1.
    state.cross_search.query = "a".to_string();
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
}

#[test]
fn test_e2e_cross_search_query_at_2_chars_spawns_scan() {
    // AC5 positive case: at exactly 2 characters, the helper returns Spawn
    // with the query carried for the async task.
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchScanAction, apply_cross_search_query_change,
    };

    let mut state = cross_search_overlay_state();
    state.cross_search.query = "ab".to_string();
    let action = apply_cross_search_query_change(&mut state);

    match action {
        CrossSearchScanAction::Spawn { query } => {
            assert_eq!(query, "ab");
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

    let mut state = cross_search_overlay_state();
    // User's current query is "foobar".
    state.cross_search.query = "foobar".to_string();

    // Stale scan completes for query "foo".
    let outcome = apply_cross_search_results(
        &mut state,
        "foo".to_string(),
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
}

#[test]
fn test_e2e_cross_search_fresh_result_applied() {
    // AC5 positive case: when the result's query matches the current query,
    // the helper writes the results into state and returns Applied.
    use rustain::infrastructure::runtime::event_loop::{
        CrossSearchResultsOutcome, apply_cross_search_results,
    };

    let mut state = cross_search_overlay_state();
    state.cross_search.query = "foobar".to_string();
    state.cross_search.running = true;

    let outcome = apply_cross_search_results(
        &mut state,
        "foobar".to_string(),
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
}

#[test]
fn test_e2e_cross_search_applied_results_clamp_selection() {
    // AC5 Dev Notes: when applying results, `selected` must clamp to
    // `results.len() - 1` so a stale out-of-range selection is recovered.
    use rustain::infrastructure::runtime::event_loop::apply_cross_search_results;

    let mut state = cross_search_overlay_state();
    state.cross_search.query = "q".to_string();
    state.cross_search.selected = 15;

    apply_cross_search_results(
        &mut state,
        "q".to_string(),
        vec![
            stub_result("a", "alpha", "q", 0),
            stub_result("b", "beta", "q", 0),
        ],
        false,
        false,
    );
    assert_eq!(
        state.cross_search.selected, 1,
        "selected must clamp to results.len() - 1"
    );

    // Apply empty results — selected resets to 0.
    apply_cross_search_results(&mut state, "q".to_string(), vec![], false, false);
    assert_eq!(state.cross_search.selected, 0);
}

#[test]
fn test_e2e_cross_search_widget_shows_truncation_by_count_hint() {
    let backend = TestBackend::new(50, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut state = CrossSearchState::new();
    state.active = true;
    state.query = "hit".to_string();
    state.truncated_by_count = true;
    state.total = 40;
    state.scanned = 40;
    state.results = vec![stub_result("a", "first", "hit", 0)];
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme);
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
    state.active = true;
    state.query = "hit".to_string();
    state.truncated_by_time = true;
    state.total = 100;
    state.scanned = 40;
    state.results = vec![stub_result("a", "first", "hit", 0)];
    terminal
        .draw(|frame| {
            cross_search::render(frame, frame.area(), &state, &theme);
        })
        .unwrap();
    let text = common::buffer_text(&terminal);
    // AC5 (party-mode Fix 9): spec-mandated hint wording uses "200 ms" with
    // a space and specifies "refine query for full coverage".
    assert!(text.contains("Scan stopped after 200 ms"));
    assert!(text.contains("refine query"));
}
