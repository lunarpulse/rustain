//! Story 21.2 — shortcut truth hotfix: cross-host conformance (K1, K5, K6, K7, K9).
//!
//! Every assertion enters through a production front door (`handle_input`,
//! `submit_message_for_test`, the shared registries and widgets) — never a
//! hand-built effect. Host-specific key ingress (attached `handle_attached_key`,
//! daemon interrupt transport) is covered by the inline `story21_2_` tests next
//! to those private items; this module pins the shared truth tables.

use rustain::adapters::builtin_commands::{self, AttachedRoute, CommandHost};
use rustain::adapters::command_registry::{CommandRegistry, CommandSource};
use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
use rustain::adapters::tui::hints::contextual_hint;
use rustain::adapters::tui::keymap::render_help::help_rows;
use rustain::adapters::tui::keymap::{AvailCtx, render::leader_menu};
use rustain::adapters::tui::presentation::{Surface, is_open};
use rustain::adapters::tui::quit_confirm::{
    self, QuitFacts, QuitHost, QuitKey, QuitKeyOutcome, QuitRequest,
};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::DensityMode;

fn key(c: char) -> DomainInputEvent {
    DomainInputEvent::KeyPress(c)
}
fn special(k: DomainKey) -> DomainInputEvent {
    DomainInputEvent::SpecialKey(k)
}

/// K1 — every offered built-in reaches its dispatcher in the local host, the
/// registry offers exactly the authority per host, and the per-host sets are
/// not the union.
#[test]
fn k1_slash_parity_per_host() {
    let registry = CommandRegistry::new();
    for host in [CommandHost::Local, CommandHost::Attached] {
        let mut offered: Vec<String> = registry
            .filter_for_host("", host)
            .into_iter()
            .filter(|c| matches!(c.source, CommandSource::BuiltIn))
            .map(|c| c.name.clone())
            .collect();
        let mut expected: Vec<String> = builtin_commands::offered_names(host)
            .map(str::to_owned)
            .collect();
        offered.sort();
        expected.sort();
        assert_eq!(offered, expected, "{host:?} discovery == authority");
    }
    for name in builtin_commands::offered_names(CommandHost::Local) {
        let mut state = TuiState::new(80, 24);
        state.input_buffer = format!("/{name} ünï");
        let action = submit_message_for_test(&mut state);
        assert!(
            matches!(&action, InputAction::ExecuteCommand { name: n, .. } if n == name),
            "/{name} offered locally but routed to {action:?}"
        );
    }
    // Attached-only /detach is never offered locally; typed login/account
    // refusals exist but are not advertised as attached sign-in.
    assert!(!builtin_commands::offered_in(CommandHost::Local, "detach"));
    for name in ["login", "models", "logout", "accounts"] {
        assert!(!builtin_commands::offered_in(CommandHost::Attached, name));
        assert_eq!(
            builtin_commands::find(name).unwrap().attached,
            AttachedRoute::TypedRefusal
        );
    }
    // `/ cmd` stays literal; an unregistered name stays a custom lookup.
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "/ quit".into();
    assert_eq!(
        submit_message_for_test(&mut state),
        InputAction::SubmitMessage("/ quit".into())
    );
    let mut state = TuiState::new(80, 24);
    state.input_buffer = "/definitely-not-registered x".into();
    assert!(matches!(
        submit_message_for_test(&mut state),
        InputAction::SubmitWithContext {
            command: Some(_),
            ..
        }
    ));
}

