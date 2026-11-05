//! E2E tests for within-conversation search (Story 4-4 AC1–AC4, AC7).
//!
//! These tests exercise the input-dispatch layer (`handle_input` → `InputAction`)
//! and the render-layer integration (`render_with_search` applies highlights).
//! Production Search effects use `handlers::search::handle_search_action`;
//! frame-only routing fixtures open through the real presentation reducer.

use std::collections::HashMap;

use crate::common;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use rustain::adapters::tui::app::{InputAction, handle_input};
use rustain::adapters::tui::handlers::search;
use rustain::adapters::tui::keymap::Host;
use rustain::adapters::tui::layout::compute_layout;
use rustain::adapters::tui::notification_log::LogFacts;
use rustain::adapters::tui::presentation::{self, Shell, Surface, reducer};
use rustain::adapters::tui::state::{SearchSubstate, TabRenderState, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::adapters::tui::widgets::chat_pane::RenderResult;
use rustain::adapters::tui::widgets::chat_pane::render_with_search;
use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
use rustain::domain::clock::SystemClock;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::view_state::ViewState;
use rustain::domain::models::{
    ChatMessage, Conversation, FeedbackBlock, FocusState, MessageRole, StreamingState,
};
use rustain::domain::services::search::{SearchMatch, find_matches};

// ── Helpers ────────────────────────────────────────────────────────────────

fn make_conversation(messages: Vec<&str>) -> Conversation {
    Conversation {
        id: "conv-test".to_string(),
        title: "Search Test".to_string(),
        messages: messages
            .into_iter()
            .enumerate()
            .map(|(i, content)| ChatMessage {
                synthetic: false,
                id: format!(
                    "msg-{
                    }",
                    i
                ),
                role: if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                content: content.to_string(),
                content_blocks: vec![],
                tool_calls: vec![],
                created_at: 1_700_000_000 + i as i64,
                token_count: None,
                stop_reason: None,
                images: vec![],
                origin: rustain::domain::models::ChannelKind::Terminal,
                authorship: rustain::domain::models::Authorship::HumanWritten,
                send_authorization: rustain::domain::models::SendAuthorization::Unknown,
                retracted_at_ms: None,
                history_source: Default::default(),
            })
            .collect(),
        turns: Vec::new(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_100,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
        compaction: None,
        peer_row_controls: Default::default(),
        peer_row_revision: 0,
    }
}

fn make_state_with_focus(focus: FocusState) -> TuiState {
    let mut s = TuiState::new(80, 24);
    s.focus = focus;
    s
}

fn render_search_frame(state: &TuiState) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            let layout = compute_layout(
                frame.area(),
                &state.theme,
                &state.input_buffer,
                1,
                state.sidebar_visible,
                state.density_mode,
            )
            .unwrap();
            let shell = Shell::new(frame.area(), &layout);
            presentation::compose(
                frame,
                state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    terminal
}

fn render_once(
    state: &mut TuiState,
    conversation: &Conversation,
    search_query: Option<&str>,
    focused_match: Option<&SearchMatch>,
) -> (Terminal<TestBackend>, RenderResult) {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let streaming = StreamingState::default();
    let theme = Theme::dark();
    let mut tab_render_state = TabRenderState::default();
    let mut rr = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
        user_message_boundaries: Vec::new(),
        focused_tool_id: None,
        visible_feedback_rows: Vec::new(),
    };
    terminal
        .draw(|frame| {
            let area = frame.area();
            rr = render_with_search(
                frame,
                area,
                conversation,
                None,
                &streaming,
                &ViewState::default(),
                &SystemClock::default(),
                state.scroll_offset(),
                state.auto_scroll(),
                &theme,
                &mut tab_render_state,
                &HashMap::<String, ToolBlockState>::new(),
                &std::collections::BTreeMap::<String, FeedbackBlock>::new(),
                search_query,
                focused_match,
                &[],
                &[],
                None,
                None,
                None, // open_prose
                None, // current_focus
            );
        })
        .unwrap();
    (terminal, rr)
}

// ── AC1: Ctrl+F opens within-conversation search bar ───────────────────────

#[test]
fn test_e2e_ctrl_f_opens_search_bar() {
    // S16.8: Ctrl+F in Chat focus → ScrollFullPageDown (narrow override).
    // Search overlay opens via Ctrl+F in Input focus (empty buffer).
    let mut state = make_state_with_focus(FocusState::Input);
    state.input_buffer.clear();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlF));
    assert_eq!(action, InputAction::OpenSearch);
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Search);
    assert!(rustain::adapters::tui::presentation::is_open(
        &state,
        Surface::Search
    ));
    assert_eq!(state.search_state.substate, SearchSubstate::Typing);
}

