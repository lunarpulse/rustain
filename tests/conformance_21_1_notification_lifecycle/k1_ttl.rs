//! K1 — TTL from the injected clock (AC1): a notice is visible from arrival
//! until its deadline and absent from the first frame drawn at or after it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rustain::adapters::tui::notifications::{
    self, NoticeDraft, NoticeSeverity, NoticeSource, NotificationStore,
};
use rustain::domain::clock::MockClock;
use rustain::domain::models::visual::OverlayType;
use rustain::domain::models::{FocusState, NotificationsConfig};

use super::support::{advance, clocked, clocked_with, draw, notify, screen};

const MS: Duration = Duration::from_millis(1);

#[test]
fn k1_defaults_and_clamp() {
    let defaults = NotificationsConfig::default().clamped();
    assert_eq!(
        (
            defaults.info,
            defaults.success,
            defaults.warning,
            defaults.error
        ),
        (4, 4, 10, 60)
    );
    let clamp = |secs: u64| {
        NotificationsConfig {
            info_ttl_secs: secs,
            success_ttl_secs: secs,
            warning_ttl_secs: secs,
            error_ttl_secs: secs,
        }
        .clamped()
    };
    // 0 -> 1, 1 and 60 are kept, 61 -> 60 (positive controls on both ends).
    assert_eq!(clamp(0).info, 1);
    assert_eq!(clamp(1).error, 1);
    assert_eq!(clamp(60).error, 60);
    assert_eq!(clamp(61).error, 60);
    assert_eq!(clamp(u64::MAX).warning, 60);
    // The 60 s ceiling is not configurable: a TOML round trip clamps too.
    let parsed: NotificationsConfig = toml::from_str("error_ttl_secs = 3600").unwrap();
    assert_eq!(parsed.clamped().error, 60);
    assert_eq!(parsed.clamped().info, 4, "unset keys keep their defaults");
}

/// S1-7: both hosts build their store with `NotificationStore::from_config`,
/// so the parsed `[notifications]` table is what decides expiry. A TOML table
/// with `info_ttl_secs = 1` and an over-the-ceiling `error_ttl_secs = 61`
/// expires Info at exactly 1 s and Error at exactly 60 s; the other severities
/// keep their defaults.
#[test]
fn k1_the_production_constructor_applies_the_parsed_config() {
    let config: NotificationsConfig =
        toml::from_str("info_ttl_secs = 1\nerror_ttl_secs = 61").unwrap();
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut store = NotificationStore::from_config(&config, clock.clone());
    store.set_capacity(4); // one slot per severity, so no toast is displaced
    for (severity, message) in [
        (NoticeSeverity::Info, "info"),
        (NoticeSeverity::Success, "success"),
        (NoticeSeverity::Warning, "warning"),
        (NoticeSeverity::Error, "error"),
    ] {
        store.notify(NoticeDraft::new(NoticeSource::System, severity, message));
    }
    let live = |store: &NotificationStore| -> Vec<String> {
        store.visible().map(|r| r.message.clone()).collect()
    };
    assert_eq!(live(&store), ["info", "success", "warning", "error"]);
    // (offset, still visible) — 1 s info, 4 s success, 10 s warning, 60 s error.
    for (offset, expected) in [
        (
            Duration::from_secs(1) - MS,
            vec!["info", "success", "warning", "error"],
        ),
        (MS, vec!["success", "warning", "error"]),
        (Duration::from_secs(3), vec!["warning", "error"]),
        (Duration::from_secs(6), vec!["error"]),
        (Duration::from_secs(50) - MS, vec!["error"]),
        (MS, vec![]),
    ] {
        advance(&clock, offset);
        store.sweep();
        assert_eq!(live(&store), expected, "after +{offset:?}");
    }
}

#[test]
fn k1_every_severity_is_present_one_ms_before_and_absent_at_its_deadline() {
    for (severity, secs) in [
        (NoticeSeverity::Info, 4u64),
        (NoticeSeverity::Success, 4),
        (NoticeSeverity::Warning, 10),
        (NoticeSeverity::Error, 60),
    ] {
        let (mut state, clock) = clocked(80, 24);
        notify(&mut state, severity, "k1 probe message");
        let ttl = Duration::from_secs(secs);
        advance(&clock, ttl - MS);
        let before = screen(&draw(&mut state, 80, 24, true));
        assert!(
            before.contains(&format!("{} k1 probe message", severity.word())),
            "{severity:?} must still paint 1 ms before its deadline:\n{before}"
        );
        advance(&clock, MS);
        let at = screen(&draw(&mut state, 80, 24, true));
        assert!(
            !at.contains("k1 probe message"),
            "{severity:?} must be absent from the first frame at its deadline:\n{at}"
        );
        assert_eq!(state.notifications.len(), 1, "the record stays in the log");
    }
}

#[test]
fn k1_coalescing_never_renews_the_deadline() {
    let (mut state, clock) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Info, "same notice");
    advance(&clock, Duration::from_secs(2));
    notify(&mut state, NoticeSeverity::Info, "same notice");
    advance(&clock, Duration::from_secs(2) - MS);
    let before = screen(&draw(&mut state, 80, 24, true));
    assert!(before.contains("same notice ×2"), "{before}");
    advance(&clock, MS);
    let after = screen(&draw(&mut state, 80, 24, true));
    assert!(
        !after.contains("same notice"),
        "the re-arrival at +2 s must not push the first deadline (+4 s):\n{after}"
    );
}

#[test]
fn k1_configured_one_second_ttl_applies_per_severity() {
    let (mut state, clock) = clocked_with(
        80,
        24,
        NotificationsConfig {
            info_ttl_secs: 1,
            success_ttl_secs: 1,
            warning_ttl_secs: 1,
            error_ttl_secs: 1,
        },
    );
    notify(&mut state, NoticeSeverity::Warning, "one second warning");
    advance(&clock, Duration::from_secs(1) - MS);
    assert!(screen(&draw(&mut state, 80, 24, true)).contains("one second warning"));
    advance(&clock, MS);
    assert!(!screen(&draw(&mut state, 80, 24, true)).contains("one second warning"));
}

#[test]
fn k1_expiry_ignores_focus_and_overlays() {
    let (mut state, clock) = clocked(80, 24);
    state.focus = FocusState::Overlay(OverlayType::Help);
    notify(&mut state, NoticeSeverity::Info, "focus independent");
    advance(&clock, Duration::from_secs(4));
    assert!(!screen(&draw(&mut state, 80, 24, true)).contains("focus independent"));
}

#[test]
fn k1_the_tick_asks_for_a_redraw_only_after_a_deadline_passed() {
    let (mut state, clock) = clocked(80, 24);
    notify(&mut state, NoticeSeverity::Info, "tick probe");
    state.needs_redraw = false;
    advance(&clock, Duration::from_secs(4) - MS);
    notifications::tick(&mut state);
    assert!(!state.needs_redraw, "no redraw before the deadline");
    advance(&clock, MS);
    notifications::tick(&mut state);
    assert!(state.needs_redraw, "the deadline passed: redraw sweeps it");
    draw(&mut state, 80, 24, true);
    state.needs_redraw = false;
    notifications::tick(&mut state);
    assert!(
        !state.needs_redraw,
        "an expired toast keeps no redraw alive"
    );
}
