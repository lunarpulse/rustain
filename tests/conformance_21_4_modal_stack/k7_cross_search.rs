//! CrossSearch witnesses use its History slash opener and production dispatch.
use super::keymap_support::press;
use rustain::adapters::tui::{app::InputAction, keymap::Key, state::TuiState};
use rustain::adapters::tui::{
    handlers::export::{self, CrossSearchResultsOutcome as Outcome, CrossSearchScanAction as Scan},
    presentation::{self, Surface},
};
use rustain::domain::models::{
    FocusState,
    visual::{DensityMode, PanelType},
};
use rustain::domain::services::cross_search::CrossSearchResult;

pub(super) fn history() -> TuiState {
    let mut state = TuiState::new(130, 30);
    state.focus = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };
    state.sidebar_visible = true;
    state.sidebar_panel = Some(PanelType::History);
    state.density_mode = DensityMode::Monitor;
    state
}

#[test]
fn real_history_opener_owns_a_global_frame_at_narrow_widths() {
    for (width, height) in [(130, 30), (80, 24), (60, 16)] {
        let mut state = history();
        let caller = state.focus.clone();
        assert_eq!(
            press(&mut state, Key::Char('/')),
            InputAction::OpenCrossSearch
        );
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.title()),
            Some("Cross-Search")
        );
        assert!(state.modal_stack.top().unwrap().conversation_id.is_none());
        assert_eq!(
            state.focus, caller,
            "opening must not overwrite interrupted base focus"
        );
        rustain::adapters::tui::app::handle_input(
            &mut state,
            &rustain::domain::events::DomainInputEvent::Resize(width, height),
        );
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.title()),
            Some("Cross-Search")
        );
    }
}

pub(super) fn open(state: &mut TuiState) {
    assert_eq!(press(state, Key::Char('/')), InputAction::OpenCrossSearch);
    assert_eq!(
        state.modal_stack.top().unwrap().surface,
        Surface::CrossSearch
    );
}

fn edit(state: &mut TuiState, key: Key) -> Scan {
    assert_eq!(press(state, key), InputAction::CrossSearchQueryChanged);
    export::apply_cross_search_query_change(state)
}

fn query(state: &mut TuiState, text: &str) -> Scan {
    let mut scan = Scan::Cleared;
    for ch in text.chars() {
        scan = edit(state, Key::Char(ch));
    }
    scan
}

fn deliver(state: &mut TuiState, scan: Scan, title: &str) -> Outcome {
    let Scan::Spawn {
        query,
        frame_id,
        revision,
    } = scan
    else {
        panic!("the real query dispatcher must have claimed a scan");
    };
    export::apply_cross_search_results(
        state,
        frame_id,
        revision,
        query,
        vec![CrossSearchResult {
            conversation_id: "stored-result".into(),
            title: title.into(),
            excerpt: "ab matching text".into(),
            timestamp: 1_700_000_000,
            first_match_message_index: 0,
        }],
        false,
        false,
    )
}

#[test]
fn same_query_reopen_rejects_the_discarded_opening() {
    let mut state = history();
    open(&mut state);
    let old_id = state.modal_stack.top().unwrap().id;
    let old = query(&mut state, "ab");
    presentation::reducer::close(&mut state, Surface::CrossSearch);
    open(&mut state);
    let current = query(&mut state, "ab");
    assert_ne!(state.modal_stack.top().unwrap().id, old_id);
    assert_eq!(
        deliver(&mut state, old, "discarded"),
        Outcome::DiscardedStale
    );
    assert!(state.cross_search.results.is_empty());
    assert!(
        state.cross_search.running,
        "stale delivery cannot finish the current request"
    );
    assert_eq!(deliver(&mut state, current, "live"), Outcome::Applied);
    assert_eq!(state.cross_search.results[0].title, "live");
}

#[test]
fn retyping_back_to_equal_text_rejects_the_earlier_revision() {
    let mut state = history();
    open(&mut state);
    let id = state.modal_stack.top().unwrap().id;
    let old = query(&mut state, "ab");
    let _ = edit(&mut state, Key::Char('c'));
    let current = edit(&mut state, Key::Backspace);
    assert_eq!(state.cross_search.query, "ab");
    assert_eq!(state.modal_stack.top().unwrap().id, id);
    assert_eq!(
        deliver(&mut state, old, "earlier revision"),
        Outcome::DiscardedStale
    );
    assert!(state.cross_search.results.is_empty());
    assert_eq!(
        deliver(&mut state, current, "current revision"),
        Outcome::Applied
    );
    assert_eq!(state.cross_search.results[0].title, "current revision");
}

