//! Stage replacement uses the production load seam, never TUI-only view mirrors.
use super::keymap_support::press;
use rustain::adapters::tui::{
    app::InputAction,
    handlers::search,
    keymap::Key,
    presentation::{self, Surface},
    state::TuiState,
};
use rustain::domain::{
    models::{
        AnchorMode, StreamingState,
        session::{SessionManager, SessionState},
        tab::TabManager,
    },
    services::turn_queue::TurnQueue,
};
use rustain::infrastructure::runtime::event_loop::load_active_tab;
use tokio_util::sync::CancellationToken;

#[test]
fn tab_switch_pops_stage_bound_frames_only() {
    let mut tabs = TabManager::new(CancellationToken::new());
    tabs.create_tab();
    tabs.active_tab_mut().view_state.scroll_offset = 7;
    tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
    tabs.switch_to_index(1);
    tabs.active_tab_mut().view_state.scroll_offset = 41;
    tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
    let mut state = TuiState::new(80, 24);
    state.active_tab_id = tabs.active_tab_id();
    let mut conversation = tabs.active_tab().conversation.clone();
    let action = press(&mut state, Key::Ctrl('f'));
    assert_eq!(action, InputAction::OpenSearch);
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert!(
        presentation::is_open(&state, Surface::Search),
        "live Search before replacement"
    );
    assert_eq!(
        state
            .search_state
            .pre_search_view
            .as_ref()
            .unwrap()
            .scroll_offset,
        41
    );
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('?'));
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('n'));
    let globals: Vec<_> = state
        .modal_stack
        .iter()
        .filter(|f| f.surface != Surface::Search)
        .cloned()
        .collect();
    assert_eq!(
        globals.len(),
        2,
        "Help and S-LOG must actually be reachable over Search"
    );
    assert!(globals.iter().any(|f| f.surface == Surface::Help));
    assert!(
        globals
            .iter()
            .any(|f| f.surface == Surface::NotificationLog)
    );
    tabs.switch_to_index(2);
    let destination = format!("{:?}", tabs.active_tab().view_state);
    let mut streaming = StreamingState::default();
    let mut session = SessionManager::new(SessionState::Empty);
    let mut queue = TurnQueue::default();
    load_active_tab(
        &tabs,
        &mut conversation,
        &mut streaming,
        &mut session,
        &mut state,
        &mut queue,
    );
    assert!(
        !presentation::is_open(&state, Surface::Search),
        "real tab loading discards the old Stage's Search"
    );
    assert_eq!(
        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
        globals
    );
    assert!(state.search_state.pre_search_view.is_none());
    assert_eq!(state.active_tab_id, tabs.active_tab_id());
    assert_eq!(state.scroll_offset(), 7);
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::CloseSearch,
    );
    assert_eq!(
        format!("{:?}", tabs.active_tab().view_state),
        destination,
        "discard cannot restore the old Stage into the destination"
    );
    press(&mut state, Key::Esc); // Leave retained S-LOG and Help before reopening Search.
    press(&mut state, Key::Esc);
    let action = press(&mut state, Key::Ctrl('f'));
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert_eq!(
        state.scroll_offset(),
        7,
        "fresh Search uses destination authority"
    );
    assert_eq!(
        format!("{:?}", state.search_state.pre_search_view.as_ref().unwrap()),
        destination
    );
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::CloseSearch,
    );
    assert_eq!(format!("{:?}", tabs.active_tab().view_state), destination);
}

