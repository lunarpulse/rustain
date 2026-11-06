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

    // Extend this same real-tab witness without replacing its Ctrl-F path above.
    // History slash is dispatched at its supported width before FrameAPI setup;
    // this does not offer History through an already-owned frame.
    state = super::k7_cross_search::history();
    state.active_tab_id = tabs.active_tab_id();
    super::k7_cross_search::open(&mut state);
    let cross = populate_global_cross(&mut state);
    let payload = cross_payload(&state);
    presentation::reducer::open(&mut state, Surface::Search);
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::OpenSearch,
    );
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Search);
    assert_eq!(
        state.modal_stack.top().unwrap().conversation_id.as_deref(),
        Some(conversation.id.as_str())
    );
    for key in ['?', 'n'] {
        press(&mut state, Key::Ctrl('x'));
        press(&mut state, Key::Char(key));
    }
    let globals: Vec<_> = state
        .modal_stack
        .iter()
        .filter(|frame| frame.surface != Surface::Search)
        .cloned()
        .collect();
    assert_eq!(globals.len(), 3);
    assert_eq!(globals[0], cross);
    tabs.switch_to_index(1);
    let destination = format!("{:?}", tabs.active_tab().view_state);
    load_active_tab(
        &tabs,
        &mut conversation,
        &mut streaming,
        &mut session,
        &mut state,
        &mut queue,
    );
    assert!(!presentation::is_open(&state, Surface::Search));
    assert!(state.search_state.pre_search_view.is_none());
    assert_eq!(
        state.modal_stack.iter().cloned().collect::<Vec<_>>(),
        globals
    );
    assert_eq!(cross_payload(&state), payload);
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
    assert_eq!(state.search_state.query.as_str(), "z");
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
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                Default::default(),
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

#[tokio::test]
async fn new_bookmarks_use_the_live_stage_without_saving_the_tab_mirror() {
    reset_bookmarks_use_the_live_stage("/new").await;
}

#[tokio::test]
async fn clear_bookmarks_use_the_live_stage_without_saving_the_tab_mirror() {
    reset_bookmarks_use_the_live_stage("/clear").await;
}