#[test]
fn covered_live_owner_receives_its_completion_without_taking_keys() {
    let mut state = history();
    open(&mut state);
    let id = state.modal_stack.top().unwrap().id;
    let pending = query(&mut state, "ab");
    press(&mut state, Key::Ctrl('x'));
    press(&mut state, Key::Char('?'));
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
    assert_eq!(
        deliver(&mut state, pending, "covered result"),
        Outcome::Applied
    );
    assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
    press(&mut state, Key::Esc);
    assert_eq!(state.modal_stack.top().unwrap().id, id);
    assert_eq!(state.cross_search.results[0].title, "covered result");
}

#[test]
fn esc_and_ctrl_c_apply_the_legacy_history_return_without_leaving_an_owner() {
    for key in [Key::Esc, Key::Ctrl('c')] {
        let mut state = history();
        open(&mut state);
        let pending = query(&mut state, "ab");
        assert_eq!(press(&mut state, key), InputAction::CloseCrossSearch);
        export::close_cross_search(&mut state);
        assert!(!presentation::is_open(&state, Surface::CrossSearch));
        assert_eq!(
            state.focus,
            FocusState::Sidebar {
                panel: PanelType::History,
                selected: 0
            }
        );
        assert!(state.cross_search.query.is_empty());
        assert!(!state.cross_search.running);
        assert_eq!(
            deliver(&mut state, pending, "late discarded result"),
            Outcome::DiscardedStale
        );
        assert!(state.cross_search.results.is_empty());
    }
}

#[test]
fn fitted_cross_search_keeps_query_selected_result_and_primary_keys() {
    use crate::e2e_harness::TestHarness;
    use rustain::domain::models::TabManager;
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    use tokio_util::sync::CancellationToken;
    let core = AgentCore::test_noop();
    for (width, height) in [(130, 30), (80, 24), (60, 16)] {
        for full in [false, true] {
            let mut h = TestHarness::with_size(130, 30);
            let mut tabs = TabManager::new(CancellationToken::new());
            tabs.active_tab_mut().conversation = h.conversation.clone();
            h.state = history();
            h.state.active_tab_id = tabs.active_tab_id();
            let draft = if full { "1\n2\n3\n4\n5\n6\n7\n8" } else { "" };
            h.state.input_buffer = draft.into();
            h.state.cursor_position = draft.chars().count();
            crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
            open(&mut h.state);
            let scan = query(&mut h.state, "ab");
            assert_eq!(
                deliver(&mut h.state, scan, "Selected result title"),
                Outcome::Applied
            );
            // The real opener ran and painted at a supported History width
            // before changing the terminal and dispatching its Resize event.
            crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
            h.terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            rustain::adapters::tui::app::handle_input(
                &mut h.state,
                &rustain::domain::events::DomainInputEvent::Resize(width, height),
            );
            let screen =
                crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
            assert!(
                screen.contains("/ ab"),
                "{width}x{height} full={full}: {screen}"
            );
            assert!(screen.contains("Selected result title"), "{screen}");
            assert!(
                screen.contains("[Enter] open") && screen.contains("[Esc] close"),
                "{screen}"
            );
            assert!(
                !h.terminal.backend().cursor_visible(),
                "only the frame owns input"
            );
            assert_eq!(h.state.input_buffer, draft);
            assert_eq!(h.state.cursor_position, draft.chars().count());
            let layout = rustain::adapters::tui::layout::compute_layout(
                ratatui::layout::Rect::new(0, 0, width, height),
                &h.state.theme,
                draft,
                tabs.tab_count(),
                h.state.sidebar_visible,
                h.state.density_mode,
            )
            .unwrap();
            let status = h.terminal.backend().buffer();
            let status_row: String = (0..width)
                .map(|x| status[(x, layout.status_bar.y)].symbol())
                .collect();
            assert!(status_row.contains("Ready"), "{status_row}");
        }
    }
}