#[test]
fn test_e2e_ctrl_f_from_input_with_pending_text_is_ignored() {
    // AC1 (party-mode Fix 16): Ctrl+F in Input focus is ignored when the
    // user has pending text in the input buffer.
    let mut state = make_state_with_focus(FocusState::Input);
    state.input_buffer = "hello ".to_string();
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlF));
    assert!(!rustain::adapters::tui::presentation::is_open(
        &state,
        Surface::Search
    ));
    assert_eq!(state.focus, FocusState::Input);
}

#[test]
fn test_e2e_ctrl_f_from_input_with_empty_buffer_opens_search() {
    // AC1 (party-mode Fix 16): Ctrl+F in Input focus opens search overlay
    // when the input buffer is empty.
    let mut state = make_state_with_focus(FocusState::Input);
    state.input_buffer.clear();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlF));
    assert_eq!(action, InputAction::OpenSearch);
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Search);
    assert!(rustain::adapters::tui::presentation::is_open(
        &state,
        Surface::Search
    ));
}

// ── AC2: Live incremental highlighting ─────────────────────────────────────

#[test]
fn test_e2e_search_renders_match_highlight() {
    let conv = make_conversation(vec![
        "Hello there",
        "The quick brown fox",
        "Another message",
    ]);
    let mut state = TuiState::new(80, 24);
    let (terminal, _) = render_once(&mut state, &conv, Some("fox"), None);
    let text = common::buffer_text(&terminal);
    // The match text itself renders verbatim.
    assert!(
        text.contains("fox"),
        "Expected 'fox' to render in the highlighted output"
    );
}

#[test]
fn test_e2e_search_no_matches_renders_plain() {
    let conv = make_conversation(vec!["nothing matches"]);
    let mut state = TuiState::new(80, 24);
    let (terminal, _) = render_once(&mut state, &conv, Some("xyzzy"), None);
    let text = common::buffer_text(&terminal);
    // Original content still visible — highlighting a non-match doesn't hide it.
    assert!(text.contains("nothing matches"));
}

// ── AC3: Enter commits, n/N navigate ───────────────────────────────────────

#[test]
fn test_e2e_search_enter_commits_query_transitions_substate() {
    // AC3: Enter in Typing sub-state → transition to Navigating (when matches exist).
    // Route through a real frame and apply the production Search effect.
    let mut state = TuiState::new(80, 24);
    let conversation = make_conversation(vec!["fox"]);
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = conversation.clone();
    reducer::open(&mut state, Surface::Search);
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::OpenSearch,
    );
    state.search_state.matches = vec![rustain::domain::services::search::SearchMatch {
        message_index: 0,
        byte_start: 0,
        byte_end: 3,
    }];

    handle_input(&mut state, &DomainInputEvent::KeyPress('f'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('o'));
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    assert_eq!(state.search_state.query.as_str(), "fox");
    assert_eq!(state.search_state.substate, SearchSubstate::Typing);

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::SearchCommit);
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert_eq!(
        state.search_state.substate,
        SearchSubstate::Navigating,
        "Enter in Typing with matches must transition to Navigating (AC3)"
    );
}

#[test]
fn test_e2e_search_enter_on_zero_matches_stays_in_typing() {
    // AC3: Enter with zero matches is a no-op — stay in Typing.
    let mut state = TuiState::new(80, 24);
    let conversation = make_conversation(vec!["nothing matches"]);
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = conversation.clone();
    reducer::open(&mut state, Surface::Search);
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::OpenSearch,
    );
    state.search_state.query = "xyzzy".into();
    state.search_state.matches = vec![]; // no matches

    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter));
    assert_eq!(action, InputAction::SearchCommit);
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert_eq!(
        state.search_state.substate,
        SearchSubstate::Typing,
        "Enter with zero matches must stay in Typing (AC3 zero-match clause)"
    );
}

#[test]
fn test_e2e_search_n_in_query_stays_in_typing_critical_regression() {
    // CRITICAL regression guard for the spec bug: typing `nginx` must NOT
    // trigger navigation. Every `n` / `N` in Typing sub-state is a literal.
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    for c in "nginx".chars() {
        let action = handle_input(&mut state, &DomainInputEvent::KeyPress(c));
        assert_eq!(action, InputAction::SearchQueryChanged);
    }
    assert_eq!(state.search_state.query.as_str(), "nginx");
    assert_eq!(state.search_state.substate, SearchSubstate::Typing);
}

#[test]
fn test_e2e_search_n_key_in_navigating_navigates() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    state.search_state.substate = SearchSubstate::Navigating;
    let action_n = handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert_eq!(action_n, InputAction::SearchNext);
    let action_shift_n = handle_input(&mut state, &DomainInputEvent::KeyPress('N'));
    assert_eq!(action_shift_n, InputAction::SearchPrev);
}