async fn reset_bookmarks_use_the_live_stage(command: &str) {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::adapters::tui::handlers::{bookmark, wave_overlay};
    use rustain::adapters::tui::widgets::chat_pane::find_scroll_offset_for_message;
    use rustain::domain::models::Conversation;
    use rustain::domain::ports::StoragePort;
    use rustain::infrastructure::runtime::event_loop::reset_stage_conversation;
    let tmp = tempfile::tempdir().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));
    let (mut h, mut tabs) = super::stage_support::stage(80, 24);
    tabs.active_tab_mut().session_meta.bookmarks.clear();
    let mirror_id = tabs.active_tab().conversation.id.clone();
    let mut conversation = tabs.active_tab().conversation.clone();
    let replacement = Conversation {
        id: command.into(),
        ..conversation.clone()
    };
    reset_stage_conversation(&mut tabs, &mut conversation, &mut h.state, replacement);
    assert_ne!(mirror_id, conversation.id, "distinct current Stage control");
    h.render_current_stage(&tabs, &conversation);
    for message in [2, 0] {
        let offset = find_scroll_offset_for_message(
            message,
            &h.state.message_boundaries,
            h.state.total_content_height,
            h.state.viewport_height as usize,
        );
        if message == 0 {
            assert!(offset > 0, "nontrivial jump control");
        }
        tabs.active_tab_mut().view_state.scroll_offset = offset;
        tabs.active_tab_mut().view_state.mode = if offset == 0 {
            AnchorMode::Following
        } else {
            AnchorMode::Reading
        };
        h.state.set_scroll_offset(offset);
        h.state.set_auto_scroll(offset == 0);
        bookmark::handle_apply_bookmark_toggle(&mut tabs, &mut h.state, &conversation, &storage)
            .await;
    }
    assert_eq!(tabs.active_tab().session_meta.bookmarks, [0, 2]);
    tabs.active_tab_mut().view_state.scroll_offset = 0;
    tabs.active_tab_mut().view_state.mode = AnchorMode::Following;
    h.state.set_scroll_offset(0);
    h.state.set_auto_scroll(true);
    h.render_current_stage(&tabs, &conversation);
    let following = super::stage_support::chat_pixels(&h);
    assert_eq!(
        press(&mut h.state, Key::Char('\'')),
        InputAction::OpenBookmarkList
    );
    bookmark::handle_apply_open_bookmark_list(&tabs, &mut h.state, &conversation);
    h.render_current_stage(&tabs, &conversation);
    assert_eq!(
        h.state
            .modal_stack
            .top()
            .unwrap()
            .conversation_id
            .as_deref(),
        Some(command)
    );
    assert!(h.screen_text().contains("Bookmarks"));
    assert_eq!(press(&mut h.state, Key::Enter), InputAction::JumpToBookmark);
    bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &conversation);
    assert!(tabs.active_tab().view_state.scroll_offset > 0);
    assert_eq!(tabs.active_tab().view_state.mode, AnchorMode::Reading);
    h.render_current_stage(&tabs, &conversation);
    assert_ne!(super::stage_support::chat_pixels(&h), following);
    assert_eq!(
        press(&mut h.state, Key::Char('\'')),
        InputAction::OpenBookmarkList
    );
    bookmark::handle_apply_open_bookmark_list(&tabs, &mut h.state, &conversation);
    assert_eq!(
        press(&mut h.state, Key::Char('d')),
        InputAction::DeleteBookmark
    );
    bookmark::handle_apply_delete_bookmark(&mut tabs, &mut h.state, &conversation, &storage).await;
    assert_eq!(tabs.active_tab().session_meta.bookmarks, [2]);
    assert!(presentation::is_open(&h.state, Surface::BookmarkList));
    assert_eq!(
        press(&mut h.state, Key::Char('u')),
        InputAction::UndoBookmarkDelete
    );
    bookmark::handle_apply_undo_bookmark_delete(&mut tabs, &mut h.state, &conversation, &storage)
        .await;
    assert_eq!(tabs.active_tab().session_meta.bookmarks, [0, 2]);
    assert!(h.state.bookmark_undo_buffer.is_none());
    assert_eq!(
        storage
            .load_session_meta(command)
            .await
            .unwrap()
            .unwrap()
            .bookmarks,
        [0, 2]
    );
    assert!(
        storage
            .load_session_meta(&mirror_id)
            .await
            .unwrap()
            .is_none()
    );

    // The current proxy and owning tab are independent of the stale mirror.
    // A genuinely different proxy must still fail closed for all three effects.
    bookmark::handle_apply_delete_bookmark(&mut tabs, &mut h.state, &conversation, &storage).await;
    let other = Conversation {
        id: "wrong-live-stage".into(),
        ..conversation.clone()
    };
    let view = super::stage_support::view(&tabs);
    bookmark::handle_apply_jump_bookmark(&mut tabs, &mut h.state, &other);
    assert_eq!(super::stage_support::view(&tabs), view);
    assert!(!presentation::is_open(&h.state, Surface::BookmarkList));
    bookmark::handle_apply_open_bookmark_list(&tabs, &mut h.state, &conversation);
    bookmark::handle_apply_delete_bookmark(&mut tabs, &mut h.state, &other, &storage).await;
    bookmark::handle_apply_undo_bookmark_delete(&mut tabs, &mut h.state, &other, &storage).await;
    assert_eq!(tabs.active_tab().session_meta.bookmarks, [2]);
    assert!(
        storage
            .load_session_meta(&other.id)
            .await
            .unwrap()
            .is_none()
    );
    bookmark::handle_apply_undo_bookmark_delete(&mut tabs, &mut h.state, &conversation, &storage)
        .await;
    assert_eq!(tabs.active_tab().session_meta.bookmarks, [0, 2]);

    // Wave binds the same live proxy; Search already has this reset regression.
    h.state.wave_run = Some(std::sync::Arc::new(super::keymap_support::StubWave(2)));
    let action = press(&mut h.state, Key::Char('r'));
    assert_eq!(action, InputAction::OpenWaveOverlay);
    assert!(wave_overlay::apply(&mut h.state, &conversation.id, &action));
    h.render_current_stage(&tabs, &conversation);
    assert_eq!(
        h.state
            .modal_stack
            .top()
            .unwrap()
            .conversation_id
            .as_deref(),
        Some(command)
    );
    wave_overlay::apply(
        &mut h.state,
        &conversation.id,
        &InputAction::CloseWaveOverlay,
    );
    assert_eq!(
        tabs.active_tab().conversation.id,
        mirror_id,
        "no hidden mirror save"
    );
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