#[tokio::test]
async fn successful_existing_and_new_result_navigation_releases_the_global_owner() {
    use rustain::adapters::filesystem::FileSystemStorage;
    use rustain::domain::{
        models::{
            generate_conversation_id,
            session::{SessionManager, SessionState},
        },
        ports::StoragePort,
        services::session_index::SessionIndex,
    };
    use rustain::infrastructure::runtime::{agent_core::AgentCore, event_loop};
    for existing in [true, false] {
        for with_permission in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let storage = FileSystemStorage::new(tmp.path().join("sessions"));
            let (mut h, mut tabs) = super::stage_support::stage(130, 30);
            h.conversation = tabs.active_tab().conversation.clone();
            h.state = history();
            h.state.active_tab_id = tabs.active_tab_id();
            let core = AgentCore::test_noop();
            crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
            assert!(h.state.total_content_height > h.state.viewport_height as usize);
            let mut target = h.conversation.clone();
            target.id = generate_conversation_id();
            target.title = "Stored destination".into();
            target.messages[1].content =
                format!("ab selected match\n{}", target.messages[1].content);
            storage.save_conversation(&target).await.unwrap();
            if existing {
                tabs.create_tab_with_conversation(target.clone());
                let tab = tabs.active_tab_mut();
                tab.message_boundaries = h.state.message_boundaries.clone();
                tab.total_content_height = h.state.total_content_height;
                tab.view_state.scroll_offset = 99;
                tab.view_state.mode = rustain::domain::models::AnchorMode::Reading;
                tabs.switch_to_index(1);
            }
            open(&mut h.state);
            let scan = query(&mut h.state, "ab");
            assert_eq!(
                deliver(&mut h.state, scan, "Stored destination"),
                Outcome::Applied
            );
            h.state.cross_search.results[0].conversation_id = target.id.clone();
            h.state.cross_search.results[0].first_match_message_index = 1;
            let permission =
                with_permission.then(|| super::k2_boundary::request_permission(&mut h.state));
            if with_permission {
                assert!(h.state.pending_permission.is_none());
                assert_eq!(h.state.permission_queue.len(), 1);
            }
            assert_eq!(
                press(&mut h.state, Key::Enter),
                InputAction::OpenCrossSearchResult
            );
            let mut session = SessionManager::new(SessionState::Empty);
            let mut index = SessionIndex::new();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            event_loop::apply_open_cross_search_result(
                &mut tabs,
                &mut h.conversation,
                &mut h.streaming,
                &mut session,
                &mut h.state,
                &mut h.turn_queue,
                &storage,
                &storage,
                &mut index,
                &tx,
            )
            .await;
            assert_eq!(h.conversation.id, target.id);
            assert_eq!(tabs.active_tab().conversation.id, target.id);
            assert!(
                !presentation::is_open(&h.state, Surface::CrossSearch),
                "existing={existing}"
            );
            assert!(h.state.cross_search.query.is_empty() && !h.state.cross_search.running);
            if !with_permission {
                assert_eq!(h.state.focus, FocusState::Chat);
            }
            assert_eq!(h.state.search_state.query.as_str(), "ab");
            assert!(h.state.search_state.peek_highlight.is_some());
            assert_eq!(
                tabs.active_tab().view_state.scroll_offset,
                h.state.scroll_offset()
            );
            assert!(
                h.state.scroll_offset() > 0,
                "real long Stage must position above its end"
            );
            assert_ne!(
                tabs.active_tab().view_state.scroll_offset,
                99,
                "position effect must actually execute"
            );
            assert!(!h.state.auto_scroll());
            let positioned = super::stage_support::view(&tabs);
            if let Some(id) = permission {
                super::k2_boundary::assert_runtime_permission_painted(
                    &mut h, &mut tabs, &core, &id,
                );
            } else {
                let action = press(&mut h.state, Key::Ctrl('p'));
                assert_eq!(action, InputAction::Consumed);
                press(&mut h.state, Key::Esc);
            }
            assert_eq!(super::stage_support::view(&tabs), positioned);
        }
    }
}

fn deliver_event(state: &mut TuiState, event: rustain::domain::events::AppEvent) -> Outcome {
    let rustain::domain::events::AppEvent::CrossSearchResultsReady {
        query,
        frame_id,
        revision,
        results,
        truncated_by_count,
        truncated_by_time,
    } = event
    else {
        panic!("actual scan must publish its completion");
    };
    export::apply_cross_search_results(
        state,
        frame_id,
        revision,
        query,
        results,
        truncated_by_count,
        truncated_by_time,
    )
}

