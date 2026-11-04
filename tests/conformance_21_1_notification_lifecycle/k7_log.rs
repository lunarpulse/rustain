//! K7 — S-LOG rows, selection, detail, copy, retry eligibility, dismiss and
//! close (AC5, A7, A8). Keys enter through the shared ingress both hosts use
//! (`handle_domain_event` local, `handle_raw_key` attached); the host side of
//! `Retry`/`Copy` (`notice_actions`, `attach_keys::log_key`) is covered by the
//! in-file `story21_1_` tests next to those private items.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use rustain::adapters::tui::notifications::{
    self, MAX_MESSAGE_BYTES, MAX_RECORDS, NoticeAction, NoticeDraft, NoticeSeverity, NoticeSource,
    Presentation,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{ChatMessage, Conversation, FocusState, MessageRole};

use super::support::{clocked, notify, row_text};

fn key(c: char) -> DomainInputEvent {
    DomainInputEvent::KeyPress(c)
}

fn special(k: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(k)
}

/// Messages the failed conversation held when its turn failed.
const FAILED_AT: usize = 1;

fn facts(active: &str, running: bool, retry: bool) -> LogFacts {
    LogFacts {
        active_conversation_id: Some(active.to_owned()),
        active_messages: FAILED_AT,
        turn_running: running,
        retry_supported: retry,
        decisions_supported: false,
        context_warning: false,
        titles: vec![("conv-bg".into(), "Research".into())],
    }
}

fn press(state: &mut TuiState, event: DomainInputEvent, f: &LogFacts) -> LogOutcome {
    notification_log::handle_domain_event(state, &event, f).expect("a user key")
}

fn paint(state: &TuiState, f: &LogFacts, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = state.theme.clone();
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height - 1);
            notification_log::render(
                frame,
                area,
                state.notification_log.as_ref().expect("log open"),
                &state.notifications,
                &theme,
                f,
                false,
            );
        })
        .unwrap();
    (0..height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect()
}

fn failed_turn(state: &mut TuiState, conversation: &str, message: &str) {
    notifications::notify_draft(
        state,
        NoticeDraft::new(NoticeSource::System, NoticeSeverity::Error, message)
            .conversation(conversation)
            .action(NoticeAction::RetryTurn {
                messages: FAILED_AT,
            }),
    );
}

#[test]
fn k7_ctrl_x_n_and_the_slash_command_are_the_two_front_doors() {
    let (mut state, _c) = clocked(80, 24);
    // Local slash ingress: `/notifications` routes to the dispatcher.
    state.input_buffer = "/notifications".into();
    assert!(matches!(
        submit_message_for_test(&mut state),
        InputAction::ExecuteCommand { ref name, .. } if name == "notifications"
    ));
    // Chord ingress opens S-LOG and saves the caller focus and draft.
    let (mut state, _c) = clocked(80, 24);
    state.focus = FocusState::Chat;
    state.input_buffer = "unsent draft".into();
    rustain::adapters::tui::app::handle_input(&mut state, &special(DomainKey::CtrlX));
    rustain::adapters::tui::app::handle_input(&mut state, &key('n'));
    let log = state.notification_log.as_ref().expect("S-LOG open");
    assert_eq!(log.caller_focus, FocusState::Chat);
    // Every user key is consumed; the draft is never edited underneath.
    let f = facts("conv-a", false, true);
    assert_eq!(press(&mut state, key('z'), &f), LogOutcome::Consumed);
    assert_eq!(
        press(&mut state, special(DomainKey::Backspace), &f),
        LogOutcome::Consumed
    );
    assert_eq!(state.input_buffer, "unsent draft");
    // Resize is not user input: the caller handles it.
    assert!(
        notification_log::handle_domain_event(&mut state, &DomainInputEvent::Resize(90, 30), &f)
            .is_none()
    );
    // Esc closes and restores the caller focus.
    assert_eq!(
        press(&mut state, special(DomainKey::Esc), &f),
        LogOutcome::Closed
    );
    assert!(state.notification_log.is_none());
    assert_eq!(state.focus, FocusState::Chat);
    assert_eq!(state.input_buffer, "unsent draft");
}

#[test]
fn k7_ctrl_c_closes_like_escape() {
    let (mut state, _c) = clocked(80, 24);
    state.focus = FocusState::Input;
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    assert_eq!(
        press(&mut state, special(DomainKey::CtrlC), &f),
        LogOutcome::Closed
    );
    assert_eq!(state.focus, FocusState::Input);
}