/// K5 — help and hints tell the current truth.
#[test]
fn k5_help_and_hints_tell_the_truth() {
    let hint = contextual_hint(
        &FocusState::Input,
        1,
        5,
        false,
        &Default::default(),
        None,
        false,
    )
    .expect("input hint");
    assert!(hint.contains("Ctrl+X ?"), "{hint}");
    let mut state = TuiState::new(80, 24);
    handle_input(&mut state, &key('?'));
    assert_eq!(state.input_buffer, "?", "bare ? is literal in Input");
    assert!(!is_open(&state, Surface::Help));
    let mut chat = TuiState::new(80, 24);
    chat.focus = FocusState::Chat;
    handle_input(&mut chat, &key('?'));
    assert!(is_open(&chat, Surface::Help), "Chat ? still opens help");

    let all = help_rows();
    let braces = all.iter().find(|b| b.key == "{ / }").expect("brace row");
    assert!(
        braces.description.contains("previous / next"),
        "{}",
        braces.description
    );
    for tab in all.iter().filter(|b| b.key.starts_with("Tab")) {
        assert!(
            tab.description.contains("sidebar") && tab.description.contains("next tab"),
            "Tab row must name the real fallback: {}",
            tab.description
        );
        // The overlay clips (never wraps) a row at its width: at the 130-column
        // native viewport 85 description columns are visible, so the fallback
        // clause must fit or the help would lie by truncation.
        assert!(
            tab.description.chars().count() <= 85,
            "Tab row would be clipped in the 130-column help overlay: {}",
            tab.description
        );
    }
    let ctrl_e = all.iter().find(|b| b.key == "Ctrl+E").expect("Ctrl+E row");
    assert_eq!(
        ctrl_e.description, "Move to the end of the current line",
        "Ctrl+E is end-of-line (not a toggle)"
    );
    let alt_m = all
        .iter()
        .find(|b| b.key.contains("Alt+M"))
        .expect("Alt+M row");
    assert!(
        alt_m.description.to_lowercase().contains("multi"),
        "{}",
        alt_m.description
    );
}

/// K6 — the leader is untimed and complete; deleted timeout symbols stay deleted.
#[test]
fn k6_leader_inventory_is_complete_untimed_and_structurally_clean() {
    for (label, avail) in [
        ("Local", AvailCtx::local(130, DensityMode::Focus)),
        ("Attached", AvailCtx::attached(130, false)),
    ] {
        let items = leader_menu(&avail);
        // `f` (Focus) and `d` (Dashboard) are retired locally; `h` is History.
        for retained in "pmiaslretuwh?nx".chars() {
            let painted = retained.to_ascii_uppercase().to_string();
            assert!(
                items.iter().any(|i| i.key == painted),
                "{label} hides retained chord {retained:?}"
            );
        }
        assert!(items.iter().any(|i| i.key == "Enter" && i.name == "Send"));
        assert!(!items.iter().any(|i| i.key == "F"), "{label} paints [F]");
        // `d` is the attached client's Detach only.
        assert_eq!(
            items.iter().any(|i| i.key == "D"),
            label == "Attached",
            "{label} Detach/Dashboard chord"
        );
    }
    let state_src = std::fs::read_to_string("src/adapters/tui/state.rs").unwrap();
    let theme_src = std::fs::read_to_string("src/adapters/tui/theme.rs").unwrap();
    let bridge_src = std::fs::read_to_string("src/infrastructure/runtime/panel_bridge.rs").unwrap();
    for (name, src) in [
        ("state.rs", &state_src),
        ("theme.rs", &theme_src),
        ("panel_bridge.rs", &bridge_src),
    ] {
        assert!(
            !src.contains("is_timed_out"),
            "{name}: timeout method survived"
        );
        assert!(
            !src.contains("which_key_timeout_ms"),
            "{name}: timeout token survived"
        );
    }
    assert!(
        !state_src.contains("pub struct WhichKeyState"),
        "the old leader owner is gone, so it cannot retain started_at"
    );
    assert!(
        state_src.contains("pending_leader: Option<"),
        "the untimed leader retains only its context, never a deadline"
    );

    // Untimed behaviour through the real key path: Esc restores the origin focus.
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    handle_input(&mut state, &special(DomainKey::CtrlX));
    assert!(state.pending_leader.is_some());
    handle_input(&mut state, &special(DomainKey::Esc));
    assert!(state.pending_leader.is_none());
    assert_eq!(state.focus, FocusState::Chat);
    // Portable send: Ctrl+X Enter sends once; Esc never submits a multiline draft.
    let mut state = TuiState::new(80, 24);
    state.multiline_mode = true;
    state.input_buffer = "a\nb".into();
    state.cursor_position = 3;
    let esc = handle_input(&mut state, &special(DomainKey::Esc));
    assert!(!matches!(esc, InputAction::SubmitMessage(_)));
    assert_eq!(state.input_buffer, "a\nb");
    state.focus = FocusState::Input;
    handle_input(&mut state, &special(DomainKey::CtrlX));
    assert_eq!(
        handle_input(&mut state, &special(DomainKey::Enter)),
        InputAction::SubmitMessage("a\nb".into())
    );
}

