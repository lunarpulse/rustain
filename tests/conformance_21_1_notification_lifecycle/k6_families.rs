//! K6 — the Flash families are store notices (AC4, AC6).
//!
//! One real producer per family, entered through its public handler front
//! door: bookmark (`handle_apply_open_bookmark_list`), search wrap
//! (`search::handle_search_action`) and export (`apply_export_command`, the
//! event loop's own re-export). Success is typed, never inferred from the
//! message text, and no family writes the status row. The in-lib families
//! (clipboard copy, permission mode, attached read-only send, socket closed)
//! are `story21_1_` tests next to their private seams.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::handlers::bookmark::handle_apply_open_bookmark_list;
use rustain::adapters::tui::handlers::search;
use rustain::adapters::tui::notifications::{NoticeSeverity, NoticeSource};
use rustain::adapters::tui::presentation::{Surface, reducer};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::conversation::Conversation;
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::{SessionMeta, StatusState};
use rustain::domain::services::search::SearchMatch;
use rustain::infrastructure::runtime::event_loop::apply_export_command;
use tokio_util::sync::CancellationToken;

fn newest(state: &TuiState) -> (NoticeSource, NoticeSeverity, String) {
    let record = state
        .notifications
        .records()
        .max_by_key(|record| (record.last_at, record.id))
        .expect("the producer raised a notice");
    (record.source, record.severity, record.message.clone())
}

fn idle_state() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.status = StatusState::Idle;
    state
}

fn meta() -> SessionMeta {
    SessionMeta {
        version: 1,
        title: "K6".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_060,
        message_count: 0,
        bookmarks: vec![],
        fork_source: None,
        imported_from: None,
        extra: serde_json::Map::new(),
        plan_slug: None,
    }
}

#[test]
fn k6_bookmark_teaching_notice_is_an_info_notice_not_a_status() {
    let tabs = TabManager::new(CancellationToken::new());
    let mut state = idle_state();
    handle_apply_open_bookmark_list(&tabs, &mut state, &tabs.active_tab().conversation);
    let (source, severity, message) = newest(&state);
    assert_eq!(source, NoticeSource::Bookmark);
    assert_eq!(severity, NoticeSeverity::Info);
    assert!(
        message.starts_with("No bookmarks in this conversation"),
        "{message}"
    );
    assert_eq!(
        state.status,
        StatusState::Idle,
        "the status row holds no flash"
    );
}

#[test]
fn k6_search_wrap_is_an_info_notice_only_when_the_index_wraps() {
    let mut state = idle_state();
    let mut tabs = TabManager::new(CancellationToken::new());
    let conversation = tabs.active_tab().conversation.clone();
    reducer::open(&mut state, Surface::Search);
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::OpenSearch,
    );
    state.search_state.matches = (0..2)
        .map(|message_index| SearchMatch {
            message_index,
            byte_start: 0,
            byte_end: 4,
        })
        .collect();
    // 0 -> 1: no wrap, no notice (positive control for the wrap below).
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::SearchNext,
    );
    assert!(
        state.notifications.is_empty(),
        "a plain step raises nothing"
    );
    // 1 -> 0 wraps forward.
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::SearchNext,
    );
    let (source, severity, message) = newest(&state);
    assert_eq!(
        (source, severity, message.as_str()),
        (NoticeSource::Search, NoticeSeverity::Info, "Wrapped to top")
    );
    // 0 -> 1 wraps backward.
    search::handle_search_action(
        &mut tabs,
        &mut state,
        &conversation,
        &InputAction::SearchPrev,
    );
    assert_eq!(newest(&state).2, "Wrapped to bottom");
    assert_eq!(state.status, StatusState::Idle);
}

#[tokio::test]
async fn k6_export_result_severity_is_typed_success_or_error() {
    let conversation = Conversation::default();

    // Success: a real write under `.rustain/exports`.
    let dir = tempfile::tempdir().unwrap();
    let mut state = idle_state();
    apply_export_command(None, &conversation, &meta(), dir.path(), &mut state).await;
    let (source, severity, message) = newest(&state);
    assert_eq!(
        (source, severity),
        (NoticeSource::Export, NoticeSeverity::Success)
    );
    assert!(message.contains("Exported to"), "{message}");
    assert_eq!(state.status, StatusState::Idle);

    // Failure: `.rustain` is a file, so the exports dir cannot be created.
    let blocked = tempfile::tempdir().unwrap();
    std::fs::write(blocked.path().join(".rustain"), "not a directory").unwrap();
    let mut state = idle_state();
    apply_export_command(None, &conversation, &meta(), blocked.path(), &mut state).await;
    let (source, severity, message) = newest(&state);
    assert_eq!(
        (source, severity),
        (NoticeSource::Export, NoticeSeverity::Error)
    );
    assert!(
        message.starts_with(
            "Could not create the exports folder.\n\
             Check the permissions of .rustain/exports/, then run /export again.\n"
        ),
        "{message}"
    );
    assert_eq!(state.status, StatusState::Idle);
}