#[test]
fn reset_and_direct_cross_search_replacement_discard_instead_of_backing() {
    use rustain::domain::models::Conversation;
    use rustain::infrastructure::runtime::event_loop::replace_stage_conversation;
    for reason in ["/new", "/clear", "CrossSearch direct new-tab"] {
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().view_state.scroll_offset = 41;
        tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
        let mut conversation = tabs.active_tab().conversation.clone();
        let mut state = TuiState::new(80, 24);
        let action = press(&mut state, Key::Ctrl('f'));
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert!(
            presentation::is_open(&state, Surface::Search),
            "{reason}: live control"
        );
        press(&mut state, Key::Ctrl('x'));
        press(&mut state, Key::Char('?'));
        let global = state.modal_stack.top().unwrap().clone();
        let base = state.focus.clone();
        let replacement = Conversation {
            id: reason.into(),
            ..Conversation::default()
        };
        // This is the extracted production assignment called by BOTH reset
        // commands and the direct CrossSearch branch, not a state-only test shim.
        replace_stage_conversation(&mut conversation, &mut state, replacement);
        assert_eq!(conversation.id, reason);
        assert!(!presentation::is_open(&state, Surface::Search), "{reason}");
        assert_eq!(
            state.modal_stack.top(),
            Some(&global),
            "{reason}: exact global identity"
        );
        assert_eq!(state.focus, base);
        assert!(state.search_state.pre_search_view.is_none());
        assert_eq!(
            tabs.active_tab().view_state.scroll_offset,
            41,
            "discard never restores a view"
        );
    }
}

#[test]
fn fork_and_rewind_post_storage_loads_discard_even_when_the_id_is_unchanged() {
    use rustain::domain::models::Conversation;
    for fork in [true, false] {
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().view_state = old_stage_view();
        let mut conversation = tabs.active_tab().conversation.clone();
        let original_id = conversation.id.clone();
        let mut state = TuiState::new(80, 24);
        let action = press(&mut state, Key::Ctrl('f'));
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert!(presentation::is_open(&state, Surface::Search));
        press(&mut state, Key::Ctrl('x'));
        press(&mut state, Key::Char('n'));
        let global = state.modal_stack.top().unwrap().clone();
        // The exact post-storage operations from ForkConfirm/RewindConfirm:
        // fork creates a tab; rewind replaces the current tab in place.
        if fork {
            tabs.create_tab_with_conversation(Conversation {
                id: "fork-result".into(),
                ..Conversation::default()
            });
        } else {
            tabs.active_tab_mut().conversation = Conversation {
                id: original_id.clone(),
                ..Conversation::default()
            };
        }
        let destination = format!("{:?}", tabs.active_tab().view_state);
        let mut streaming = StreamingState::default();
        let mut session = SessionManager::new(SessionState::Empty);
        let mut queue = TurnQueue::default();
        load_active_tab(
            &tabs,
            &mut conversation,
            &mut streaming,
            &mut session,
            &mut state,
            &mut queue,
        );
        assert!(!presentation::is_open(&state, Surface::Search));
        assert_eq!(state.modal_stack.top(), Some(&global));
        assert_eq!(conversation.id == original_id, !fork);
        assert!(state.search_state.pre_search_view.is_none());
        assert_eq!(
            state.scroll_offset(),
            tabs.active_tab().view_state.scroll_offset
        );
        assert_eq!(format!("{:?}", tabs.active_tab().view_state), destination);
        assert_eq!(
            state.auto_scroll(),
            matches!(tabs.active_tab().view_state.mode, AnchorMode::Following)
        );
        press(&mut state, Key::Esc); // Back from retained global, not the discarded Search.
        let action = press(&mut state, Key::Ctrl('f'));
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert_eq!(
            format!("{:?}", state.search_state.pre_search_view.as_ref().unwrap()),
            destination,
            "fork initializes a fresh view; rewind preserves its owning view"
        );
        search::handle_search_action(
            &mut tabs,
            &mut state,
            &conversation,
            &InputAction::CloseSearch,
        );
        assert_eq!(format!("{:?}", tabs.active_tab().view_state), destination);
    }
}

