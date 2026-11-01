//! K4 — retention and the unseen badge (AC2, AC5, A6): 1,000 records / 1 MiB
//! FIFO, the badge counts warning/error OCCURRENCES independently of eviction
//! and clears only when S-LOG presents them.

use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rustain::adapters::tui::app::handle_input;
use rustain::adapters::tui::notification_log::{self, LogFacts, LogOutcome};
use rustain::adapters::tui::notifications::{
    self, MAX_MESSAGE_BYTES, MAX_PAYLOAD_BYTES, MAX_RECORDS, NoticeSeverity, NoticeSource,
};
use rustain::adapters::tui::state::{LogAwareness, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::status_bar;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::visual::DensityMode;
use rustain::domain::models::{PermissionMode, StatusState};

use super::support::{advance, clocked, draw, notify, row_text};

/// The real status row, drawn the way both hosts call it.
pub(super) fn status_row(state: &TuiState, width: u16, hint: Option<&str>) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
    let theme = Theme::dark();
    terminal
        .draw(|frame| {
            status_bar::render(
                frame,
                frame.area(),
                "model",
                None,
                &StatusState::Idle,
                &theme,
                0,
                &[],
                0,
                10,
                PermissionMode::Normal,
                None,
                0,
                false,
                None,
                false,
                true,
                0,
                hint,
                0,
                None,
                None,
                None,
                false,
                None,
                None,
                DensityMode::Focus,
                false,
                None,
                LogAwareness::Hidden,
                status_bar::NoticeBadges {
                    unseen: state.notifications.unseen(),
                    decisions: state.decisions.len(),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    row_text(terminal.backend().buffer(), 0)
}

fn special(key: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(key)
}

#[test]
fn k4_the_record_count_is_bounded_and_fifo() {
    let (mut state, _c) = clocked(80, 24);
    for i in 0..=MAX_RECORDS {
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Info,
            format!("distinct {i}"),
        );
    }
    assert_eq!(state.notifications.len(), MAX_RECORDS);
    let first = state.notifications.records().next().unwrap();
    assert_eq!(
        first.message, "distinct 1",
        "the oldest record was evicted first"
    );
    assert_eq!(
        state.notifications.records().last().unwrap().message,
        format!("distinct {MAX_RECORDS}")
    );
}

#[test]
fn k4_the_payload_is_bounded_at_one_mebibyte() {
    let (mut state, _c) = clocked(80, 24);
    let big = |i: usize| format!("{i:04}{}", "x".repeat(MAX_MESSAGE_BYTES - 4));
    let wanted = MAX_PAYLOAD_BYTES / MAX_MESSAGE_BYTES + 6;
    for i in 0..wanted {
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Info,
            big(i),
        );
    }
    assert!(state.notifications.payload_bytes() <= MAX_PAYLOAD_BYTES);
    assert!(
        state.notifications.len() < wanted,
        "payload pressure evicted"
    );
    // FIFO, exactly: the records that went are the `wanted - len` oldest, so
    // the first retained message is that index and the last is the newest.
    let len = state.notifications.len();
    let first = &state.notifications.records().next().unwrap().message;
    assert!(
        first.starts_with(&format!("{:04}", wanted - len)),
        "FIFO: the first retained record is #{} (got {})",
        wanted - len,
        &first[..4]
    );
    let newest = &state.notifications.records().last().unwrap().message;
    assert!(
        newest.starts_with(&format!("{:04}", wanted - 1)),
        "the newest record is retained"
    );
    // A single record above the per-message bound is bounded, never dropped.
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Error,
        "z".repeat(MAX_PAYLOAD_BYTES * 2),
    );
    assert!(state.notifications.payload_bytes() <= MAX_PAYLOAD_BYTES);
    let last = state.notifications.records().last().unwrap();
    assert!(last.message.len() <= MAX_MESSAGE_BYTES && last.message.ends_with('…'));
}

#[test]
fn k4_the_badge_counts_occurrences_independently_of_eviction_and_toast_paint() {
    let (mut state, clock) = clocked(80, 24);
    for _ in 0..3 {
        notify(&mut state, NoticeSeverity::Warning, "repeated warning");
    }
    notify(&mut state, NoticeSeverity::Error, "an error");
    notify(&mut state, NoticeSeverity::Info, "info never counts");
    notify(&mut state, NoticeSeverity::Success, "success never counts");
    assert_eq!(
        state.notifications.unseen(),
        4,
        "3 coalesced warnings + 1 error"
    );
    // Painting the toasts is NOT presentation in the log.
    draw(&mut state, 80, 24, true);
    assert_eq!(state.notifications.unseen(), 4);
    // Evict every warning/error record: the counter does not move. A toast is
    // never evicted before its deadline (S1-9), so let them all expire first.
    advance(&clock, Duration::from_secs(61));
    draw(&mut state, 80, 24, true);
    assert_eq!(state.notifications.visible().count(), 0);
    for i in 0..=MAX_RECORDS {
        notifications::notify(
            &mut state,
            NoticeSource::System,
            NoticeSeverity::Info,
            format!("filler {i}"),
        );
    }
    assert!(
        state
            .notifications
            .records()
            .all(|r| r.severity == NoticeSeverity::Info)
    );
    assert_eq!(state.notifications.unseen(), 4, "eviction never decrements");
    assert!(status_row(&state, 80, None).contains("Log !4"));
}

#[test]
fn k4_opening_the_log_through_ctrl_x_n_clears_the_badge_and_only_that_does() {
    let (mut state, _c) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Warning, "look at me");
    assert!(status_row(&state, 80, None).contains("Log !1"));
    handle_input(&mut state, &special(DomainKey::CtrlX));
    handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    assert!(state.notification_log.is_some(), "Ctrl+X n opened S-LOG");
    assert_eq!(state.notifications.unseen(), 0);
    assert!(!status_row(&state, 80, None).contains("Log !"));
    // While the log is open a new warning is presented at once: no badge.
    notify(&mut state, NoticeSeverity::Warning, "arrives while open");
    assert_eq!(state.notifications.unseen(), 0);
    // The open log owns the keys (the local host consults it before `handle_input`).
    let facts = LogFacts::default();
    let closed =
        notification_log::handle_domain_event(&mut state, &special(DomainKey::Esc), &facts);
    assert_eq!(closed, Some(LogOutcome::Closed));
    assert!(state.notification_log.is_none());
    notify(&mut state, NoticeSeverity::Warning, "arrives after close");
    assert_eq!(state.notifications.unseen(), 1);
    assert!(status_row(&state, 80, None).contains("Log !1"));
}

#[test]
fn k4_the_badge_never_evicts_the_hint() {
    // Admission order: log reminder, then the hint, then the badge. At every
    // width the hint is painted exactly when it is painted without a badge.
    let (mut with_badge, _c) = clocked(80, 24);
    notify(&mut with_badge, NoticeSeverity::Error, "e");
    let (without, _c2) = clocked(80, 24);
    let mut badge_seen = false;
    for width in 20..=120u16 {
        let a = status_row(&with_badge, width, Some("^X d detach"));
        let b = status_row(&without, width, Some("^X d detach"));
        assert_eq!(
            a.contains("^X d detach"),
            b.contains("^X d detach"),
            "width {width}: the badge changed hint admission\n{a:?}\n{b:?}"
        );
        badge_seen |= a.contains("Log !1");
    }
    assert!(badge_seen, "the badge is painted somewhere in the range");
}