#[tokio::test]
async fn real_scan_publication_preserves_generation_through_late_and_covered_delivery() {
    use rustain::{
        adapters::filesystem::FileSystemStorage,
        domain::{ports::StoragePort, services::session_index::SessionIndex},
        infrastructure::runtime::event_loop::start_cross_search_scan,
    };
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let storage: Arc<dyn StoragePort> =
        Arc::new(FileSystemStorage::new(tmp.path().join("sessions")));
    let mut fixture = crate::e2e_harness::TestHarness::with_size(130, 30);
    fixture.send_message("ab durable match from a real stored conversation");
    storage
        .save_conversation(&fixture.conversation)
        .await
        .unwrap();
    let mut index = SessionIndex::new();
    index.touch(&fixture.conversation.id, None, Some(1));
    for path in [
        "same-query reopen",
        "retype",
        "covered",
        "discard",
        "short query",
    ] {
        let mut state = history();
        open(&mut state);
        let _ = query(&mut state, "ab");
        let old_id = state.modal_stack.top().unwrap().id;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let old_work = start_cross_search_scan(&mut state, &storage, &index, &tx).unwrap();
        let old_revision = state.cross_search.request_revision;
        let mut current = None;
        match path {
            "same-query reopen" => {
                export::close_cross_search(&mut state);
                open(&mut state);
                let _ = query(&mut state, "ab");
                assert_ne!(state.modal_stack.top().unwrap().id, old_id);
            }
            "retype" => {
                let _ = edit(&mut state, Key::Char('c'));
                let _ = edit(&mut state, Key::Backspace);
                assert_eq!(state.modal_stack.top().unwrap().id, old_id);
            }
            "covered" => {
                press(&mut state, Key::Ctrl('x'));
                press(&mut state, Key::Char('?'));
                assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
            }
            "discard" => export::close_cross_search(&mut state),
            "short query" => {
                assert_eq!(edit(&mut state, Key::Backspace), Scan::Cleared);
                assert_eq!(state.cross_search.query, "a");
            }
            _ => unreachable!(),
        }
        if matches!(path, "same-query reopen" | "retype") {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            start_cross_search_scan(&mut state, &storage, &index, &tx)
                .unwrap()
                .await
                .unwrap();
            current = Some(rx.try_recv().unwrap());
            if path == "same-query reopen" {
                assert_eq!(
                    state.cross_search.request_revision, old_revision,
                    "equal revision and text make frame identity independently necessary"
                );
            } else {
                assert!(state.cross_search.request_revision > old_revision);
            }
        }
        old_work.await.unwrap();
        let event = rx.try_recv().unwrap();
        if let rustain::domain::events::AppEvent::CrossSearchResultsReady {
            frame_id,
            revision,
            results,
            ..
        } = &event
        {
            assert_eq!(*frame_id, old_id);
            assert_eq!(*revision, old_revision);
            assert_ne!(*frame_id, *revision, "swapped metadata is observably wrong");
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].conversation_id, fixture.conversation.id);
            assert!(results[0].excerpt.contains("ab"));
        } else {
            panic!("real publisher returned the wrong event");
        }
        let outcome = deliver_event(&mut state, event);
        if path == "covered" {
            assert_eq!(outcome, Outcome::Applied);
            assert_eq!(state.modal_stack.top().unwrap().surface, Surface::Help);
            press(&mut state, Key::Esc);
            assert_eq!(state.modal_stack.top().unwrap().id, old_id);
            assert_eq!(
                state.cross_search.results[0].conversation_id,
                fixture.conversation.id
            );
        } else {
            assert_eq!(outcome, Outcome::DiscardedStale, "{path}");
            assert!(state.cross_search.results.is_empty(), "{path}");
            if let Some(event) = current {
                assert!(
                    state.cross_search.running,
                    "late results cannot finish the new work"
                );
                assert_eq!(deliver_event(&mut state, event), Outcome::Applied);
                assert_eq!(
                    state.cross_search.results[0].conversation_id,
                    fixture.conversation.id
                );
            } else {
                assert!(!state.cross_search.running);
            }
        }
    }
}