#[test]
fn a_suspended_search_is_discarded_without_losing_global_continuations() {
    use rustain::adapters::tui::{
        presentation::reducer,
        quit_confirm::{self, QuitFacts, QuitHost},
    };
    let mut tabs = TabManager::new(CancellationToken::new());
    let conversation = tabs.active_tab().conversation.clone();
    let mut state = TuiState::new(80, 24);
    for global in [Surface::Help, Surface::NotificationLog, Surface::UsagePanel] {
        reducer::open(&mut state, global);
    }
    reducer::open(&mut state, Surface::Search);
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::OpenSearch,
    );
    assert_eq!(state.modal_stack.len(), 4);
    assert!(presentation::is_open(&state, Surface::Search));
    assert_eq!(press(&mut state, Key::Ctrl('q')), InputAction::Quit);
    quit_confirm::request(
        &mut state,
        QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Local)
        },
    );
    assert_eq!(state.modal_stack.len(), 4);
    assert!(
        !state
            .modal_stack
            .iter()
            .any(|f| f.surface == Surface::Search)
    );
    assert!(
        presentation::is_open(&state, Surface::Search),
        "Search is a real suspended continuation"
    );
    let globals: Vec<_> = state.modal_stack.iter().cloned().collect();
    reducer::reconcile_stage(&mut state);
    assert!(!presentation::is_open(&state, Surface::Search));
    assert_eq!(
        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
        globals
    );
    press(&mut state, Key::Char('n'));
    assert!(
        !presentation::is_open(&state, Surface::Search),
        "Quit Back cannot resurrect a discarded Stage"
    );
    assert_eq!(state.modal_stack.len(), 3);
    assert!(presentation::consistent(&state));
}

#[test]
fn attached_registry_offers_neither_stage_search_nor_the_history_popup() {
    use rustain::adapters::tui::keymap::{
        self, CommandId, Context, Host, Resolved, table_input::InputCmd,
    };
    let opens = |command| {
        matches!(
            command,
            CommandId::Input(InputCmd::OpenSearch | InputCmd::ReverseSearch)
        )
    };
    let rows: Vec<_> = keymap::table()
        .iter()
        .filter(|row| opens(row.command))
        .collect();
    assert_eq!(rows.len(), 2, "independent local entry-point inventory");
    for row in rows {
        assert!(row.hosts.includes(Host::Local));
        assert!(!row.hosts.includes(Host::Attached));
        assert!(
            matches!(keymap::resolve(Context::Input, Host::Local, row.keys), Resolved::Hit(hit) if opens(hit.command))
        );
        assert!(
            !matches!(keymap::resolve(Context::Input, Host::Attached, row.keys), Resolved::Hit(hit) if opens(hit.command))
        );
    }
}

#[test]
fn a_reset_can_search_again_before_the_switch_time_conversation_mirror_is_saved() {
    use rustain::domain::models::Conversation;
    use rustain::infrastructure::runtime::event_loop::replace_stage_conversation;
    let mut tabs = TabManager::new(CancellationToken::new());
    let mut conversation = tabs.active_tab().conversation.clone();
    let mut state = TuiState::new(80, 24);
    replace_stage_conversation(
        &mut conversation,
        &mut state,
        Conversation {
            id: "current-reset-stage".into(),
            ..Conversation::default()
        },
    );
    assert_ne!(
        tabs.active_tab().conversation.id,
        conversation.id,
        "the runtime saves its active conversation proxy on switching, not every event"
    );
    let action = press(&mut state, Key::Ctrl('f'));
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    let frame = state.modal_stack.top().unwrap().clone();
    assert_eq!(
        frame.conversation_id.as_deref(),
        Some(conversation.id.as_str())
    );
    let action = press(&mut state, Key::Char('z'));
    assert_eq!(action, InputAction::SearchQueryChanged);
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert_eq!(
        state.modal_stack.top(),
        Some(&frame),
        "the current Stage's Search must not close against a stale switch-time model mirror"
    );
    assert_eq!(state.search_state.query, "z");
}