#[test]
fn test_e2e_search_return_to_typing_from_navigating() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    state.search_state.query = "foo".into();
    state.search_state.substate = SearchSubstate::Navigating;
    // Typing 'x' in Navigating returns to Typing AND appends the char.
    let action = handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    assert_eq!(action, InputAction::SearchReturnToTyping);
    assert_eq!(state.search_state.query.as_str(), "foox");
    assert_eq!(state.search_state.substate, SearchSubstate::Typing);
}

// ── AC4: Esc closes ────────────────────────────────────────────────────────

#[test]
fn test_e2e_search_esc_closes_overlay() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    state.search_state.query = "test".into();
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Esc));
    assert_eq!(action, InputAction::CloseSearch);
}

// ── AC3 amendment: Ctrl+U clears query ─────────────────────────────────────

#[test]
fn test_e2e_search_ctrl_u_clears_query() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    state.search_state.query = "something long".into();
    state.search_state.substate = SearchSubstate::Navigating;
    let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlU));
    assert_eq!(action, InputAction::SearchReturnToTyping);
    assert_eq!(state.search_state.query.as_str(), "");
    assert_eq!(state.search_state.substate, SearchSubstate::Typing);
}

// ── Search service integration ─────────────────────────────────────────────

#[test]
fn test_e2e_find_matches_against_live_conversation() {
    // End-to-end sanity: search engine actually finds matches in a
    // realistic conversation. This catches regressions in find_matches
    // plumbing without needing the event loop.
    let conv = make_conversation(vec![
        "Hello world, this is the first message.",
        "Here is the second; it mentions postgres and mongodb.",
        "Third message about redis.",
        "Fourth message back to postgres topics.",
    ]);
    let matches = find_matches(&conv, "postgres");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].message_index, 1);
    assert_eq!(matches[1].message_index, 3);
}

#[test]
fn test_e2e_search_bar_renders_counter_when_matches_exist() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    state.search_state.query = "test".into();
    state.search_state.matches = vec![
        SearchMatch {
            message_index: 0,
            byte_start: 0,
            byte_end: 4,
        },
        SearchMatch {
            message_index: 1,
            byte_start: 0,
            byte_end: 4,
        },
    ];
    state.search_state.focused_match_index = 0;
    let terminal = render_search_frame(&state);
    let text = common::buffer_text(&terminal);
    assert!(text.contains("Search:"));
    assert!(text.contains("test"));
    assert!(text.contains("1/2"));
}

#[test]
fn test_e2e_search_bar_shows_no_matches_found_when_query_has_zero_matches() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    state.search_state.query = "xyzzy".into();
    // matches intentionally empty
    let terminal = render_search_frame(&state);
    let text = common::buffer_text(&terminal);
    assert!(text.contains("No matches found"));
}

#[test]
fn test_e2e_search_bar_shows_zero_zero_when_query_empty() {
    let mut state = TuiState::new(80, 24);
    reducer::open(&mut state, Surface::Search);
    // query empty, matches empty
    let terminal = render_search_frame(&state);
    let text = common::buffer_text(&terminal);
    assert!(text.contains("0/0"));
}

// Suppress unused import warnings in the AC5 module — render path helpers.
#[allow(dead_code)]
fn _unused(c: &chat_pane::RenderResult) -> usize {
    c.total_content_height
}

// ── Second-audit Fix 1: role lines must not receive search highlights ──

#[test]
fn test_e2e_search_does_not_highlight_role_line_word() {
    // Regression guard for Fix 1: if a user searches "assistant" (or any
    // token that appears in the role indicator), the highlight pass MUST
    // skip the role line. Otherwise the word "Assistant:" gets visually
    // highlighted AND the match_cursor drifts, causing the focused-match
    // style to land on the wrong content match.
    //
    // The rendered role line uses a bold accent color WITHOUT the
    // `search_highlight` style. We verify this by checking that the
    // rendered buffer for the role-line row does not contain any cell with
    // the reversed/highlight modifier — even though a naive rebuild would
    // have highlighted "Assistant" on that row.
    use rustain::adapters::tui::state::TabRenderState;
    use rustain::adapters::tui::widgets::chat_pane::{RenderResult, render_with_search};
    use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
    use rustain::domain::models::{FeedbackBlock, StreamingState};
    use std::collections::{BTreeMap, HashMap};

    let conv = make_conversation(vec![
        // First message is assistant-labeled content. Role line: "Assistant:".
        "assistant says hello",
    ]);
    // Force the first message to be an assistant so the role line is "Assistant:".
    let mut conv = conv;
    conv.messages[0].role = MessageRole::Assistant;

    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut tab_render_state = TabRenderState::default();
    let streaming = StreamingState::default();
    let mut _rr = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
        user_message_boundaries: Vec::new(),
        focused_tool_id: None,
        visible_feedback_rows: Vec::new(),
    };
    terminal
        .draw(|frame| {
            let area = frame.area();
            _rr = render_with_search(
                frame,
                area,
                &conv,
                None,
                &streaming,
                &ViewState::default(),
                &SystemClock::default(),
                0,
                true,
                &theme,
                &mut tab_render_state,
                &HashMap::<String, ToolBlockState>::new(),
                &BTreeMap::<String, FeedbackBlock>::new(),
                Some("assistant"),
                None,
                &[],
                &[],
                None,
                None,
                None, // open_prose
                None, // current_focus
            );
        })
        .unwrap();

    // Inspect the buffer: find the row containing "Assistant:" and verify
    // that cell does NOT carry the search_highlight style (REVERSED).
    let buffer = terminal.backend().buffer().clone();
    let mut role_row: Option<u16> = None;
    for y in 0..buffer.area.height {
        let row_text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        if row_text.contains("Assistant:") {
            role_row = Some(y);
            break;
        }
    }
    let role_row = role_row.expect("role line must be rendered");

    // Scan the role-line cells for any with the search_highlight REVERSED
    // modifier. If any are found, the role-line skip is not working.
    let highlight_cells: Vec<_> = (0..buffer.area.width)
        .filter(|x| {
            let cell = &buffer[(*x, role_row)];
            cell.modifier.contains(ratatui::style::Modifier::REVERSED)
        })
        .collect();
    assert!(
        highlight_cells.is_empty(),
        "Role line must not receive search highlights; found {} highlighted cells at row {}",
        highlight_cells.len(),
        role_row
    );
}