#[test]
fn cross_search_preserves_the_real_stage_and_composer_with_a_scroll_control() {
    use rustain::adapters::tui::{app, handlers::scroll};
    use rustain::domain::{events::DomainInputEvent, models::view_state::ScrollDelta};
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    let (mut h, mut tabs) = super::stage_support::stage(130, 30);
    h.conversation = tabs.active_tab().conversation.clone();
    h.state = history();
    h.state.active_tab_id = tabs.active_tab_id();
    h.state.input_buffer = "1\n2\n3\n4\n5\n6\n7\n8".into();
    h.state.cursor_position = 5;
    let core = AgentCore::test_noop();
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    let layout = || {
        rustain::adapters::tui::layout::compute_layout(
            ratatui::layout::Rect::new(0, 0, 130, 30),
            &h.state.theme,
            &h.state.input_buffer,
            tabs.tab_count(),
            h.state.sidebar_visible,
            h.state.density_mode,
        )
        .unwrap()
    };
    let rect = layout().chat_pane;
    let input_height = layout().input_area.height;
    let pixels = |h: &crate::e2e_harness::TestHarness| -> String {
        let b = h.terminal.backend().buffer();
        (rect.y..rect.bottom())
            .flat_map(|y| (rect.x..rect.right()).map(move |x| b[(x, y)].symbol()))
            .collect()
    };
    let before_pixels = pixels(&h);
    let before_view = super::stage_support::view(&tabs);
    let before_cache = super::stage_support::cache_keys(&h, &tabs);
    assert!(
        !before_cache.0.is_empty() || !before_cache.1.is_empty(),
        "populated real render cache"
    );
    let composer = (
        h.state.input_buffer.clone(),
        h.state.cursor_position,
        input_height,
    );
    open(&mut h.state);
    let id = h.state.modal_stack.top().unwrap().id;
    let scan = query(&mut h.state, "ab");
    assert_eq!(
        deliver(&mut h.state, scan, "first result"),
        Outcome::Applied
    );
    h.state.cross_search.results.push(CrossSearchResult {
        title: "second result".into(),
        ..h.state.cross_search.results[0].clone()
    });
    press(&mut h.state, Key::Down);
    assert_eq!(
        h.state.cross_search.selected, 1,
        "frame navigation actually changed"
    );
    let wheel = DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(30));
    assert_eq!(
        app::handle_input(&mut h.state, &wheel),
        InputAction::Consumed
    );
    for covered in [true, false] {
        if covered {
            press(&mut h.state, Key::Ctrl('p'));
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::CommandPalette
            );
            press(&mut h.state, Key::Char('m'));
        } else {
            press(&mut h.state, Key::Esc);
            assert_eq!(h.state.modal_stack.top().unwrap().id, id);
            assert_eq!(h.state.cross_search.query, "ab");
            assert_eq!(h.state.cross_search.selected, 1);
        }
        crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
        assert_eq!(super::stage_support::view(&tabs), before_view);
        assert_eq!(super::stage_support::cache_keys(&h, &tabs), before_cache);
        let actual = rustain::adapters::tui::layout::compute_layout(
            ratatui::layout::Rect::new(0, 0, 130, 30),
            &h.state.theme,
            &h.state.input_buffer,
            tabs.tab_count(),
            h.state.sidebar_visible,
            h.state.density_mode,
        )
        .unwrap();
        assert_eq!(actual.chat_pane, rect);
        assert_eq!(
            (
                h.state.input_buffer.clone(),
                h.state.cursor_position,
                actual.input_area.height
            ),
            composer
        );
    }
    assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
    export::close_cross_search(&mut h.state);
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert_eq!(super::stage_support::view(&tabs), before_view);
    assert_eq!(super::stage_support::cache_keys(&h, &tabs), before_cache);
    assert_eq!(pixels(&h), before_pixels);
    let InputAction::MouseScroll(delta) = app::handle_input(&mut h.state, &wheel) else {
        panic!("uncovered production scroll dispatcher must be reachable");
    };
    scroll::handle_apply_scroll_intent(&mut tabs, &mut h.state, &h.conversation, delta);
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert_ne!(
        super::stage_support::view(&tabs),
        before_view,
        "same owning manager actually moves"
    );
    assert_ne!(
        pixels(&h),
        before_pixels,
        "actual runtime paint observes that movement"
    );
}