// These are the production post-storage/load assignments, not command parsing
// or storage-success witnesses. CrossSearch is a global frame across replacement.
fn replace_bookmark_wave_stage(
    reason: &str,
    tabs: &mut TabManager,
    state: &mut TuiState,
    conversation: &mut rustain::domain::models::Conversation,
) {
    use rustain::infrastructure::runtime::event_loop::{
        position_cross_search_result, replace_stage_conversation, reset_stage_conversation,
    };
    let mut replacement = conversation.clone();
    if reason != "switch" && reason != "rewind" {
        replacement.id = format!("replacement-{reason}");
    }
    match reason {
        "/new" | "/clear" => {
            replacement.messages.clear();
            reset_stage_conversation(tabs, conversation, state, replacement);
            return;
        }
        "CrossSearch direct" => {
            // The direct branch replaces the live proxy, not the switch-time
            // conversation mirror in the owning manager.
            replace_stage_conversation(conversation, state, replacement);
            position_cross_search_result(tabs, state, 1);
            return;
        }
        "switch" => {
            tabs.create_tab_with_conversation(replacement);
            tabs.switch_to_index(1);
            tabs.switch_to_index(2);
        }
        "fork" => {
            tabs.create_tab_with_conversation(replacement);
        }
        "rewind" => tabs.active_tab_mut().conversation = replacement,
        _ => unreachable!("replacement reason inventory"),
    }
    tabs.active_tab_mut().view_state.mode = AnchorMode::Reading;
    tabs.active_tab_mut().view_state.scroll_offset = 7;
    load_active_tab(
        tabs,
        conversation,
        &mut StreamingState::default(),
        &mut SessionManager::new(SessionState::Empty),
        state,
        &mut TurnQueue::default(),
    );
}