#[test]
fn mismatched_proxy_or_tab_discards_without_restoring_into_the_destination() {
    use rustain::domain::models::Conversation;
    for another_tab in [false, true] {
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().view_state.scroll_offset = 41;
        let old = tabs.active_tab().conversation.clone();
        let mut state = TuiState::new(80, 24);
        let action = press(&mut state, Key::Ctrl('f'));
        search::handle_search_action(&mut tabs, &mut state, &old, &action);
        assert!(presentation::is_open(&state, Surface::Search));
        let current = if another_tab {
            // Same conversation id in a distinct tab exercises tab identity
            // independently of the current-proxy conversation identity check.
            tabs.create_tab_with_conversation(old.clone());
            old
        } else {
            Conversation {
                id: "replacement-proxy".into(),
                ..Conversation::default()
            }
        };
        tabs.active_tab_mut().view_state.scroll_offset = 7;
        let destination = format!("{:?}", tabs.active_tab().view_state);
        search::handle_search_action(&mut tabs, &mut state, &current, &InputAction::CloseSearch);
        assert!(!presentation::is_open(&state, Surface::Search));
        assert!(state.search_state.pre_search_view.is_none());
        assert_eq!(format!("{:?}", tabs.active_tab().view_state), destination);
    }
}

#[test]
fn replacement_base_advice_preserves_a_retained_global_caller() {
    use rustain::adapters::tui::presentation::reducer;
    use rustain::domain::models::FocusState;
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    reducer::open(&mut state, Surface::Help);
    let global = state.modal_stack.top().unwrap().clone();
    // The production NewTab/CloseTab/carryover-completion post-load seam.
    reducer::settle_stage_base(&mut state, FocusState::Input);
    assert_eq!(state.focus, FocusState::Chat);
    assert_eq!(state.modal_stack.top(), Some(&global));
    reducer::back(&mut state);
    let chat_hint = state.current_hint.clone();
    reducer::settle_stage_base(&mut state, FocusState::Input);
    assert_eq!(
        state.focus,
        FocusState::Input,
        "independent empty-stack target control"
    );
    assert!(
        state.current_hint.is_some(),
        "the new base's painter-read cache is settled"
    );
    assert_ne!(
        state.current_hint, chat_hint,
        "independent cache-change control"
    );
}

fn old_stage_view() -> rustain::domain::models::view_state::ViewState {
    use rustain::domain::models::{AnchorRef, SummaryTier, TurnId};
    rustain::domain::models::view_state::ViewState {
        mode: AnchorMode::Pinned(AnchorRef {
            turn_id: TurnId("old-turn".into()),
            line_in_turn: 3,
        }),
        collapsed: [(TurnId("old-turn".into()), true)].into(),
        summary_tier: SummaryTier::Tier2,
        scroll_offset: 41,
        focused_turn: Some(TurnId("old-turn".into())),
        pending_append_lines: 17,
    }
}

// Paint the real active tab, not a default/mirror-owned ViewState. Return the
// exact metrics consumed by the runtime's post-paint resize resolution seam.
fn paint_active_stage(
    tabs: &TabManager,
    state: &mut TuiState,
    conversation: &rustain::domain::models::Conversation,
    width: u16,
) -> ratatui::buffer::Buffer {
    use ratatui::{Terminal, backend::TestBackend};
    use rustain::adapters::tui::widgets::chat_pane::render_with_search;
    let mut terminal = Terminal::new(TestBackend::new(width, 10)).unwrap();
    let tab = tabs.active_tab();
    let theme = state.theme.clone();
    let offset = state.scroll_offset();
    let auto = state.auto_scroll();
    let mut result = None;
    terminal
        .draw(|frame| {
            let area = frame.area();
            result = Some(render_with_search(
                frame,
                area,
                conversation,
                tab.reducer.open_turn.as_ref(),
                &tab.streaming,
                &tab.view_state,
                &*tab.clock,
                offset,
                auto,
                &theme,
                state.tab_render_state(tabs.active_tab_id()),
                &Default::default(),
                &Default::default(),
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                None,
            ));
        })
        .unwrap();
    let result = result.unwrap();
    state.total_content_height = result.total_content_height;
    state.block_boundaries = result.block_boundaries;
    state.message_boundaries = result.message_boundaries;
    state.viewport_height = 10;
    terminal.backend().buffer().clone()
}