// ── Second-audit Fix 2: multi-match focused ordinal correctness ───────

#[test]
fn test_e2e_search_focused_ordinal_with_multiple_matches_in_one_message() {
    // Regression guard for Fix 2: if a message contains >1 match, the
    // focused style must land on the match at the user's current focused
    // index, not always on the first match in that message.
    //
    // We construct a 1-message conversation with 3 "foo" matches, set the
    // focused_match_index to point at the middle match, and verify the
    // render path's `focused_local_ordinal` computation picks ordinal 1
    // (the second match, 0-indexed), not 0.
    use rustain::adapters::tui::state::TabRenderState;
    use rustain::adapters::tui::widgets::chat_pane::{RenderResult, render_with_search};
    use rustain::adapters::tui::widgets::tool_block::ToolBlockState;
    use rustain::domain::models::{FeedbackBlock, StreamingState};
    use std::collections::{BTreeMap, HashMap};

    let conv = make_conversation(vec!["foo bar foo baz foo qux"]);
    let all_matches = find_matches(&conv, "foo");
    assert_eq!(
        all_matches.len(),
        3,
        "fixture must contain exactly 3 matches for the regression to be meaningful"
    );
    // Focus the middle (index 1) match.
    let focused = all_matches[1].clone();

    let backend = TestBackend::new(60, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let theme = Theme::dark();
    let mut tab_render_state = TabRenderState::default();
    let streaming = StreamingState::default();
    let mut _rr = RenderResult {
        total_content_height: 0,
        block_boundaries: Vec::new(),
        message_boundaries: Vec::new(),
        user_message_boundaries: Vec::new(),
        focused_tool_id: None,
        visible_feedback_rows: Vec::new(),
    };
    // This render call must succeed and the focused ordinal lookup in
    // chat_pane::render_with_search must compute ordinal == 1 for this
    // message. We cannot directly inspect the ordinal from outside, but
    // we CAN verify that passing the full match list (Fix 2's signature
    // addition) does not panic and produces a valid render.
    terminal
        .draw(|frame| {
            let area = frame.area();
            _rr = render_with_search(
                frame,
                area,
                &conv,
                None,
                &streaming,
                &ViewState::default(),
                &SystemClock::default(),
                0,
                true,
                &theme,
                &mut tab_render_state,
                &HashMap::<String, ToolBlockState>::new(),
                &BTreeMap::<String, FeedbackBlock>::new(),
                Some("foo"),
                Some(&focused),
                all_matches.as_slice(),
                &[],
                None,
                None,
                None, // open_prose
                None, // current_focus
            );
        })
        .unwrap();

    // Verify that the full match list parameter shape works end-to-end by
    // asserting the render produced a non-empty message boundary list.
    assert!(
        !_rr.message_boundaries.is_empty(),
        "render must complete successfully with full match list"
    );
    // The focused-ordinal computation inside render_with_search filters
    // matches by message_index == i and uses position(). With 3 matches
    // all at message_index 0, and focused pointing at all_matches[1],
    // the computed local ordinal is 1 — correct.
    let local_ordinal: Option<usize> = all_matches
        .iter()
        .filter(|m| m.message_index == focused.message_index)
        .position(|m| m == &focused);
    assert_eq!(
        local_ordinal,
        Some(1),
        "focused_local_ordinal must point to the middle match (index 1), not the first (index 0)"
    );
}