#[tokio::test]
async fn failed_loads_keep_the_owner_feedback_and_queued_permission_until_explicit_close() {
    use rustain::{
        adapters::filesystem::FileSystemStorage,
        domain::{
            models::session::{SessionManager, SessionState},
            ports::StoragePort,
            services::session_index::SessionIndex,
        },
        infrastructure::runtime::{agent_core::AgentCore, event_loop},
    };
    use std::sync::Arc;
    for path in ["missing", "covered", "io-error"] {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        if path == "io-error" {
            std::fs::create_dir_all(&sessions).unwrap();
            // The observed flat PersistedConversation path, not its SessionMeta sidecar.
            std::fs::write(sessions.join("stored-result.meta.json"), b"{").unwrap();
        }
        let storage = Arc::new(FileSystemStorage::new(sessions));
        if path == "io-error" {
            assert!(
                storage.load_conversation("stored-result").await.is_err(),
                "a corrupt persisted conversation must reach the real error branch"
            );
        }
        let (mut h, mut tabs) = super::stage_support::stage(130, 30);
        h.conversation = tabs.active_tab().conversation.clone();
        if path == "missing" {
            let mut stored = h.conversation.clone();
            stored.id = "stored-result".into();
            storage.save_conversation(&stored).await.unwrap();
            assert!(
                storage
                    .load_conversation(&stored.id)
                    .await
                    .unwrap()
                    .is_some()
            );
            storage.delete_conversation(&stored.id).await.unwrap();
            assert!(
                storage
                    .load_conversation(&stored.id)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        h.state = history();
        h.state.active_tab_id = tabs.active_tab_id();
        let core = AgentCore::test_noop();
        open(&mut h.state);
        let scan = query(&mut h.state, "ab");
        assert_eq!(
            deliver(&mut h.state, scan, "stale stored result"),
            Outcome::Applied
        );
        let owner = h.state.modal_stack.top().cloned();
        let active = tabs.active_tab_id();
        let conversation = h.conversation.id.clone();
        let id = super::k2_boundary::request_permission(&mut h.state);
        if path == "covered" {
            let mut target = h.conversation.clone();
            target.id = "stored-result".into();
            tabs.create_tab_with_conversation(target);
            tabs.switch_to_index(1);
            press(&mut h.state, Key::Ctrl('p'));
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::CommandPalette
            );
        }
        crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
        if path != "covered" {
            h.terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            rustain::adapters::tui::app::handle_input(
                &mut h.state,
                &rustain::domain::events::DomainInputEvent::Resize(80, 24),
            );
        }
        let mut index = SessionIndex::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let pending = if path == "io-error" {
            assert_eq!(
                press(&mut h.state, Key::Char('c')),
                InputAction::CrossSearchQueryChanged
            );
            assert_eq!(h.state.cross_search.query, "abc");
            index.touch("stored-result", Some("stale stored result".into()), Some(1));
            let scan_storage: Arc<dyn StoragePort> = storage.clone();
            let task =
                event_loop::start_cross_search_scan(&mut h.state, &scan_storage, &index, &tx)
                    .expect("real edited query must claim and publish a current scan");
            assert!(h.state.cross_search.running);
            assert_eq!(
                h.state.cross_search.results.len(),
                1,
                "pending scan retains old results"
            );
            Some(task)
        } else {
            None
        };
        crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
        let before_view = super::stage_support::view(&tabs);
        assert!(tabs.active_tab().view_state.scroll_offset > 0);
        assert!(!h.state.cross_search.load_failed);
        let before_query = h.state.cross_search.query.clone();
        h.state.needs_redraw = false;
        let mut session = SessionManager::new(SessionState::Empty);
        event_loop::apply_open_cross_search_result(
            &mut tabs,
            &mut h.conversation,
            &mut h.streaming,
            &mut session,
            &mut h.state,
            &mut h.turn_queue,
            storage.as_ref(),
            storage.as_ref(),
            &mut index,
            &tx,
        )
        .await;
        assert_eq!(tabs.active_tab_id(), active, "{path}");
        assert_eq!(h.conversation.id, conversation, "{path}");
        assert!(presentation::is_open(&h.state, Surface::CrossSearch));
        assert_eq!(h.state.cross_search.query, before_query);
        assert_eq!(h.state.cross_search.results.len(), 1);
        assert!(h.state.pending_permission.is_none());
        assert_eq!(h.state.permission_queue.len(), 1);
        if path == "covered" {
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::CommandPalette
            );
            press(&mut h.state, Key::Esc);
        }
        assert_eq!(h.state.modal_stack.top(), owner.as_ref());
        let screen = crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
        assert_eq!(super::stage_support::view(&tabs), before_view, "{path}");
        if path != "covered" {
            assert!(
                screen.contains("Failed to open conversation"),
                "{path}: load failure must remain visible INSIDE its owning sheet: {screen}"
            );
            assert!(h.state.cross_search.load_failed);
            assert!(h.state.needs_redraw, "{path}: failure must request a paint");
        }
        if let Some(task) = pending {
            task.await.unwrap();
            let event = rx
                .recv()
                .await
                .expect("actual scan must publish its completion");
            assert_eq!(deliver_event(&mut h.state, event), Outcome::Applied);
            assert!(h.state.cross_search.results.is_empty());
            assert!(!h.state.cross_search.running && h.state.cross_search.load_failed);
            let screen =
                crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
            assert!(
                screen.contains("Failed to open conversation")
                    && !screen.contains("No matches found"),
                "valid empty completion must not erase the owned load failure: {screen}"
            );
            assert_eq!(h.state.modal_stack.top(), owner.as_ref());
            assert_eq!(h.state.cross_search.query, before_query);
            assert_eq!(super::stage_support::view(&tabs), before_view);
            assert_eq!(tabs.active_tab_id(), active);
            assert_eq!(h.conversation.id, conversation);
            assert!(h.state.pending_permission.is_none());
            assert_eq!(h.state.permission_queue.len(), 1);
        }
        assert_eq!(
            press(&mut h.state, Key::Char('y')),
            InputAction::CrossSearchQueryChanged
        );
        let _ = export::apply_cross_search_query_change(&mut h.state);
        assert!(
            !h.state.cross_search.load_failed,
            "editing releases stale failure feedback"
        );
        let screen = crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
        assert!(!screen.contains("Failed to open conversation"));
        assert!(
            h.state.pending_permission.is_none(),
            "typing must not answer the queued request"
        );
        assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
        export::close_cross_search(&mut h.state);
        assert!(!h.state.cross_search.load_failed);
        super::k2_boundary::assert_runtime_permission_painted(&mut h, &mut tabs, &core, &id);
    }
}