#[test]
fn k7_rows_show_severity_time_source_count_and_message_newest_first() {
    let (mut state, _c) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Info, "older info");
    for _ in 0..3 {
        notify(&mut state, NoticeSeverity::Warning, "newer warning");
    }
    notification_log::open(&mut state);
    let rows = paint(&state, &facts("conv-a", false, true), 80, 24);
    let newer = rows
        .iter()
        .position(|r| r.contains("newer warning"))
        .expect("newer row");
    let older = rows
        .iter()
        .position(|r| r.contains("older info"))
        .expect("older row");
    assert!(newer < older, "newest first: {rows:#?}");
    let re = |row: &str, severity: &str, source: &str, count: Option<&str>| {
        let cols: Vec<&str> = row
            .trim_matches(|c| c == '│' || c == ' ')
            .split(" · ")
            .collect();
        assert_eq!(cols[0], severity, "{row:?}");
        assert_eq!(cols[1].len(), 8, "HH:MM:SS in {row:?}");
        assert!(
            cols[1].as_bytes()[2] == b':' && cols[1].as_bytes()[5] == b':',
            "{row:?}"
        );
        assert_eq!(cols[2], source);
        if let Some(count) = count {
            assert_eq!(cols[3], count, "{row:?}");
        }
    };
    re(&rows[newer], "Warning", "system", Some("×3"));
    re(&rows[older], "Info", "system", None);
    assert!(
        rows.iter().any(|r| r.contains("System · Notifications")),
        "frame title"
    );
}

#[test]
fn k7_selection_moves_with_j_k_and_arrows_and_stays_in_bounds() {
    let (mut state, _c) = clocked(80, 24);
    for i in 0..3 {
        notify(&mut state, NoticeSeverity::Error, &format!("row {i}"));
    }
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    let selected = |s: &TuiState| s.notification_log.as_ref().unwrap().selected;
    assert_eq!(selected(&state), 0);
    press(&mut state, key('k'), &f);
    assert_eq!(selected(&state), 0, "no wrap above the newest");
    press(&mut state, key('j'), &f);
    press(&mut state, special(DomainKey::Down), &f);
    press(&mut state, key('j'), &f);
    assert_eq!(selected(&state), 2, "no wrap below the oldest");
    press(&mut state, special(DomainKey::Up), &f);
    assert_eq!(selected(&state), 1);
}

#[test]
fn k7_enter_shows_the_full_message_and_copy_carries_it_whole() {
    let (mut state, _c) = clocked(80, 24);
    notify(
        &mut state,
        NoticeSeverity::Warning,
        "Logged out of OpenAI account API key #1.\nNo OpenAI credentials remain.",
    );
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    let list = paint(&state, &f, 80, 24);
    assert!(
        list.iter()
            .any(|r| r.contains("Logged out of OpenAI account API key #1.…"))
    );
    assert!(
        !list
            .iter()
            .any(|r| r.contains("No OpenAI credentials remain."))
    );
    press(&mut state, special(DomainKey::Enter), &f);
    let detail = paint(&state, &f, 80, 24);
    assert!(
        detail
            .iter()
            .any(|r| r.contains("No OpenAI credentials remain.")),
        "{detail:#?}"
    );
    assert_eq!(
        press(&mut state, key('c'), &f),
        LogOutcome::Copy(
            "Logged out of OpenAI account API key #1.\nNo OpenAI credentials remain.".into()
        ),
        "c is Copy and nothing else"
    );
    // The key that is NOT an action types nothing and executes nothing.
    assert_eq!(press(&mut state, key('K'), &f), LogOutcome::Consumed);
}

#[test]
fn k7_retry_is_offered_and_acts_only_on_eligible_records_exactly_once() {
    let (mut state, _c) = clocked(80, 24);
    notification_log::open(&mut state);
    notification_log::close(&mut state);
    failed_turn(&mut state, "conv-a", "provider failed");
    notification_log::open(&mut state);
    let eligible = facts("conv-a", false, true);
    assert!(
        paint(&state, &eligible, 80, 24)
            .iter()
            .any(|r| r.contains("[r]")),
        "eligible: Retry is painted"
    );
    // A running turn, a different active conversation and a host without a
    // retry path (attached) all refuse and keep the handle.
    for (label, f) in [
        ("running", facts("conv-a", true, true)),
        (
            "other active conversation",
            facts("conv-other", false, true),
        ),
        ("attached host", facts("conv-a", false, false)),
    ] {
        assert!(
            !paint(&state, &f, 80, 24).iter().any(|r| r.contains("[r]")),
            "{label}: Retry must not be painted"
        );
        assert_eq!(
            press(&mut state, key('r'), &f),
            LogOutcome::Consumed,
            "{label}"
        );
    }
    assert_eq!(
        press(&mut state, key('r'), &eligible),
        LogOutcome::Retry {
            action: NoticeAction::RetryTurn {
                messages: FAILED_AT
            },
            conversation_id: "conv-a".into()
        }
    );
    assert_eq!(
        press(&mut state, key('r'), &eligible),
        LogOutcome::Consumed,
        "the handle is consumed: a second r does nothing"
    );
}