#[test]
fn bookmark_wave_replacements_discard_live_covered_and_continued_stage_payloads() {
    use super::stage_support::{stage, view};
    use rustain::adapters::tui::{
        handlers::{bookmark, wave_overlay},
        presentation::reducer,
        quit_confirm::{self, QuitFacts, QuitHost},
        state::WaveViewState,
    };
    use rustain::domain::{models::AgentId, ports::wave_handle::WaveHandle};
    use std::{sync::Arc, time::Instant};

    for reason in [
        "switch",
        "/new",
        "/clear",
        "fork",
        "rewind",
        "CrossSearch direct",
    ] {
        for surface in [Surface::BookmarkList, Surface::WaveOverlay] {
            for placement in ["live", "covered", "suspended", "continued", "closed"] {
                if placement == "closed" && surface != Surface::BookmarkList {
                    continue;
                }
                let (mut h, mut tabs) = stage(60, 16);
                let state = &mut h.state;
                let mut conversation = tabs.active_tab().conversation.clone();
                let original = paint_active_stage(&tabs, state, &conversation, 60);
                let run: Arc<dyn WaveHandle> = Arc::new(super::keymap_support::StubWave(3));
                state.wave_run = Some(run.clone());
                state.wave_state = Some(WaveViewState::started(AgentId::root(), 3));
                let wave_state = format!("{:?}", state.wave_state);
                state.wave_cancel = Some(CancellationToken::new());
                let child = state.wave_cancel.as_ref().unwrap().child_token();
                state.active_wave_id = Some(7);
                state.rerunning_slot = Some(1);
                if matches!(placement, "suspended" | "continued") {
                    // Reducer-only depth setup, as in the Search continuation
                    // witness: these covered openers are NOT offered by keys.
                    for global in [Surface::Help, Surface::NotificationLog, Surface::UsagePanel] {
                        reducer::open(state, global);
                    }
                } else {
                    let action = press(
                        state,
                        Key::Char(if surface == Surface::BookmarkList {
                            '\''
                        } else {
                            'r'
                        }),
                    );
                    assert_eq!(
                        action,
                        if surface == Surface::BookmarkList {
                            InputAction::OpenBookmarkList
                        } else {
                            InputAction::OpenWaveOverlay
                        }
                    );
                }
                if surface == Surface::BookmarkList {
                    bookmark::handle_apply_open_bookmark_list(&tabs, state, &conversation);
                    press(state, Key::Char('j'));
                    assert_eq!(
                        (state.bookmark_list_selected, state.bookmark_list_count),
                        (1, 2)
                    );
                    // Post-success Delete producer payload, not a save/Delete
                    // reachability claim; serial Delete has separate witnesses.
                    state.bookmark_undo_buffer =
                        Some(rustain::adapters::tui::state::BookmarkUndo {
                            index: 2,
                            when: Instant::now(),
                            owning_tab: tabs.active_tab_id(),
                            conversation_id: conversation.id.clone(),
                        });
                } else {
                    assert!(wave_overlay::apply(
                        state,
                        &conversation.id,
                        &InputAction::OpenWaveOverlay
                    ));
                    wave_overlay::apply(state, &conversation.id, &InputAction::CloseWaveOverlay);
                    assert!(!presentation::is_open(state, surface));
                    assert!(Arc::ptr_eq(state.wave_run.as_ref().unwrap(), &run));
                    assert_eq!(format!("{:?}", state.wave_state), wave_state);
                    assert_eq!(
                        (state.active_wave_id, state.rerunning_slot),
                        (Some(7), Some(1))
                    );
                    assert!(!child.is_cancelled());
                    // Handler opener seam when globals cover Chat, not a
                    // claim that r is offered through those global owners.
                    wave_overlay::apply(state, &conversation.id, &InputAction::OpenWaveOverlay);
                    press(state, Key::Char('j'));
                    assert_eq!(state.wave_overlay_selected, 1);
                    // StubWave is retained data only and cannot drill. Seed the
                    // completed drill producer payload; d still takes App/effect.
                    state.wave_drill_body = Some((1, "retained drill payload".into()));
                    let action = press(state, Key::Char('d'));
                    assert_eq!(action, InputAction::OpenDivergeView);
                    wave_overlay::apply(state, &conversation.id, &action);
                    assert!(state.wave_diverge_open && state.wave_chat_keys);
                    assert!(state.wave_drill_body.is_some());
                }
                assert_eq!(
                    state.modal_stack.top().unwrap().conversation_id.as_deref(),
                    Some(conversation.id.as_str())
                );
                let mut continued_quit = None;
                match placement {
                    "covered" => {
                        for key in ['?', 'n'] {
                            press(state, Key::Ctrl('x'));
                            press(state, Key::Char(key));
                        }
                        assert_eq!(state.modal_stack.len(), 3);
                    }
                    "suspended" | "continued" => {
                        assert_eq!(state.modal_stack.len(), 4);
                        assert_eq!(press(state, Key::Ctrl('q')), InputAction::Quit);
                        quit_confirm::request(
                            state,
                            QuitFacts {
                                streams: 1,
                                ..QuitFacts::idle(QuitHost::Local)
                            },
                        );
                        if placement == "continued" {
                            continued_quit = state.modal_stack.top().cloned();
                            // Reducer-only nested continuation setup: Login
                            // displaces Quit, which still owns the Stage frame.
                            reducer::open(state, Surface::Login);
                        }
                        assert!(
                            !state
                                .modal_stack
                                .iter()
                                .any(|frame| frame.surface == surface)
                        );
                        assert!(
                            presentation::is_open(state, surface),
                            "real suspended payload"
                        );
                    }
                    "closed" => {
                        bookmark::close(state);
                        assert!(!presentation::is_open(state, surface));
                        assert!(
                            state.bookmark_undo_buffer.is_some(),
                            "ordinary close retains undo"
                        );
                    }
                    _ => {}
                }
                let globals: Vec<_> = state
                    .modal_stack
                    .iter()
                    .filter(|frame| frame.surface != surface)
                    .cloned()
                    .collect();
                let focus = state.focus.clone();
                // Observable destination authority differs from the old Stage;
                // a stale Back/StageRestore must not put offset 3 back here.
                tabs.active_tab_mut().view_state.scroll_offset = 7;
                let expected_offset = match reason {
                    "/new" | "/clear" => 0,
                    "CrossSearch direct" => state
                        .total_content_height
                        .saturating_sub(state.viewport_height as usize)
                        .saturating_sub(state.message_boundaries[1]),
                    _ => 7,
                };
                let old_tab = tabs.active_tab_id();
                let old_id = conversation.id.clone();
                replace_bookmark_wave_stage(reason, &mut tabs, state, &mut conversation);
                let destination = view(&tabs);
                assert_eq!(
                    tabs.active_tab_id() != old_tab,
                    matches!(reason, "switch" | "fork")
                );
                assert_eq!(
                    conversation.id == old_id,
                    matches!(reason, "switch" | "rewind")
                );
                assert_eq!(
                    tabs.active_tab().view_state.scroll_offset,
                    expected_offset,
                    "{reason}"
                );
                assert!(
                    !presentation::is_open(state, surface),
                    "{reason}/{surface:?}/{placement}"
                );
                assert_eq!(
                    state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                    globals
                );
                assert_eq!(state.focus, focus);
                assert_eq!(
                    (state.bookmark_list_selected, state.bookmark_list_count),
                    (0, 0)
                );
                assert!(
                    state.bookmark_undo_buffer.is_none(),
                    "including an already closed list"
                );
                assert_eq!(state.wave_overlay_selected, 0);
                assert!(!state.wave_diverge_open && !state.wave_chat_keys);
                assert!(state.wave_drill_body.is_none());
                assert!(Arc::ptr_eq(state.wave_run.as_ref().unwrap(), &run));
                assert_eq!(format!("{:?}", state.wave_state), wave_state);
                assert_eq!(
                    (state.active_wave_id, state.rerunning_slot),
                    (Some(7), Some(1))
                );
                assert!(!child.is_cancelled());
                let painted = paint_active_stage(&tabs, state, &conversation, 60);
                assert_ne!(
                    painted, original,
                    "independent real-manager destination view control"
                );
                while let Some(top) = state.modal_stack.top().map(|frame| frame.surface) {
                    if top == Surface::QuitConfirm {
                        press(state, Key::Char('n'));
                    } else {
                        reducer::back(state);
                    }
                    if top == Surface::Login {
                        assert_eq!(
                            state.modal_stack.top(),
                            continued_quit.as_ref(),
                            "retained hidden Quit identity"
                        );
                    }
                    assert!(
                        !presentation::is_open(state, surface),
                        "Back cannot resurrect discarded payload"
                    );
                    assert!(presentation::consistent(state));
                }
                bookmark::close(state);
                wave_overlay::apply(state, &conversation.id, &InputAction::CloseWaveOverlay);
                assert_eq!(
                    view(&tabs),
                    destination,
                    "late closes never restore an old view"
                );
                assert_eq!(paint_active_stage(&tabs, state, &conversation, 60), painted);
                // Positive identity control: the retained root is still the
                // original token, not merely a fresh uncancelled replacement.
                state.wave_cancel.as_ref().unwrap().cancel();
                assert!(child.is_cancelled());
            }
        }
    }
}