#[test]
fn a_real_stream_keeps_appending_under_cross_search() {
    use rustain::domain::models::{StreamChunk, TabManager};
    use rustain::infrastructure::runtime::agent_core::AgentCore;
    let mut h = crate::e2e_harness::TestHarness::with_size(130, 30);
    h.send_message("stream beneath CrossSearch");
    h.process_chunk(StreamChunk::Text {
        content: "first line\n".into(),
        parent_tool_use_id: None,
    });
    h.state.focus = history().focus;
    h.state.sidebar_visible = true;
    h.state.sidebar_panel = Some(PanelType::History);
    h.state.density_mode = DensityMode::Monitor;
    h.state.input_buffer = "kept draft".into();
    h.state.cursor_position = 4;
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = h.conversation.clone();
    h.state.active_tab_id = tabs.active_tab_id();
    let core = AgentCore::test_noop();
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert!(h.streaming.is_streaming);
    let view = super::stage_support::view(&tabs);
    open(&mut h.state);
    let owner = h.state.modal_stack.top().cloned();
    h.process_chunk(StreamChunk::Text {
        content: "independent appended row".into(),
        parent_tool_use_id: None,
    });
    crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert!(h.streaming.is_streaming);
    assert_eq!(h.state.modal_stack.top(), owner.as_ref());
    assert_eq!(super::stage_support::view(&tabs), view);
    assert_eq!(
        (&*h.state.input_buffer, h.state.cursor_position),
        ("kept draft", 4)
    );
    assert_eq!(press(&mut h.state, Key::Esc), InputAction::CloseCrossSearch);
    export::close_cross_search(&mut h.state);
    let screen = crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert!(screen.contains("independent appended row"), "{screen}");
    assert!(h.streaming.is_streaming);
}

#[test]
fn cross_search_field_context_protects_arrivals_independently_of_recent_typing() {
    let mut state = history();
    state.last_printable_at = None;
    assert!(
        presentation::arrival_may_preempt(&state, state.notifications.now()),
        "uncovered non-typing positive control"
    );
    open(&mut state);
    state.last_printable_at = None;
    assert!(state.input_buffer.is_empty() && state.cross_search.query.is_empty());
    assert!(!presentation::arrival_may_preempt(
        &state,
        state.notifications.now()
    ));
    export::close_cross_search(&mut state);
    assert!(
        presentation::arrival_may_preempt(&state, state.notifications.now()),
        "closing releases only the typing-context guard"
    );
}

#[test]
fn cross_search_boundary_owns_every_ingress_and_recovers_after_tiny_resize() {
    use rustain::domain::{events::DomainInputEvent, models::view_state::ScrollDelta};
    for key in Key::universe() {
        let mut state = history();
        state.input_buffer = "untouched composer".into();
        state.cursor_position = 5;
        open(&mut state);
        let action = press(&mut state, key);
        assert!(
            !matches!(
                action,
                InputAction::CancelOrQuit
                    | InputAction::PermissionAllow
                    | InputAction::PermissionDeny
                    | InputAction::MouseScroll(_)
            ),
            "{key:?}: {action:?}"
        );
        assert!(
            presentation::is_open(&state, Surface::CrossSearch),
            "{key:?}"
        );
        assert_eq!(
            (&*state.input_buffer, state.cursor_position),
            ("untouched composer", 5)
        );
    }
    let mut state = history();
    open(&mut state);
    let _ = query(&mut state, "ab");
    let owner = state.modal_stack.top().cloned();
    for event in [
        DomainInputEvent::Paste("not composer text".into()),
        DomainInputEvent::ImagePaste(vec![1, 2, 3]),
        DomainInputEvent::MouseScroll(ScrollDelta::WheelUp(3)),
    ] {
        assert_eq!(
            rustain::adapters::tui::app::handle_input(&mut state, &event),
            InputAction::Consumed
        );
        assert!(state.input_buffer.is_empty() && state.pending_images.is_empty());
        assert_eq!(state.cross_search.query, "ab");
        assert_eq!(state.modal_stack.top(), owner.as_ref());
    }
    rustain::adapters::tui::app::handle_input(&mut state, &DomainInputEvent::Resize(59, 16));
    for key in Key::universe() {
        assert_eq!(press(&mut state, key), InputAction::Consumed, "{key:?}");
        assert_eq!(state.modal_stack.top(), owner.as_ref());
        assert_eq!(state.cross_search.query, "ab");
    }
    rustain::adapters::tui::app::handle_input(&mut state, &DomainInputEvent::Resize(80, 24));
    assert_eq!(
        press(&mut state, Key::Char('a')),
        InputAction::CrossSearchQueryChanged
    );
    assert_eq!(
        state.cross_search.query, "aba",
        "visible-width control must really edit the owner"
    );
}