#[test]
fn resize_resolution_is_authoritative_before_search_opens_and_backs() {
    use rustain::domain::models::{ChatMessage, MessageRole};
    use rustain::infrastructure::runtime::event_loop::resolve_resize_anchor;
    for valid_anchor in [true, false] {
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().view_state = old_stage_view();
        tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
        let mut conversation = tabs.active_tab().conversation.clone();
        conversation.messages = (0..6)
            .map(|i| ChatMessage {
                id: format!("resize-{i}"),
                role: MessageRole::User,
                content: format!(
                    "marker {i}: {}",
                    "wrapping changes these heights ".repeat(16)
                ),
                ..ChatMessage::default()
            })
            .collect();
        let mut state = TuiState::new(80, 24);
        tabs.active_tab_mut().conversation = conversation.clone();
        load_active_tab(
            &tabs,
            &mut conversation,
            &mut StreamingState::default(),
            &mut SessionManager::new(SessionState::Empty),
            &mut state,
            &mut TurnQueue::default(),
        );
        paint_active_stage(&tabs, &mut state, &conversation, 80);
        let old_height = state.total_content_height;
        let old_boundaries = state.block_boundaries.clone();
        paint_active_stage(&tabs, &mut state, &conversation, 40);
        assert!(
            state.total_content_height > old_height,
            "real wrapping changed"
        );
        assert_ne!(state.block_boundaries, old_boundaries);
        state.pending_anchor = Some(if valid_anchor { 1 } else { usize::MAX });
        let expected_offset = if valid_anchor {
            state
                .total_content_height
                .saturating_sub(10)
                .saturating_sub(state.block_boundaries[1])
        } else {
            0
        };
        assert_ne!(expected_offset, 41, "independent stale-offset control");
        resolve_resize_anchor(Some(&mut tabs), &mut state);
        assert_eq!(state.scroll_offset(), expected_offset);
        assert!(state.pending_anchor.is_none());
        let before = paint_active_stage(&tabs, &mut state, &conversation, 40);
        let mut expected = old_stage_view();
        expected.scroll_offset = expected_offset;
        expected.mode = if expected_offset == 0 {
            AnchorMode::Following
        } else {
            AnchorMode::Reading
        };
        let action = press(&mut state, Key::Ctrl('f'));
        assert_eq!(action, InputAction::OpenSearch);
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert_eq!(
            state.scroll_offset(),
            expected_offset,
            "opening must not jump"
        );
        assert_eq!(
            paint_active_stage(&tabs, &mut state, &conversation, 40),
            before,
            "the painted Stage must not move on Search-open"
        );
        assert_eq!(
            format!("{:?}", state.search_state.pre_search_view.as_ref().unwrap()),
            format!("{expected:?}"),
            "snapshot includes the resolved offset and all original metadata"
        );
        let action = press(&mut state, Key::Char('m'));
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert!(!state.search_state.matches.is_empty());
        let action = press(&mut state, Key::Esc);
        assert_eq!(action, InputAction::CloseSearch);
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert_eq!(
            format!("{:?}", tabs.active_tab().view_state),
            format!("{expected:?}")
        );
        assert_eq!(state.scroll_offset(), expected_offset);
        assert_eq!(state.auto_scroll(), expected_offset == 0);
        assert_eq!(
            paint_active_stage(&tabs, &mut state, &conversation, 40),
            before
        );
    }
}

#[test]
fn new_clears_tab_owned_view_before_search_snapshot() {
    reset_clears_tab_owned_view_before_search_snapshot("/new");
}

#[test]
fn clear_clears_tab_owned_view_before_search_snapshot() {
    reset_clears_tab_owned_view_before_search_snapshot("/clear");
}