#[test]
fn k7_background_records_name_their_conversation_and_have_no_retry() {
    let (mut state, _c) = clocked(80, 24);
    failed_turn(&mut state, "conv-bg", "background provider failure");
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    let rows = paint(&state, &f, 80, 24);
    assert!(rows.iter().any(|r| r.contains("[Research]")), "{rows:#?}");
    assert!(!rows.iter().any(|r| r.contains("[r]")));
    assert_eq!(press(&mut state, key('r'), &f), LogOutcome::Consumed);
}

#[test]
fn k7_compaction_failure_retry_is_eligible_for_the_active_conversation_only() {
    let (mut state, _c) = clocked(80, 24);
    notifications::notify_draft(
        &mut state,
        NoticeDraft::new(
            NoticeSource::Compaction,
            NoticeSeverity::Error,
            "Compaction failed",
        )
        .conversation("conv-a")
        .action(NoticeAction::RetryCompaction),
    );
    notification_log::open(&mut state);
    // Compaction retry does not need an idle turn, only the active conversation.
    let f = facts("conv-a", true, true);
    assert_eq!(
        press(&mut state, key('r'), &f),
        LogOutcome::Retry {
            action: NoticeAction::RetryCompaction,
            conversation_id: "conv-a".into()
        }
    );
}

#[test]
fn k7_x_dismisses_the_toast_presentation_and_never_the_record() {
    let (mut state, _c) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Error, "dismiss me");
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    assert_eq!(state.notifications.visible().count(), 1);
    assert_eq!(press(&mut state, key('x'), &f), LogOutcome::Consumed);
    assert_eq!(state.notifications.visible().count(), 0);
    let record = state.notifications.records().next().unwrap();
    assert_eq!(record.presentation, Presentation::Archived);
    assert_eq!(record.message, "dismiss me", "the fact stays in the log");
}

fn chat(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        role,
        content: content.to_owned(),
        ..ChatMessage::default()
    }
}

/// S1-3: a turn-failure record is bound to the turn that failed. After turn 1
/// fails and turn 2 succeeds, `r` on the old record must not truncate turn 2's
/// good answer: it is neither painted nor acted on, and keeps its handle.
#[test]
fn k7_retry_is_bound_to_the_turn_that_failed() {
    let (mut state, _c) = clocked(80, 24);
    let mut conversation = Conversation::default();
    conversation
        .messages
        .push(chat(MessageRole::User, "turn 1"));
    // The Error is raised for turn 1 with the conversation as it stood.
    notifications::notify_draft(
        &mut state,
        NoticeDraft::new(
            NoticeSource::System,
            NoticeSeverity::Error,
            "provider failed",
        )
        .conversation("conv-a")
        .action(NoticeAction::RetryTurn {
            messages: conversation.messages.len(),
        }),
    );
    // Turn 2 succeeds on the same conversation.
    conversation
        .messages
        .push(chat(MessageRole::User, "turn 2"));
    conversation
        .messages
        .push(chat(MessageRole::Assistant, "good answer"));
    let contents = |c: &Conversation| -> Vec<String> {
        c.messages.iter().map(|m| m.content.clone()).collect()
    };
    let before = contents(&conversation);
    notification_log::open(&mut state);
    let mut f = facts("conv-a", false, true);
    f.active_messages = conversation.messages.len();
    assert!(
        !paint(&state, &f, 80, 24).iter().any(|r| r.contains("[r]")),
        "a stale turn failure is not offered a retry"
    );
    assert_eq!(
        press(&mut state, key('r'), &f),
        LogOutcome::Consumed,
        "nothing is scheduled"
    );
    assert_eq!(contents(&conversation), before, "turn 2's answer is intact");
    assert!(
        state
            .notifications
            .records()
            .next()
            .unwrap()
            .action
            .is_some(),
        "the refused key keeps the handle"
    );

    // Positive control: the same record, on the conversation it failed on.
    f.active_messages = FAILED_AT;
    assert!(paint(&state, &f, 80, 24).iter().any(|r| r.contains("[r]")));
    assert_eq!(
        press(&mut state, key('r'), &f),
        LogOutcome::Retry {
            action: NoticeAction::RetryTurn {
                messages: FAILED_AT
            },
            conversation_id: "conv-a".into()
        }
    );
}

/// S1-5: the selection follows the record, not the row. An arrival while the
/// log is open shifts every row down; `x`, `c` and `r` still act on the record
/// the user selected.
#[test]
fn k7_selection_follows_the_record_when_a_notice_arrives_while_open() {
    let (mut state, _c) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Error, "first");
    notify(&mut state, NoticeSeverity::Error, "second");
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    press(&mut state, key('j'), &f); // newest first: second, [first]
    notify(&mut state, NoticeSeverity::Error, "third");
    assert_eq!(
        press(&mut state, key('c'), &f),
        LogOutcome::Copy("first".into()),
        "still the record the user selected"
    );
    assert_eq!(press(&mut state, key('x'), &f), LogOutcome::Consumed);
    let visible: Vec<&str> = state
        .notifications
        .visible()
        .map(|r| r.message.as_str())
        .collect();
    assert_eq!(visible, vec!["second", "third"], "only first was dismissed");
}