#[test]
fn frame_origin_menu_quit_and_nested_palette_preserve_cross_and_refuse_history_visibly() {
    use rustain::{
        adapters::tui::{
            keymap::{Context, table::CLOSE_WINDOW_FIRST},
            notification_log,
            quit_confirm::{self, QuitFacts, QuitHost},
        },
        domain::models::{
            TabManager,
            palette::{PaletteAction, PaletteEntry, PaletteScope},
        },
        infrastructure::runtime::agent_core::AgentCore,
    };
    let mut h = crate::e2e_harness::TestHarness::with_size(130, 30);
    let mut tabs = TabManager::new(tokio_util::sync::CancellationToken::new());
    tabs.active_tab_mut().conversation = h.conversation.clone();
    h.state = history();
    h.state.active_tab_id = tabs.active_tab_id();
    let core = AgentCore::test_noop();
    open(&mut h.state);
    let _ = query(&mut h.state, "ab");
    let owner = h.state.modal_stack.top().cloned();
    press(&mut h.state, Key::Ctrl('x'));
    assert_eq!(h.state.pending_leader, Some(Context::CrossSearch));
    assert_eq!(press(&mut h.state, Key::Char('h')), InputAction::Consumed);
    assert_eq!(h.state.pending_leader, Some(Context::CrossSearch));
    assert_eq!(
        h.state.leader_feedback.as_ref().unwrap().message,
        CLOSE_WINDOW_FIRST
    );
    let screen = crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert!(screen.contains("Please close the window first"), "{screen}");
    assert_eq!(press(&mut h.state, Key::Ctrl('q')), InputAction::Quit);
    quit_confirm::request(
        &mut h.state,
        QuitFacts {
            streams: 1,
            ..QuitFacts::idle(QuitHost::Local)
        },
    );
    assert_eq!(
        h.state.modal_stack.top().unwrap().surface,
        Surface::QuitConfirm
    );
    let screen = crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
    assert!(
        screen.contains("Stay") && !screen.contains("┌ Ctrl+X "),
        "{screen}"
    );
    press(&mut h.state, Key::Char('n'));
    assert_eq!(h.state.modal_stack.top(), owner.as_ref());
    assert_eq!(h.state.pending_leader, Some(Context::CrossSearch));
    assert_eq!(
        h.state.leader_feedback.as_ref().unwrap().message,
        CLOSE_WINDOW_FIRST
    );
    press(&mut h.state, Key::Esc);
    for action in [
        PaletteAction::OpenPanel(PanelType::History),
        PaletteAction::ExecuteCommand("notifications".into(), None),
    ] {
        press(&mut h.state, Key::Ctrl('p'));
        assert_eq!(
            h.state.modal_stack.top().unwrap().surface,
            Surface::CommandPalette
        );
        h.state.command_palette.filtered_entries = vec![PaletteEntry {
            name: "actual selected target".into(),
            description: String::new(),
            shortcut: None,
            scope: PaletteScope::All,
            action: action.clone(),
        }];
        let effect = press(&mut h.state, Key::Enter);
        if matches!(action, PaletteAction::OpenPanel(_)) {
            assert_eq!(effect, InputAction::Consumed);
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::CommandPalette
            );
            let screen =
                crate::conformance_tui_orchestration::render_runtime(&mut h, &mut tabs, &core);
            assert!(screen.contains("Please close the window first"), "{screen}");
            press(&mut h.state, Key::Esc);
        } else {
            assert_eq!(
                effect,
                InputAction::ExecuteCommand {
                    name: "notifications".into(),
                    args: None
                }
            );
            assert!(
                !presentation::is_open(&h.state, Surface::CommandPalette),
                "A4 closes the palette, not its caller"
            );
            notification_log::open(&mut h.state);
            assert_eq!(
                h.state.modal_stack.top().unwrap().surface,
                Surface::NotificationLog
            );
            press(&mut h.state, Key::Esc);
        }
        assert_eq!(h.state.modal_stack.top(), owner.as_ref());
        assert_eq!(h.state.cross_search.query, "ab");
        assert_eq!(
            h.state.focus,
            FocusState::Sidebar {
                panel: PanelType::History,
                selected: 0
            }
        );
    }
}
