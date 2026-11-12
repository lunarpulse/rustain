//! K10 — failures state an action (AC17; D26). An Error notice is `what` (a one-line fact), `next`
//! (a one-line imperative next step) and an optional `detail`; the toast and the S-LOG row paint
//! the fact, the S-LOG Details paint all three. The domain bridge (turn failures with the generated
//! retry path), the clipboard, session, attached-daemon and runtime producers are `story21_14_`
//! tests next to their crate-private seams (`system_notice`, `error_notices`, `handlers::clipboard`).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use rustain::adapters::tui::notification_log::{self, LogFacts};
use rustain::adapters::tui::notifications::{
    self, NoticeAction, NoticeDraft, NoticeSeverity, NoticeSource, headline,
};
use rustain::adapters::tui::state::TuiState;
use rustain::adapters::tui::widgets::toast_layer;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::conversation::Conversation;
use rustain::domain::models::{SessionMeta, StatusState};
use rustain::infrastructure::runtime::event_loop::apply_export_command;

use super::k8_voice_gate::text_violations;

fn idle_state() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.status = StatusState::Idle;
    state
}

fn meta() -> SessionMeta {
    SessionMeta {
        version: 1,
        title: "K10".to_string(),
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

fn newest_message(state: &TuiState) -> String {
    state
        .notifications
        .records()
        .max_by_key(|record| (record.last_at, record.id))
        .expect("the producer raised a notice")
        .message
        .clone()
}

fn rows_of(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

fn paint_toasts(state: &TuiState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = state.theme.clone();
    terminal
        .draw(|frame| {
            let pane = Rect::new(0, 0, width, height - 2);
            toast_layer::render(
                frame,
                pane,
                &state.notifications,
                &theme,
                &LogFacts::default(),
            );
        })
        .unwrap();
    rows_of(&terminal)
}

fn paint_log(state: &TuiState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = state.theme.clone();
    terminal
        .draw(|frame| {
            notification_log::render(
                frame,
                Rect::new(0, 0, width, height - 1),
                state.notification_log.as_ref().expect("S-LOG open"),
                &state.notifications,
                &theme,
                &LogFacts::default(),
                false,
            );
        })
        .unwrap();
    rows_of(&terminal)
}

#[test]
fn a_failure_is_what_then_next_then_the_optional_detail() {
    let draft = NoticeDraft::failure(
        NoticeSource::Export,
        "Could not export the conversation.",
        "Check the folder, then run /export again.",
        Some("permission denied (os error 13)".into()),
    );
    assert_eq!(draft.severity, NoticeSeverity::Error);
    assert_eq!(
        draft.message,
        "Could not export the conversation.\nCheck the folder, then run /export again.\npermission denied (os error 13)"
    );
    let bare = NoticeDraft::failure(NoticeSource::Export, "A fact.", "Do this.", None);
    assert_eq!(bare.message, "A fact.\nDo this.");
    let blank_detail = NoticeDraft::failure(
        NoticeSource::Export,
        "A fact.",
        "Do this.",
        Some("  ".into()),
    );
    assert_eq!(blank_detail.message, "A fact.\nDo this.");
    // Source text embedded in `what` is flattened so the next step stays on line two.
    let embedded = NoticeDraft::failure(
        NoticeSource::Model,
        "Provider said:\n  rate limited  \n",
        "Wait a moment, then send again.",
        None,
    );
    assert_eq!(
        embedded.message,
        "Provider said: rate limited\nWait a moment, then send again."
    );
}

/// M11's killing test: every Error record carries a non-empty, single-line next step on its
/// second line — through the store and through real producers entered by their public handlers.
#[tokio::test]
async fn every_error_record_states_a_next_step() {
    let mut state = idle_state();
    notifications::failure(
        &mut state,
        NoticeSource::Session,
        "Could not fork the conversation.",
        "Try again after the turn ends.",
        None,
    );
    let direct = newest_message(&state);
    assert_eq!(
        direct.lines().collect::<Vec<_>>(),
        vec![
            "Could not fork the conversation.",
            "Try again after the turn ends."
        ]
    );

    let conversation = Conversation::default();
    let dir = tempfile::tempdir().unwrap();
    let blocked = tempfile::tempdir().unwrap();
    std::fs::write(blocked.path().join(".rustain"), "not a directory").unwrap();
    let escape = dir.path().join("elsewhere").join("out.md");
    let producers: Vec<(&str, TuiState)> = vec![
        ("exports folder cannot be created", {
            let mut s = idle_state();
            apply_export_command(None, &conversation, &meta(), blocked.path(), &mut s).await;
            s
        }),
        ("absolute path refused", {
            let mut s = idle_state();
            let arg = escape.to_string_lossy().into_owned();
            apply_export_command(Some(&arg), &conversation, &meta(), dir.path(), &mut s).await;
            s
        }),
        ("parent directory refused", {
            let mut s = idle_state();
            apply_export_command(
                Some("../out.md"),
                &conversation,
                &meta(),
                dir.path(),
                &mut s,
            )
            .await;
            s
        }),
    ];
    for (label, state) in &producers {
        let record = state
            .notifications
            .records()
            .find(|record| record.severity == NoticeSeverity::Error)
            .unwrap_or_else(|| panic!("{label}: no Error notice"));
        let lines: Vec<&str> = record.message.lines().collect();
        assert!(lines.len() >= 2, "{label}: {:?}", record.message);
        assert!(!lines[0].trim().is_empty(), "{label}: empty fact");
        assert!(!lines[1].trim().is_empty(), "{label}: empty next step");
        assert_ne!(
            lines[0], lines[1],
            "{label}: the next step repeats the fact"
        );
        // The authored lines (fact and next step) obey the voice rules.
        let authored = format!("{}\n{}", lines[0], lines[1]);
        assert!(
            text_violations(label, &authored).is_empty(),
            "{label}: {authored}"
        );
        assert!(
            lines[1].chars().next().is_some_and(char::is_uppercase),
            "{label}: {lines:?}"
        );
    }
}

#[test]
fn the_toast_and_the_log_row_show_the_fact_and_the_details_show_all_three() {
    let mut state = idle_state();
    notifications::failure(
        &mut state,
        NoticeSource::Export,
        "Could not export the conversation.",
        "Check the folder, then run /export again.",
        Some("permission denied (os error 13)".into()),
    );
    let message = newest_message(&state);
    assert_eq!(headline(&message), "Could not export the conversation.…");

    let toast = paint_toasts(&state, 80, 24).join("\n");
    assert!(
        toast.contains("Could not export the conversation.…"),
        "{toast}"
    );
    assert!(!toast.contains("Check the folder"), "{toast}");
    assert!(!toast.contains("permission denied"), "{toast}");

    notification_log::open(&mut state);
    let row = paint_log(&state, 80, 24).join("\n");
    assert!(row.contains("Could not export the conversation.…"), "{row}");
    assert!(!row.contains("Check the folder"), "{row}");

    let facts = LogFacts::default();
    notification_log::handle_domain_event(
        &mut state,
        &DomainInputEvent::SpecialKey(DomainKey::Enter),
        &facts,
    )
    .expect("Enter is a user key");
    let details = paint_log(&state, 80, 24).join("\n");
    for line in [
        "Could not export the conversation.",
        "Check the folder, then run /export again.",
        "permission denied (os error 13)",
    ] {
        assert!(details.contains(line), "Details lack {line:?}:\n{details}");
    }
}

#[test]
fn a_failure_keeps_its_conversation_and_its_retry_handle() {
    let mut state = idle_state();
    notifications::notify_draft(
        &mut state,
        NoticeDraft::failure(
            NoticeSource::System,
            "The provider refused the request.",
            "Retry from the log, or send the message again.",
            None,
        )
        .conversation("conv-1")
        .action(NoticeAction::RetryTurn { messages: 3 }),
    );
    let record = state.notifications.records().next().expect("one record");
    assert_eq!(record.severity, NoticeSeverity::Error);
    assert_eq!(record.conversation_id.as_deref(), Some("conv-1"));
    assert_eq!(record.action, Some(NoticeAction::RetryTurn { messages: 3 }));
    assert_eq!(record.message.lines().count(), 2);
}

#[test]
fn identical_failures_coalesce_like_every_other_notice() {
    let mut state = idle_state();
    for _ in 0..4 {
        notifications::failure(
            &mut state,
            NoticeSource::Bookmark,
            "Could not save the bookmark.",
            "Try again.",
            None,
        );
    }
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications.records().next().unwrap().count, 4);
}

/// 14-C-01: the stored record never loses its next step to the 16 KiB cap, whatever the source
/// text looks like (one huge ASCII or multibyte line, a huge detail, empty and blank inputs).
#[test]
fn the_cap_never_erases_the_next_step_and_clips_on_char_boundaries() {
    use rustain::adapters::tui::notifications::MAX_MESSAGE_BYTES;
    let next = "Run /export again.";
    for what in [
        "x".repeat(3 * MAX_MESSAGE_BYTES),
        "界".repeat(MAX_MESSAGE_BYTES),
        "é".repeat(5_000),
    ] {
        for detail in [
            None,
            Some("d".repeat(3 * MAX_MESSAGE_BYTES)),
            Some("界".repeat(MAX_MESSAGE_BYTES)),
        ] {
            let draft = NoticeDraft::failure(NoticeSource::Export, &what, next, detail);
            assert!(
                draft.message.len() <= MAX_MESSAGE_BYTES,
                "{}",
                draft.message.len()
            );
            let lines: Vec<&str> = draft.message.lines().collect();
            assert_eq!(lines[1], next);
            assert!(lines[0].len() < MAX_MESSAGE_BYTES, "{}", lines[0].len());
            // Valid UTF-8 by construction; the store keeps the record whole.
            let mut state = idle_state();
            notifications::notify_draft(&mut state, draft);
            let record = state.notifications.records().next().unwrap();
            assert_eq!(record.message.lines().nth(1), Some(next));
        }
    }
    // A short fact is never ellipsized; blank detail adds no line; a multi-line fact is one line.
    let short = NoticeDraft::failure(NoticeSource::Export, "界 fact", next, Some(" \n ".into()));
    assert_eq!(short.message, "界 fact\nRun /export again.");
    let flat = NoticeDraft::failure(NoticeSource::Export, "a\n\n b ", next, None);
    assert_eq!(flat.message, "a b\nRun /export again.");
    // R2-01: source text is kept WHOLE whenever the envelope fits the cap (no arbitrary quota).
    for what in ["p".repeat(10 * 1024), "界".repeat(3_000)] {
        let draft = NoticeDraft::failure(NoticeSource::Export, &what, next, None);
        assert_eq!(draft.message, format!("{what}\n{next}"));
        let detail = "d".repeat(1_000);
        let draft = NoticeDraft::failure(NoticeSource::Export, &what, next, Some(detail.clone()));
        assert_eq!(draft.message, format!("{what}\n{next}\n{detail}"));
    }
    // Over the cap the LONGER part is cut first: a short fact stays whole, the long detail is clipped.
    let long = "d".repeat(3 * MAX_MESSAGE_BYTES);
    let draft = NoticeDraft::failure(NoticeSource::Export, "Short fact.", next, Some(long));
    assert!(
        draft
            .message
            .starts_with("Short fact.\nRun /export again.\ndd")
    );
    assert!(draft.message.ends_with('…') && draft.message.len() <= MAX_MESSAGE_BYTES);
    // ... and a long fact is clipped while a short detail stays whole.
    let draft = NoticeDraft::failure(
        NoticeSource::Export,
        "x".repeat(3 * MAX_MESSAGE_BYTES),
        next,
        Some("why".into()),
    );
    assert!(draft.message.ends_with("\nRun /export again.\nwhy"));
    assert!(draft.message.len() <= MAX_MESSAGE_BYTES);
}
