//! K2 — store semantics (AC2, A6): coalescing without renewal, 5-minute
//! post-expiry suppression, displacement order, `Ctrl+X x` archiving.

use std::time::Duration;

use rustain::adapters::tui::notifications::{
    self, NoticeDraft, NoticeSeverity, NoticeSource, NotifyEffect, Presentation,
};
use rustain::adapters::tui::state::TuiState;

use super::support::{advance, clocked, draw, notify, screen};

fn visible(state: &TuiState) -> Vec<String> {
    state
        .notifications
        .visible()
        .map(|r| r.message.clone())
        .collect()
}

#[test]
fn k2_identical_arrivals_coalesce_into_one_counted_toast() {
    let (mut state, _clock) = clocked(80, 24);
    for _ in 0..5 {
        notify(&mut state, NoticeSeverity::Warning, "same warning");
    }
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications.records().next().unwrap().count, 5);
    assert_eq!(visible(&state), ["same warning"]);
    assert!(screen(&draw(&mut state, 80, 24, true)).contains("Warning same warning ×5"));
}

#[test]
fn k2_identity_includes_source_severity_and_message() {
    let (mut state, _clock) = clocked(80, 24);
    // Room for every probe: this keystone is about identity, not displacement.
    state.notifications.set_capacity(10);
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Info,
        "twin",
    );
    notifications::notify(
        &mut state,
        NoticeSource::Clipboard,
        NoticeSeverity::Info,
        "twin",
    );
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Warning,
        "twin",
    );
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Info,
        "twin ",
    );
    assert_eq!(state.notifications.len(), 4, "no field may be ignored");
    // Positive control: the exact repeat does coalesce.
    notifications::notify(
        &mut state,
        NoticeSource::System,
        NoticeSeverity::Info,
        "twin",
    );
    assert_eq!(state.notifications.len(), 4);
    assert_eq!(state.notifications.records().next().unwrap().count, 2);
}

#[test]
fn k2_suppression_window_is_five_minutes_after_expiry() {
    // (offset after the deadline, expect suppressed)
    for (after_deadline, suppressed) in [
        (Duration::from_secs(4 * 60 + 59), true),
        (Duration::from_secs(5 * 60), true),
        (Duration::from_secs(5 * 60 + 1), false),
    ] {
        let (mut state, clock) = clocked(80, 24);
        notify(&mut state, NoticeSeverity::Warning, "repeat offender");
        advance(&clock, Duration::from_secs(10) + after_deadline);
        draw(&mut state, 80, 24, true);
        state.needs_redraw = false;
        let unseen_before = state.notifications.unseen();
        notify(&mut state, NoticeSeverity::Warning, "repeat offender");
        assert_eq!(
            state.notifications.unseen(),
            unseen_before + 1,
            "{after_deadline:?}: the badge counts every occurrence"
        );
        if suppressed {
            assert_eq!(state.notifications.len(), 1, "{after_deadline:?}");
            assert_eq!(state.notifications.records().next().unwrap().count, 2);
            assert!(visible(&state).is_empty(), "{after_deadline:?}: no toast");
            // No toast paints; the one reason to redraw is the `Log !N` badge,
            // which moved with this Warning (S1-10).
            assert!(
                state.needs_redraw,
                "{after_deadline:?}: the badge moved, so the frame is redrawn"
            );
        } else {
            assert_eq!(
                state.notifications.len(),
                2,
                "{after_deadline:?}: new record"
            );
            assert_eq!(visible(&state), ["repeat offender"]);
            assert!(state.needs_redraw, "{after_deadline:?}: it paints again");
        }
    }
}

/// The suppression window is silent: an identical Info (no badge) arriving
/// after its toast expired changes nothing on screen and asks for no redraw.
#[test]
fn k2_a_suppressed_info_requests_no_redraw() {
    let (mut state, clock) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Info, "repeat info");
    advance(&clock, Duration::from_secs(10));
    draw(&mut state, 80, 24, true);
    state.needs_redraw = false;
    notify(&mut state, NoticeSeverity::Info, "repeat info");
    assert_eq!(state.notifications.len(), 1);
    assert_eq!(state.notifications.records().next().unwrap().count, 2);
    assert!(visible(&state).is_empty());
    assert!(!state.needs_redraw, "no toast and no badge: no repaint");
}

#[test]
fn k2_store_reports_silent_for_suppressed_and_repaint_for_visible() {
    let (mut state, clock) = clocked(80, 24);
    let draft = || NoticeDraft::new(NoticeSource::System, NoticeSeverity::Info, "fx");
    assert_eq!(state.notifications.notify(draft()), NotifyEffect::Repaint);
    assert_eq!(state.notifications.notify(draft()), NotifyEffect::Repaint);
    advance(&clock, Duration::from_secs(4));
    assert_eq!(state.notifications.notify(draft()), NotifyEffect::Silent);
}

fn push_all(state: &mut TuiState, items: &[(NoticeSeverity, &str)]) {
    for (severity, message) in items {
        notify(state, *severity, message);
    }
}

