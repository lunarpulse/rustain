//! K8 — chords and menu truth (AC5): `Ctrl+X n` / `Ctrl+X x` are real routes in
//! the local dispatch, painted in BOTH hosts' menus, listed by help, and the
//! `/notifications` slash row is offered once in each host. Attached key
//! ingress (`handle_attached_key`) is covered by the in-file attach_keys tests.

use rustain::adapters::builtin_commands::{self, AttachedRoute, CommandHost};
use rustain::adapters::tui::app::handle_input;
use rustain::adapters::tui::keymap::render::leader_menu;
use rustain::adapters::tui::keymap::render_help::help_rows;
use rustain::adapters::tui::keymap::resolve::rows_of;
use rustain::adapters::tui::keymap::{AvailCtx, Context, Host, Key, Resolved, resolve};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;

use super::support::{clocked, notify};

#[test]
fn k8_ctrl_x_x_clears_every_toast_without_touching_sources_or_focus() {
    let (mut state, _c) = clocked(80, 24);
    state.focus = FocusState::Chat;
    notify(&mut state, NoticeSeverity::Error, "an error");
    notify(&mut state, NoticeSeverity::Info, "an info");
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    handle_input(&mut state, &DomainInputEvent::KeyPress('x'));
    assert_eq!(state.notifications.visible().count(), 0);
    assert_eq!(state.notifications.len(), 2, "the log keeps both facts");
    assert_eq!(
        state.focus,
        FocusState::Chat,
        "the leader restored the origin"
    );
    assert!(state.pending_leader.is_none());
    assert!(state.notification_log.is_none(), "x is not the log");
}

#[test]
fn k8_ctrl_x_n_opens_the_log_with_the_origin_focus_as_its_caller() {
    let (mut state, _c) = clocked(80, 24);
    state.focus = FocusState::Chat;
    notify(&mut state, NoticeSeverity::Warning, "a warning");
    handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::CtrlX));
    handle_input(&mut state, &DomainInputEvent::KeyPress('n'));
    let log = state.notification_log.as_ref().expect("S-LOG is open");
    assert_eq!(log.caller_focus, FocusState::Chat);
    assert!(state.pending_leader.is_none(), "the leader is spent");
    assert_eq!(
        state.notifications.unseen(),
        0,
        "presenting clears the badge"
    );
    assert_eq!(state.notifications.visible().count(), 1, "n never clears");
}

#[test]
fn k8_the_menu_paints_n_and_x_in_both_hosts_and_every_painted_chord_routes() {
    for (label, avail) in [
        ("Local", AvailCtx::local(130)),
        ("Attached", AvailCtx::attached(130, false)),
    ] {
        let items = leader_menu(&avail);
        let n = items.iter().find(|i| i.key == "N").expect("[N]");
        let x = items.iter().find(|i| i.key == "X").expect("[X]");
        assert_eq!(n.name, "Notifications", "{label}");
        assert_eq!(x.name, "Clear toasts", "{label}");
        assert!(
            n.available && x.available,
            "{label}: both routes exist attached"
        );
        assert_eq!(n.text(), "[N] Notifications");
    }
    // Every painted chord is a registry route in the local host's Leader context.
    for row in rows_of(Context::Leader, Host::Local).filter(|row| !row.label.is_empty()) {
        assert!(
            matches!(
                resolve(Context::Leader, Host::Local, row.keys),
                Resolved::Hit(_)
            ),
            "{} unrouted",
            row.long_label()
        );
    }
    for key in ['n', 'x', 'q'] {
        assert!(matches!(
            resolve(Context::Leader, Host::Local, &[Key::Char(key)]),
            Resolved::Hit(_)
        ));
    }
    for (label, avail) in [
        ("Local", AvailCtx::local(130)),
        ("Attached", AvailCtx::attached(130, false)),
    ] {
        let items = leader_menu(&avail);
        let q = items.iter().find(|i| i.key == "Q").expect("[Q]");
        assert_eq!(q.name, "Decisions", "{label}");
        assert!(
            q.available,
            "{label}: attached answers honestly instead of n/a"
        );
    }
}

#[test]
fn k8_help_lists_the_new_chords_and_the_slash_command() {
    let all = help_rows();
    for key in ["Ctrl+X, N", "Ctrl+X, X", "Ctrl+X, Q", "/notifications"] {
        assert!(all.iter().any(|b| b.key == key && b.available), "{key}");
    }
}

#[test]
fn k8_notifications_is_one_builtin_row_offered_in_both_hosts() {
    let rows = builtin_commands::BUILTINS
        .iter()
        .filter(|b| b.name == "notifications")
        .count();
    assert_eq!(rows, 1);
    for host in [CommandHost::Local, CommandHost::Attached] {
        assert!(
            builtin_commands::offered_in(host, "notifications"),
            "{host:?}"
        );
    }
    assert_eq!(
        builtin_commands::find("notifications").unwrap().attached,
        AttachedRoute::Offered
    );
}