fn populate_global_cross(state: &mut TuiState) -> presentation::Frame {
    use rustain::adapters::tui::handlers::export::{self, CrossSearchScanAction};
    use rustain::domain::services::cross_search::CrossSearchResult;
    let frame = state.modal_stack.top().unwrap().clone();
    assert_eq!(frame.surface, Surface::CrossSearch);
    assert!(frame.conversation_id.is_none());
    let mut scan = CrossSearchScanAction::Cleared;
    for ch in "ab".chars() {
        assert_eq!(
            press(state, Key::Char(ch)),
            InputAction::CrossSearchQueryChanged
        );
        scan = export::apply_cross_search_query_change(state);
    }
    let CrossSearchScanAction::Spawn {
        query,
        frame_id,
        revision,
    } = scan
    else {
        panic!("the real query effect must claim a generation");
    };
    assert_eq!(frame_id, frame.id);
    assert!(revision > 0 && state.cross_search.running);
    assert_eq!(
        export::apply_cross_search_results(
            state,
            frame_id,
            revision,
            query,
            (0..2)
                .map(|index| CrossSearchResult {
                    conversation_id: format!("stored-{index}"),
                    title: format!("global result {index}"),
                    excerpt: format!("ab excerpt {index}"),
                    timestamp: 1_700_000_000 + index,
                    first_match_message_index: index as usize,
                })
                .collect(),
            true,
            false,
        ),
        export::CrossSearchResultsOutcome::Applied
    );
    press(state, Key::Down);
    assert_eq!(state.cross_search.selected, 1);
    assert_eq!(state.cross_search.query.as_str(), "ab");
    frame
}