fn reset_clears_tab_owned_view_before_search_snapshot(command: &str) {
    use rustain::domain::models::{Conversation, view_state::ViewState};
    use rustain::infrastructure::runtime::event_loop::reset_stage_conversation;
    {
        let mut tabs = TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().view_state = old_stage_view();
        let mut conversation = tabs.active_tab().conversation.clone();
        let mut state = TuiState::new(80, 24);
        load_active_tab(
            &tabs,
            &mut conversation,
            &mut StreamingState::default(),
            &mut SessionManager::new(SessionState::Empty),
            &mut state,
            &mut TurnQueue::default(),
        );
        state.pending_anchor = Some(2);
        reset_stage_conversation(
            &mut tabs,
            &mut conversation,
            &mut state,
            Conversation {
                id: command.into(),
                ..Conversation::default()
            },
        );
        assert_ne!(tabs.active_tab().conversation.id, conversation.id);
        assert_eq!(state.scroll_offset(), 0);
        assert!(state.auto_scroll());
        let before = paint_active_stage(&tabs, &mut state, &conversation, 80);
        let action = press(&mut state, Key::Ctrl('f'));
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert_eq!(state.scroll_offset(), 0, "{command}: no stale opening jump");
        assert_eq!(
            format!("{:?}", state.search_state.pre_search_view.as_ref().unwrap()),
            format!("{:?}", ViewState::default()),
            "{command}: no old anchor/collapse/tier/focus/append metadata"
        );
        assert!(
            state.pending_anchor.is_none(),
            "{command}: old resize anchor cleared"
        );
        assert_eq!(
            state.modal_stack.top().unwrap().conversation_id.as_deref(),
            Some(command)
        );
        let action = press(&mut state, Key::Char('z'));
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert!(
            presentation::is_open(&state, Surface::Search),
            "current proxy owns Search"
        );
        let action = press(&mut state, Key::Esc);
        search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
        assert_eq!(
            format!("{:?}", tabs.active_tab().view_state),
            format!("{:?}", ViewState::default())
        );
        assert_eq!(
            paint_active_stage(&tabs, &mut state, &conversation, 80),
            before
        );
    }
}

#[test]
fn readonly_tab_return_loads_destination_before_search() {
    use rustain::domain::models::FocusState;
    use rustain::infrastructure::runtime::event_loop::return_from_read_only_tab;
    let mut tabs = TabManager::new(CancellationToken::new());
    tabs.active_tab_mut().view_state = old_stage_view();
    tabs.active_tab_mut().view_state.scroll_offset = 7;
    let destination_id = tabs.active_tab().conversation.id.clone();
    let destination_view = format!("{:?}", tabs.active_tab().view_state);
    tabs.create_tab();
    tabs.active_tab_mut().read_only = true;
    tabs.active_tab_mut().view_state = old_stage_view();
    let mut conversation = tabs.active_tab().conversation.clone();
    let readonly_id = conversation.id.clone();
    let mut state = TuiState::new(80, 24);
    let mut streaming = StreamingState::default();
    let mut session = SessionManager::new(SessionState::Empty);
    let mut queue = TurnQueue::default();
    load_active_tab(
        &tabs,
        &mut conversation,
        &mut streaming,
        &mut session,
        &mut state,
        &mut queue,
    );
    return_from_read_only_tab(
        &mut tabs,
        &mut conversation,
        &mut streaming,
        &mut session,
        &mut state,
        &mut queue,
    );
    assert_ne!(readonly_id, destination_id, "distinct proxy control");
    assert_eq!(tabs.active_tab().conversation.id, destination_id);
    assert_eq!(
        conversation.id, destination_id,
        "return must load the active proxy"
    );
    assert_eq!(state.active_tab_id, tabs.active_tab_id());
    assert_eq!(
        state.scroll_offset(),
        7,
        "return must load destination mirrors"
    );
    state.focus = FocusState::Input;
    let action = press(&mut state, Key::Ctrl('f'));
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert_eq!(
        format!("{:?}", state.search_state.pre_search_view.as_ref().unwrap()),
        destination_view
    );
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::CloseSearch,
    );
    assert_eq!(
        format!("{:?}", tabs.active_tab().view_state),
        destination_view
    );
}