/// K7 — one guarded quit decision; y is owned only by the frame.
#[test]
fn k7_quit_decision_is_guarded_and_cancel_restores_the_caller() {
    let mut state = TuiState::new(80, 24);
    state.focus = FocusState::Chat;
    assert_eq!(
        quit_confirm::request(&mut state, QuitFacts::idle(QuitHost::Local)),
        QuitRequest::ExitNow
    );
    let busy = || QuitFacts {
        streams: 1,
        composer_draft: true,
        ..QuitFacts::idle(QuitHost::Local)
    };
    state.input_buffer = "unsent".into();
    assert_eq!(
        quit_confirm::request(&mut state, busy()),
        QuitRequest::Confirming
    );
    assert_eq!(
        quit_confirm::request(&mut state, busy()),
        QuitRequest::Confirming
    );
    // Unrelated keys are swallowed and edit nothing.
    assert_eq!(handle_input(&mut state, &key('x')), InputAction::Consumed);
    assert_eq!(state.input_buffer, "unsent");
    for cancel in [
        QuitKey::Char('n'),
        QuitKey::Esc,
        QuitKey::CtrlC,
        QuitKey::Enter,
    ] {
        let mut s = TuiState::new(80, 24);
        s.focus = FocusState::Chat;
        s.input_buffer = "keep".into();
        quit_confirm::request(&mut s, busy());
        assert_eq!(
            quit_confirm::handle_key(&mut s, cancel),
            QuitKeyOutcome::Cancelled
        );
        assert_eq!(s.focus, FocusState::Chat);
        assert_eq!(s.input_buffer, "keep");
    }
    assert_eq!(
        handle_input(&mut state, &key('y')),
        InputAction::QuitConfirmed
    );
}

/// K9 — Input y/e/n are text; Chat y/e/n resolve the plan; Shift+Tab maps to CycleMode.
#[test]
fn k9_plan_letters_are_text_in_input_and_decisions_in_chat() {
    use rustain::adapters::tui::state::PendingPlanCard;
    use rustain::domain::models::plan::{Plan, PlanStatus};
    let card = || PendingPlanCard {
        conversation_id: "c".into(),
        plan_id: "p".into(),
        plan_snapshot: Plan {
            id: "p".into(),
            title: "t".into(),
            tasks: vec![],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 0,
            resolved_at: None,
            host_message_id: None,
        },
    };
    let mut input = TuiState::new(80, 24);
    input.pending_plan_card = Some(card());
    for c in ['y', 'e', 'n'] {
        let action = handle_input(&mut input, &key(c));
        assert!(
            !matches!(
                action,
                InputAction::PlanCardApprove
                    | InputAction::PlanCardEdit
                    | InputAction::PlanCardReject
            ),
            "{action:?}"
        );
    }
    assert_eq!(input.input_buffer, "yen");
    assert!(input.pending_plan_card.is_some());
    let mut chat = TuiState::new(80, 24);
    chat.focus = FocusState::Chat;
    chat.pending_plan_card = Some(card());
    assert_eq!(
        handle_input(&mut chat, &key('y')),
        InputAction::PlanCardApprove
    );
    assert_eq!(
        handle_input(&mut chat, &key('e')),
        InputAction::PlanCardEdit
    );
    assert_eq!(
        handle_input(&mut chat, &key('n')),
        InputAction::PlanCardReject
    );
    let mut state = TuiState::new(80, 24);
    assert_eq!(
        handle_input(&mut state, &special(DomainKey::ShiftTab)),
        InputAction::CycleMode
    );
}