fn cross_payload(state: &TuiState) -> String {
    let cross = &state.cross_search;
    format!(
        "{:?}",
        (
            &cross.query,
            cross.request_revision,
            &cross.results,
            cross.selected,
            cross.truncated_by_count,
            cross.truncated_by_time,
            cross.total,
            cross.scanned,
            cross.running,
        )
    )
}

#[test]
fn global_cross_survives_all_stage_reasons_with_mixed_continuations() {
    use rustain::adapters::tui::{
        handlers::{bookmark, export, wave_overlay},
        presentation::reducer,
    };
    let stages = [Surface::Search, Surface::BookmarkList, Surface::WaveOverlay];
    for reason in [
        "switch",
        "/new",
        "/clear",
        "fork",
        "rewind",
        "CrossSearch direct",
    ] {
        for placement in ["live", "covered", "continued", "cross-suspended"] {
            let (mut h, mut tabs) = super::stage_support::stage(130, 30);
            let mut conversation = tabs.active_tab().conversation.clone();
            h.state = super::k7_cross_search::history();
            let state = &mut h.state;
            state.active_tab_id = tabs.active_tab_id();
            paint_active_stage(&tabs, state, &conversation, 130);
            super::k7_cross_search::open(state);
            if placement == "cross-suspended" {
                // Reducer-unit reachability only: reorder the four owners so
                // Cross can be displaced. No History opener inside Frames.
                reducer::close(state, Surface::CrossSearch);
            } else {
                populate_global_cross(state);
            }
            // Handler/FrameAPI setup deliberately bypasses key availability
            // under globals; each production handler binds the actual manager.
            reducer::open(state, Surface::Search);
            search::handle_search_action(&mut tabs, state, &conversation, &InputAction::OpenSearch);
            let action = press(state, Key::Char('s'));
            search::handle_search_action(&mut tabs, state, &conversation, &action);
            assert!(!state.search_state.matches.is_empty());
            assert!(state.search_state.pre_search_view.is_some());
            let search_frame = state.modal_stack.top().unwrap().clone();
            bookmark::handle_apply_open_bookmark_list(&tabs, state, &conversation);
            press(state, Key::Char('j'));
            assert_eq!(
                (state.bookmark_list_selected, state.bookmark_list_count),
                (1, 2)
            );
            let bookmark_frame = state.modal_stack.top().unwrap().clone();
            state.wave_run = Some(std::sync::Arc::new(super::keymap_support::StubWave(3)));
            assert!(wave_overlay::apply(
                state,
                &conversation.id,
                &InputAction::OpenWaveOverlay
            ));
            press(state, Key::Char('j'));
            assert_eq!(state.wave_overlay_selected, 1);
            let wave_frame = state.modal_stack.top().unwrap().clone();
            for (frame, surface) in [search_frame, bookmark_frame, wave_frame]
                .iter()
                .zip(stages)
            {
                assert_eq!(frame.surface, surface);
                assert_eq!(
                    frame.conversation_id.as_deref(),
                    Some(conversation.id.as_str())
                );
                assert!(
                    state.modal_stack.iter().any(|live| live == frame),
                    "{reason}/{placement}: actual live Stage owner before displacement"
                );
            }
            let cross = if placement == "cross-suspended" {
                reducer::open(state, Surface::CrossSearch);
                populate_global_cross(state)
            } else {
                state
                    .modal_stack
                    .iter()
                    .find(|frame| frame.surface == Surface::CrossSearch)
                    .unwrap()
                    .clone()
            };
            let payload = cross_payload(state);
            let mut retained = vec![cross.clone()];
            if placement != "live" {
                reducer::open(state, Surface::Help);
                retained.push(state.modal_stack.top().unwrap().clone());
                let suspended = if placement == "cross-suspended" {
                    Surface::CrossSearch
                } else {
                    Surface::WaveOverlay
                };
                assert!(
                    !state
                        .modal_stack
                        .iter()
                        .any(|frame| frame.surface == suspended)
                );
                assert!(
                    presentation::is_open(state, suspended),
                    "owned suspended payload"
                );
                if matches!(placement, "continued" | "cross-suspended") {
                    reducer::open(state, Surface::Login);
                    retained.push(state.modal_stack.top().unwrap().clone());
                    assert!(
                        !state
                            .modal_stack
                            .iter()
                            .any(|frame| frame.surface == Surface::Help)
                    );
                    assert!(
                        presentation::is_open(state, Surface::Help),
                        "nested continuation"
                    );
                    assert!(
                        presentation::is_open(state, suspended),
                        "suspended descendant"
                    );
                }
            }
            let globals: Vec<_> = state
                .modal_stack
                .iter()
                .filter(|frame| frame.conversation_id.is_none())
                .cloned()
                .collect();
            let old_tab = tabs.active_tab_id();
            let old_id = conversation.id.clone();
            tabs.active_tab_mut().view_state.scroll_offset = 7;
            let expected_offset = match reason {
                "/new" | "/clear" => 0,
                "CrossSearch direct" => state
                    .total_content_height
                    .saturating_sub(state.viewport_height as usize)
                    .saturating_sub(state.message_boundaries[1]),
                _ => 7,
            };
            replace_bookmark_wave_stage(reason, &mut tabs, state, &mut conversation);
            let destination = super::stage_support::view(&tabs);
            assert_eq!(
                tabs.active_tab_id() != old_tab,
                matches!(reason, "switch" | "fork")
            );
            assert_eq!(
                conversation.id == old_id,
                matches!(reason, "switch" | "rewind")
            );
            assert_eq!(
                tabs.active_tab().view_state.scroll_offset,
                expected_offset,
                "{reason}"
            );
            assert_eq!(
                state.modal_stack.iter().cloned().collect::<Vec<_>>(),
                globals
            );
            assert_eq!(cross_payload(state), payload, "{reason}/{placement}");
            assert!(state.search_state.pre_search_view.is_none());
            assert_eq!(
                (state.bookmark_list_selected, state.bookmark_list_count),
                (0, 0)
            );
            assert_eq!(state.wave_overlay_selected, 0);
            loop {
                for surface in stages {
                    assert!(
                        !presentation::is_open(state, surface),
                        "{reason}/{placement}: no live or suspended Stage resurrection"
                    );
                }
                assert!(presentation::consistent(state));
                let top = state.modal_stack.top().unwrap();
                assert_eq!(
                    Some(top),
                    retained.last(),
                    "exact global return order and identity, including continuations"
                );
                assert_eq!(cross_payload(state), payload);
                if top.surface == Surface::CrossSearch {
                    assert_eq!(top, &cross);
                    assert!(top.conversation_id.is_none());
                    break;
                }
                reducer::back(state);
                retained.pop();
            }
            // Reconciliation retained Cross; explicit user close must not.
            let revision = state.cross_search.request_revision;
            assert_eq!(press(state, Key::Esc), InputAction::CloseCrossSearch);
            export::close_cross_search(state);
            assert!(!presentation::is_open(state, Surface::CrossSearch));
            assert!(state.cross_search.query.is_empty() && state.cross_search.results.is_empty());
            assert_eq!(
                export::apply_cross_search_results(
                    state,
                    cross.id,
                    revision,
                    "ab".into(),
                    vec![],
                    false,
                    false,
                ),
                export::CrossSearchResultsOutcome::DiscardedStale
            );
            search::handle_search_action(
                &mut tabs,
                state,
                &conversation,
                &InputAction::CloseSearch,
            );
            bookmark::close(state);
            wave_overlay::apply(state, &conversation.id, &InputAction::CloseWaveOverlay);
            assert_eq!(
                super::stage_support::view(&tabs),
                destination,
                "late Back/close cannot restore the old Stage into its destination"
            );
        }
    }
}