#[test]
fn cross_search_existing_tab_position_is_authoritative_before_search() {
    cross_search_position_is_authoritative(false);
}

#[test]
fn cross_search_new_tab_position_is_authoritative_before_search() {
    cross_search_position_is_authoritative(true);
}

fn cross_search_position_is_authoritative(new_tab: bool) {
    use rustain::domain::models::{Conversation, FocusState};
    use rustain::infrastructure::runtime::event_loop::{
        position_cross_search_result, replace_stage_conversation,
    };
    let mut tabs = TabManager::new(CancellationToken::new());
    let mut conversation = tabs.active_tab().conversation.clone();
    let mut state = TuiState::new(80, 24);
    let loaded = Conversation {
        id: "cross-search-destination".into(),
        ..Conversation::default()
    };
    tabs.create_tab_with_conversation(loaded.clone());
    if new_tab {
        // The direct-new production branch replaces only the proxy.
        replace_stage_conversation(&mut conversation, &mut state, loaded);
    } else {
        tabs.active_tab_mut().view_state = old_stage_view();
        load_active_tab(
            &tabs,
            &mut conversation,
            &mut StreamingState::default(),
            &mut SessionManager::new(SessionState::Empty),
            &mut state,
            &mut TurnQueue::default(),
        );
    }
    state.message_boundaries = vec![0, 20, 60];
    state.total_content_height = 100;
    state.viewport_height = 10;
    let mut expected = tabs.active_tab().view_state.clone();
    expected.scroll_offset = 70; // (100 - 10) - target's line 20
    expected.mode = AnchorMode::Reading;
    position_cross_search_result(&mut tabs, &mut state, 1);
    assert_eq!(
        state.scroll_offset(),
        70,
        "independent production positioning control"
    );
    assert!(!state.auto_scroll());
    state.focus = FocusState::Input;
    let action = press(&mut state, Key::Ctrl('f'));
    search::handle_search_action(&mut tabs, &mut state, &conversation, &action);
    assert_eq!(
        state.scroll_offset(),
        70,
        "opening cannot undo CrossSearch positioning"
    );
    assert_eq!(
        format!("{:?}", state.search_state.pre_search_view.as_ref().unwrap()),
        format!("{expected:?}"),
        "position-only sync preserves collapse/tier/focus/append metadata"
    );
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::CloseSearch,
    );
    assert_eq!(
        format!("{:?}", tabs.active_tab().view_state),
        format!("{expected:?}")
    );
}

#[test]
fn readonly_first_tab_return_does_not_wrap() {
    use rustain::infrastructure::runtime::event_loop::return_from_read_only_tab;
    let mut tabs = TabManager::new(CancellationToken::new());
    let first = tabs.active_tab_id();
    tabs.active_tab_mut().view_state = old_stage_view();
    tabs.active_tab_mut().read_only = true;
    tabs.create_tab();
    tabs.switch_to_index(1);
    let expected = format!("{:?}", tabs.active_tab().view_state);
    let mut conversation = tabs.active_tab().conversation.clone();
    let id = conversation.id.clone();
    let mut state = TuiState::new(80, 24);
    let mut streaming = StreamingState::default();
    let mut session = SessionManager::new(SessionState::Empty);
    let mut queue = TurnQueue::default();
    load_active_tab(
        &tabs,
        &mut conversation,
        &mut streaming,
        &mut session,
        &mut state,
        &mut queue,
    );
    return_from_read_only_tab(
        &mut tabs,
        &mut conversation,
        &mut streaming,
        &mut session,
        &mut state,
        &mut queue,
    );
    assert_eq!(tabs.active_tab_id(), first);
    assert_eq!(state.active_tab_id, first);
    assert_eq!(conversation.id, id);
    assert_eq!(format!("{:?}", tabs.active_tab().view_state), expected);
    assert_eq!(state.scroll_offset(), 41);
}