#[test]
fn k2_displacement_archives_lowest_severity_then_oldest() {
    use NoticeSeverity::{Error, Info, Warning};
    // 1 Error + 3 Info: the oldest Info goes, the Error never does.
    let (mut state, _c) = clocked(80, 24);
    push_all(
        &mut state,
        &[(Error, "E1"), (Info, "I1"), (Info, "I2"), (Info, "I3")],
    );
    assert_eq!(visible(&state), ["E1", "I2", "I3"]);
    // Archived toasts stay in the log.
    assert_eq!(state.notifications.len(), 4);

    // A Warning outranks Info: Info is displaced first.
    let (mut state, _c) = clocked(80, 24);
    push_all(
        &mut state,
        &[(Warning, "W1"), (Info, "I1"), (Info, "I2"), (Info, "I3")],
    );
    assert_eq!(visible(&state), ["W1", "I2", "I3"]);

    // 2 Errors + 1 Info + a NEW Error: the Info is displaced, both Errors stay.
    let (mut state, _c) = clocked(80, 24);
    push_all(
        &mut state,
        &[(Error, "E1"), (Error, "E2"), (Info, "I1"), (Error, "E3")],
    );
    assert_eq!(visible(&state), ["E1", "E2", "E3"]);

    // 4 Errors: only a newer Error displaces the oldest Error.
    let (mut state, _c) = clocked(80, 24);
    push_all(
        &mut state,
        &[(Error, "E1"), (Error, "E2"), (Error, "E3"), (Error, "E4")],
    );
    assert_eq!(visible(&state), ["E2", "E3", "E4"]);
}

#[test]
fn k2_a_lower_severity_arrival_never_displaces_a_higher_one() {
    use NoticeSeverity::{Error, Info, Warning};
    let (mut state, _c) = clocked(80, 24);
    push_all(&mut state, &[(Error, "E1"), (Error, "E2"), (Error, "E3")]);
    state.needs_redraw = false;
    notify(&mut state, Info, "late info");
    assert_eq!(visible(&state), ["E1", "E2", "E3"]);
    let late = state.notifications.records().last().unwrap();
    assert_eq!(late.message, "late info");
    assert_eq!(
        late.presentation,
        Presentation::Archived,
        "archived directly"
    );
    assert!(!state.needs_redraw, "nothing changed on screen");

    let (mut state, _c) = clocked(80, 24);
    push_all(
        &mut state,
        &[(Warning, "W1"), (Warning, "W2"), (Warning, "W3")],
    );
    notify(&mut state, Info, "info under warnings");
    assert_eq!(visible(&state), ["W1", "W2", "W3"]);
}

#[test]
fn k2_clear_toasts_archives_every_visible_toast_and_keeps_the_log() {
    let (mut state, _c) = clocked(80, 24);
    push_all(
        &mut state,
        &[
            (NoticeSeverity::Error, "kept error"),
            (NoticeSeverity::Info, "kept info"),
        ],
    );
    assert_eq!(state.notifications.clear_toasts(), 2);
    assert!(visible(&state).is_empty());
    assert_eq!(state.notifications.len(), 2, "sources are untouched");
    assert!(!screen(&draw(&mut state, 80, 24, true)).contains("kept"));
}

#[test]
fn k2_a_flood_of_identical_refusals_cannot_fill_the_screen() {
    // DF-266: the same reserved-key refusal on every key press.
    let (mut state, _c) = clocked(80, 24);
    for _ in 0..200 {
        notify(
            &mut state,
            NoticeSeverity::Info,
            "r is reserved while a task runs",
        );
    }
    let screen = screen(&draw(&mut state, 80, 24, true));
    assert_eq!(screen.matches("r is reserved while a task runs").count(), 1);
    assert!(screen.contains("×200"));
}

/// S1-18: identical low-severity arrivals while every slot is held by Errors
/// are archived directly; they coalesce into that one archived record instead
/// of filling the 1,000-record log with duplicates.
#[test]
fn k2_a_flood_of_identical_infos_behind_errors_coalesces_into_one_record() {
    use NoticeSeverity::{Error, Info};
    let (mut state, _c) = clocked(80, 24);
    push_all(&mut state, &[(Error, "E1"), (Error, "E2"), (Error, "E3")]);
    for _ in 0..50 {
        notify(&mut state, Info, "the same info");
    }
    assert_eq!(state.notifications.len(), 4);
    let info = state.notifications.records().last().unwrap();
    assert_eq!(info.message, "the same info");
    assert_eq!(info.count, 50);
    assert_eq!(info.presentation, Presentation::Archived);
    assert_eq!(visible(&state), ["E1", "E2", "E3"]);
}

/// S1-10: a Warning archived directly (the slots hold Errors) paints nothing
/// but moves the `Log !N` badge, so the host must repaint.
#[test]
fn k2_an_archived_warning_still_repaints_for_the_badge() {
    use NoticeSeverity::{Error, Warning};
    let (mut state, _c) = clocked(80, 24);
    push_all(&mut state, &[(Error, "E1"), (Error, "E2"), (Error, "E3")]);
    let unseen = state.notifications.unseen();
    state.needs_redraw = false;
    notifications::notify(&mut state, NoticeSource::System, Warning, "behind errors");
    assert_eq!(visible(&state), ["E1", "E2", "E3"], "no toast changed");
    assert_eq!(state.notifications.unseen(), unseen + 1);
    assert!(
        state.needs_redraw,
        "the badge changed, so the frame must too"
    );
}