/// S1-5: when the selected record is evicted while the log is open, the
/// selection falls back to the nearest surviving row instead of pointing past
/// the end (no-op keys) while the paint highlights another row.
#[test]
fn k7_selection_falls_back_to_the_nearest_row_when_its_record_is_evicted() {
    let (mut state, _c) = clocked(80, 24);
    for i in 0..MAX_RECORDS {
        let message = format!("{i:04}{}", "x".repeat(1036));
        notify(&mut state, NoticeSeverity::Info, &message);
    }
    assert_eq!(state.notifications.len(), MAX_RECORDS);
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    for _ in 0..(MAX_RECORDS - 1) {
        press(&mut state, key('j'), &f);
    }
    match press(&mut state, key('c'), &f) {
        LogOutcome::Copy(text) => assert!(text.starts_with("0000"), "the oldest row"),
        other => panic!("expected Copy, got {other:?}"),
    }
    // One large arrival pushes the payload over the cap: the oldest records,
    // including the selected one, are evicted and the log shrinks.
    notify(
        &mut state,
        NoticeSeverity::Info,
        &"y".repeat(MAX_MESSAGE_BYTES - 1),
    );
    assert!(state.notifications.len() < MAX_RECORDS, "the log shrank");
    let oldest = state
        .notifications
        .records()
        .next()
        .unwrap()
        .message
        .clone();
    assert!(!oldest.starts_with("0000"), "the selected record is gone");
    assert_eq!(
        press(&mut state, key('c'), &f),
        LogOutcome::Copy(oldest),
        "the nearest surviving row is selected, not nothing"
    );
}

/// S1-11: a message that opens with blank lines paints its first real line in
/// the log row (the toast has the same rule).
#[test]
fn k7_a_message_with_leading_blank_lines_still_paints_its_text() {
    let (mut state, _c) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Warning, "\n\nreal text");
    notification_log::open(&mut state);
    let f = facts("conv-a", false, true);
    assert!(
        paint(&state, &f, 80, 24)
            .iter()
            .any(|r| r.contains("real text")),
    );
}

fn paint_buffer(state: &TuiState, f: &LogFacts, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = state.theme.clone();
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height - 1);
            notification_log::render(
                frame,
                area,
                state.notification_log.as_ref().expect("log open"),
                &state.notifications,
                &theme,
                f,
                false,
            );
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

fn assert_no_control_or_bidi(buffer: &Buffer, what: &str) {
    for cell in buffer.content() {
        for c in cell.symbol().chars() {
            assert!(
                !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
                "{what}: a cell holds {:?}",
                cell.symbol()
            );
        }
    }
}

/// S1-12: producer text and conversation titles are hostile input. Whatever
/// a daemon `SystemNotice` or a title carries (an OSC sequence, BEL, a bidi
/// override, a C1 control), no painted cell is a control character or a bidi
/// override — in the toast, the log row or the log detail.
#[test]
fn k7_hostile_text_never_reaches_a_cell_as_a_control_or_bidi_character() {
    let hostile = "ok\x1b]0;pwn\x07\u{202e}x\u{85}";
    let title = "Re\x1b[2Jsearch\u{202e}\u{2066}x\u{85}";
    let f = LogFacts {
        active_conversation_id: Some("conv-a".into()),
        titles: vec![("conv-bg".into(), title.into())],
        ..LogFacts::default()
    };
    let (mut state, _c) = clocked(80, 24);
    notifications::notify_draft(
        &mut state,
        NoticeDraft::new(
            NoticeSource::System,
            NoticeSeverity::Warning,
            format!("{hostile}\nline two\x1b[31m\u{2067}\u{9b}"),
        )
        .conversation("conv-bg"),
    );

    let toast = super::support::draw_with(&mut state, 80, 24, true, &f);
    assert!(
        super::support::screen(&toast).contains("Warning"),
        "the toast did paint"
    );
    assert_no_control_or_bidi(&toast, "toast");

    notification_log::open(&mut state);
    let row = paint_buffer(&state, &f, 80, 24);
    assert!(super::support::screen(&row).contains("Notifications (1)"));
    assert_no_control_or_bidi(&row, "log row");

    press(&mut state, special(DomainKey::Enter), &f);
    let detail = paint_buffer(&state, &f, 80, 24);
    assert!(super::support::screen(&detail).contains("line two"));
    assert_no_control_or_bidi(&detail, "log detail");
}
