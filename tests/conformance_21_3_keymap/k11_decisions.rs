//! K11 — decision surfaces through the local front door (Story 21.3 AC15-AC17).
//! The migrated cards are raised through producers and exercised through the real
//! decision route, including deferral, input isolation and by-value effects.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, DecisionEffect, DecisionKind, DecisionSubject, Validity,
};
use rustain::adapters::tui::keymap::{Context, Host, Key, Resolved, resolve};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::FocusState;
use tokio::sync::oneshot::error::TryRecvError;

use super::support::feedback;
use super::support::{is_inert, permission, permission_payload, press as press_base};
use crate::conformance_21_6_decision_queue::raisers::{
    self, Hold, PLAIN, open_ready, press, repaint_ready,
};
use crate::conformance_21_11_display_width::k5b_cards::peer_pending;

fn ch(c: char) -> Key {
    Key::Char(c)
}

/// One card that is a decision frame (21-6 slice 6-C): its kind, the context that owns
/// its keys, and every RESOLVING key by value. `keeps` lists the rows whose effect only
/// opens a sub-context (the item stays queued).
struct FrameCard {
    name: &'static str,
    kind: DecisionKind,
    context: Context,
    rows: Vec<(Key, InputAction)>,
    keeps: Vec<Key>,
}

fn effect(effect: DecisionEffect) -> InputAction {
    InputAction::Decision(effect)
}

fn frame_cards() -> Vec<FrameCard> {
    vec![
        FrameCard {
            name: "consolidation",
            kind: DecisionKind::Consolidation,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), InputAction::ConsolidateAcceptAll),
                (ch('n'), InputAction::ConsolidateDeclineAll),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "forget",
            kind: DecisionKind::Forget,
            context: Context::Forget,
            rows: vec![
                (ch('y'), InputAction::ForgetAcceptAll),
                (ch('n'), InputAction::ForgetDeclineAll),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "artifact apply",
            kind: DecisionKind::ArtifactApply,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), InputAction::ApplyCardAccept),
                (ch('n'), InputAction::ApplyCardDecline),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "artifact resolve",
            kind: DecisionKind::ArtifactResolve,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), InputAction::ApplyCardAccept),
                (ch('n'), InputAction::ApplyCardDecline),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "peer add",
            kind: DecisionKind::PeerAdd,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), InputAction::PeerAddConfirm),
                (ch('n'), InputAction::PeerAddDecline),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "team retract",
            kind: DecisionKind::TeamRetract,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), InputAction::TeamRetractConfirm),
                (ch('n'), InputAction::TeamRetractDecline),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "fork",
            kind: DecisionKind::Fork,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), InputAction::ForkConfirm),
                (ch('n'), InputAction::ForkCancel),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "rewind",
            kind: DecisionKind::Rewind,
            context: Context::Rewind,
            rows: vec![
                (ch('y'), InputAction::RewindConfirm),
                (ch('f'), InputAction::RewindForkInstead),
                (ch('n'), InputAction::RewindCancel),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "export overwrite",
            kind: DecisionKind::ExportOverwrite,
            context: Context::ExportOverwrite,
            rows: vec![
                (ch('y'), InputAction::ConfirmExportOverwrite),
                (ch('n'), InputAction::CancelExportOverwrite),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "skill trust",
            kind: DecisionKind::SkillTrust,
            context: Context::SkillTrust,
            rows: vec![
                (ch('y'), InputAction::SkillTrustAccept),
                (ch('n'), InputAction::SkillTrustDecline),
                (ch('i'), InputAction::SkillTrustInspect),
            ],
            keeps: vec![ch('i')],
        },
        FrameCard {
            name: "plan approval",
            kind: DecisionKind::PlanApproval,
            context: Context::PlanApproval,
            rows: vec![
                (ch('y'), InputAction::PlanApproveNormal),
                (ch('a'), InputAction::PlanApproveAutoEdit),
                (ch('n'), InputAction::PlanReject),
                (ch('e'), InputAction::PlanRevise),
            ],
            keeps: vec![ch('e')],
        },
        FrameCard {
            name: "plan card",
            kind: DecisionKind::PlanCard,
            context: Context::PlanDecision,
            rows: vec![
                (ch('y'), InputAction::PlanCardApprove),
                (ch('e'), InputAction::PlanCardEdit),
                (ch('n'), InputAction::PlanCardReject),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "plan deviation",
            kind: DecisionKind::PlanDeviation,
            context: Context::PlanDecision,
            rows: vec![
                (ch('y'), effect(DecisionEffect::PlanDeviationApprove)),
                (ch('e'), effect(DecisionEffect::PlanDeviationEdit)),
                (ch('n'), effect(DecisionEffect::PlanDeviationReject)),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "cancel plan",
            kind: DecisionKind::CancelPlan,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), effect(DecisionEffect::CancelPlanConfirm)),
                (ch('n'), effect(DecisionEffect::CancelPlanDecline)),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "skip cascade",
            kind: DecisionKind::SkipCascade,
            context: Context::SkipCascade,
            rows: vec![
                (ch('s'), effect(DecisionEffect::SkipCascadeSkip)),
                // A19 re-key: Continue anyway moved from `c` (Copy now) to `a`.
                (ch('a'), effect(DecisionEffect::SkipCascadeContinue)),
                (ch('n'), effect(DecisionEffect::SkipCascadeCancel)),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "delegation",
            kind: DecisionKind::Delegation,
            context: Context::Delegation,
            rows: vec![
                (ch('d'), InputAction::DelegateTask),
                (ch('l'), InputAction::RunTaskLocally),
                (ch('x'), InputAction::DelegationCardCancel),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "spawn gate",
            kind: DecisionKind::SpawnGate,
            context: Context::SpawnGate,
            rows: vec![
                (Key::Enter, InputAction::SpawnGateConfirm),
                (ch('n'), effect(DecisionEffect::SpawnGateCancel)),
            ],
            keeps: vec![],
        },
        FrameCard {
            name: "agent kill",
            kind: DecisionKind::AgentKill,
            context: Context::DecisionConfirm,
            rows: vec![
                (ch('y'), effect(DecisionEffect::AgentKillConfirm)),
                (ch('n'), effect(DecisionEffect::AgentKillDecline)),
            ],
            keeps: vec![],
        },
    ]
}

fn cue_records(state: &TuiState) -> Vec<String> {
    state
        .notifications
        .records()
        .filter(|r| r.severity == NoticeSeverity::Info)
        .map(|r| r.message.clone())
        .collect()
}

fn first_item(state: &TuiState, kind: DecisionKind) -> decision_queue::DecisionKey {
    let item = state
        .decisions
        .items()
        .find(|item| item.kind == kind)
        .unwrap_or_else(|| panic!("{kind:?} is queued"));
    decision_queue::DecisionKey {
        kind: item.kind,
        source: item.source.clone(),
        conversation_id: item.conversation_id.clone(),
    }
}

/// A delegation and a deviation are both queued: the queue is FIFO, so the delegation
/// (older) owns the frame, `Esc` defers it without deciding either, and the deviation
/// then decides its own item. (Before 6-C the two inline cards shared one slot order.)
#[test]
fn delegation_and_deviation_queue_in_order_and_decide_their_own_item() {
    let (mut state, clock, _hold) = open_ready(DecisionKind::Delegation, "reviewer");
    raisers::raise(&mut state, DecisionKind::PlanDeviation, PLAIN);
    assert_eq!(state.decisions.undecided(), 2, "both are pending");
    assert!(
        screen_of(&mut state).contains("[d] Delegate"),
        "the older delegation owns the frame"
    );
    // Esc defers the delegation: nothing was decided, both still wait.
    assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
    assert_eq!(state.decisions.undecided(), 2);
    assert!(
        state
            .decisions
            .items()
            .any(|i| i.kind == DecisionKind::Delegation && i.deferred)
    );
    // Open the deviation: its own `y` decides it, and only it.
    let key = first_item(&state, DecisionKind::PlanDeviation);
    assert!(decision_queue::open(&mut state, &key));
    repaint_ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, ch('y')),
        effect(DecisionEffect::PlanDeviationApprove)
    );
    assert!(state.decisions.holds(DecisionKind::Delegation));
    assert!(!state.decisions.holds(DecisionKind::PlanDeviation));
}

fn screen_of(state: &mut TuiState) -> String {
    crate::conformance_21_6_decision_queue::support::screen(
        &crate::conformance_21_6_decision_queue::support::paint(
            state,
            &raisers::facts_6d("c1"),
            80,
            24,
        ),
    )
}

/// Queued consolidation cannot steal the active deviation frame's answer.
#[test]
fn consolidation_and_deviation_decide_their_own_card_the_frame_first() {
    let (mut state, clock, _hold) = open_ready(DecisionKind::PlanDeviation, PLAIN);
    raisers::raise(&mut state, DecisionKind::Consolidation, PLAIN);
    assert_eq!(
        press(&mut state, ch('y')),
        effect(DecisionEffect::PlanDeviationApprove),
        "the frame decides the deviation, not the consolidation card"
    );
    assert!(
        state.decisions.holds(DecisionKind::Consolidation),
        "still pending"
    );
    let key = first_item(&state, DecisionKind::Consolidation);
    assert!(decision_queue::open(&mut state, &key));
    repaint_ready(&mut state, &clock);
    assert_eq!(
        press(&mut state, ch('y')),
        InputAction::ConsolidateAcceptAll,
        "with the frame gone the consolidation card decides"
    );
}

#[test]
fn drafted_inline_cards_that_swallow_i_preserve_the_draft_until_a_decision() {
    for card in frame_cards().into_iter().filter(|c| {
        [
            "spawn gate",
            "plan deviation",
            "cancel plan",
            "skip cascade",
            "consolidation",
            "forget",
        ]
        .contains(&c.name)
    }) {
        let (mut state, _clock, _hold) = open_ready(card.kind, PLAIN);
        state.input_buffer = "preserved draft".into();
        state.focus = FocusState::Chat;
        assert!(is_inert(&press(&mut state, ch('i'))), "{}", card.name);
        assert_eq!(state.focus, FocusState::Chat, "{}", card.name);
        assert_eq!(state.input_buffer, "preserved draft", "{}", card.name);
        assert!(state.decisions.holds(card.kind), "{}", card.name);
    }
}

#[test]
fn every_frame_key_decides_once_by_value() {
    for card in frame_cards() {
        for (key, want) in &card.rows {
            let (mut state, _clock, _hold) = open_ready(card.kind, PLAIN);
            assert_eq!(press(&mut state, *key), *want, "{}: {key:?}", card.name);
            if card.keeps.contains(key) {
                assert!(
                    state.decisions.holds(card.kind),
                    "{}: {key:?} opens a sub-context and keeps the item",
                    card.name
                );
                continue;
            }
            assert!(
                !state.decisions.holds(card.kind),
                "{}: {key:?} resolved the item",
                card.name
            );
            // Exactly once: the key finds nothing to answer the second time.
            assert_ne!(
                press(&mut state, *key),
                *want,
                "{}: {key:?} again",
                card.name
            );
        }
    }
}
#[test]
fn export_shifted_letters_never_decide_at_the_raw_boundary() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use rustain::adapters::tui::app::handle_operator_input;
    use rustain::domain::events::DomainInputEvent;
    for (letter, positive) in [
        ('Y', InputAction::ConfirmExportOverwrite),
        ('N', InputAction::CancelExportOverwrite),
    ] {
        let (mut state, _clock, _hold) = open_ready(DecisionKind::ExportOverwrite, PLAIN);
        let raw = Event::Key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::SHIFT));
        assert_eq!(
            handle_operator_input(&mut state, &raw, &DomainInputEvent::KeyPress(letter)),
            None
        );
        assert!(state.decisions.holds(DecisionKind::ExportOverwrite));
        assert_eq!(press(&mut state, ch(letter.to_ascii_lowercase())), positive);
    }
}

type RailCase<'a> = (DecisionKind, &'a str, &'a [(&'a str, &'a str)]);

#[test]
fn migrated_decision_titles_and_rails_are_exact() {
    use DecisionKind::*;
    let cases: &[RailCase<'_>] = &[
        (
            Consolidation,
            "Decision: Memory consolidation",
            &[
                ("[y]", "promote all"),
                ("[n]", "decline"),
                ("[Esc]", "Later"),
            ],
        ),
        (
            Forget,
            "Decision: Forget memory",
            &[
                ("[↑/↓]", "navigate"),
                ("[Space]", "toggle"),
                ("[y]", "forget selected"),
                ("[n]", "cancel"),
                ("[Esc]", "Later"),
            ],
        ),
        (
            ArtifactApply,
            "Decision: Apply artifact",
            &[("[y]", "Apply"), ("[n]", "Cancel"), ("[Esc]", "Later")],
        ),
        (
            ArtifactResolve,
            "Decision: Resolve artifact",
            &[
                ("[y]", "Record report"),
                ("[n]", "Cancel"),
                ("[Esc]", "Later"),
            ],
        ),
        (
            PeerAdd,
            "Decision: Add peer",
            &[
                ("[y]", "Confirm and pin"),
                ("[n]", "Cancel"),
                ("[Esc]", "Later"),
            ],
        ),
        (
            TeamRetract,
            "Decision: Retract",
            &[("[y]", "Retract"), ("[n]", "Cancel"), ("[Esc]", "Later")],
        ),
        (
            Fork,
            "Decision: Fork",
            &[("[y]", "Fork"), ("[n]", "Cancel"), ("[Esc]", "Later")],
        ),
        (
            Rewind,
            "Decision: Rewind",
            &[
                ("[y]", "Rewind"),
                ("[f]", "Fork instead"),
                ("[n]", "Cancel"),
                ("[Esc]", "Later"),
            ],
        ),
        (
            ExportOverwrite,
            "Decision: Export overwrite",
            &[("[y]", "Overwrite"), ("[n]", "Cancel"), ("[Esc]", "Later")],
        ),
    ];
    for &(kind, title, expected) in cases {
        let (mut state, _clock, _hold) = open_ready(kind, PLAIN);
        assert_eq!(kind.frame_title(), title);
        let item = state
            .decisions
            .items()
            .find(|item| item.kind == kind)
            .unwrap();
        let actual = decision_queue::rail_of(item);
        assert_eq!(
            actual
                .iter()
                .map(|(key, label)| (key.as_str(), *label))
                .collect::<Vec<_>>(),
            expected,
            "{kind:?}"
        );
        assert!(screen_of(&mut state).contains(title), "{kind:?}");
    }
}

#[test]
fn disarmed_retract_offers_only_cancel_and_cannot_confirm() {
    let (mut state, clock) = crate::conformance_21_6_decision_queue::support::clocked(80, 24);
    decision_queue::raise_team_retract(
        &mut state,
        raisers::retract(PLAIN, false),
        decision_queue::Responder::Local,
    );
    repaint_ready(&mut state, &clock);
    let item = state
        .decisions
        .items()
        .find(|item| item.kind == DecisionKind::TeamRetract)
        .unwrap();
    let rail = decision_queue::rail_of(item);
    assert_eq!(
        rail.iter()
            .map(|(key, label)| (key.as_str(), *label))
            .collect::<Vec<_>>(),
        vec![("[n]", "Cancel"), ("[Esc]", "Later")]
    );
    assert!(is_inert(&press(&mut state, ch('y'))));
    assert!(state.decisions.holds(DecisionKind::TeamRetract));
    assert_eq!(press(&mut state, ch('n')), InputAction::TeamRetractDecline);
}

/// 6-C: on a frame card `Esc` defers and `Ctrl+C` backs one frame (which defers): neither
/// decides, neither posts the 21-3 cue, the item stays queued, counted and answerable.
#[test]
fn esc_and_ctrl_c_defer_and_never_decide_a_frame_card() {
    for card in frame_cards() {
        for key in [Key::Esc, Key::Ctrl('c')] {
            let (mut state, clock, _hold) = open_ready(card.kind, PLAIN);
            let action = press(&mut state, key);
            assert_eq!(action, InputAction::Consumed, "{}: {key:?}", card.name);
            assert!(
                !presentation::is_open(&state, Surface::Decision),
                "{}: {key:?} closed the frame",
                card.name
            );
            assert_eq!(
                decision_queue::waiting(&state),
                1,
                "{}: {key:?} keeps it queued and counted",
                card.name
            );
            assert!(
                state.decisions.items().all(|item| item.deferred),
                "{}: {key:?}",
                card.name
            );
            assert!(
                cue_records(&state)
                    .iter()
                    .all(|m| !m.contains("never decide")),
                "{}: a frame card posts no 21-3 cue",
                card.name
            );
            // Positive control: reopened, the explicit key still decides, once.
            let item = first_item(&state, card.kind);
            assert!(decision_queue::open(&mut state, &item));
            repaint_ready(&mut state, &clock);
            let (first, want) = &card.rows[0];
            assert_eq!(
                press(&mut state, *first),
                *want,
                "{}: answerable after {key:?}",
                card.name
            );
        }
    }
}

#[test]
fn typing_in_input_never_answers_a_card() {
    // A frame card the user set aside (Esc) leaves Input a plain composer: its rail keys
    // are text, and the deferred item stays pending.
    for card in frame_cards() {
        for (key, answer) in &card.rows {
            let Key::Char(c) = key else { continue };
            let (mut state, _clock, _hold) = open_ready(card.kind, PLAIN);
            assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
            state.focus = FocusState::Input;
            let action = press_base(&mut state, *key);
            assert_ne!(&action, answer, "{}: {c:?} answered from Input", card.name);
            assert_eq!(state.input_buffer, c.to_string(), "{}: {c:?}", card.name);
            assert!(state.decisions.holds(card.kind), "{}: {c:?}", card.name);
        }
    }
}

#[test]
fn a_card_swallows_every_key_it_has_no_row_for() {
    // The frame cards: every key their context has NO row for (own or inherited) is
    // swallowed with nothing typed and the item untouched.
    for card in frame_cards() {
        let mut swept = 0;
        for key in Key::universe() {
            if !matches!(resolve(card.context, Host::Local, &[key]), Resolved::Miss) {
                continue;
            }
            let (mut state, _clock, _hold) = open_ready(card.kind, PLAIN);
            let before = (
                state.decisions.len(),
                state.focus.clone(),
                state.input_buffer.clone(),
                presentation::is_open(&state, Surface::Decision),
            );
            let action = press(&mut state, key);
            assert!(
                is_inert(&action),
                "{}: {key:?} has no row but returned {action:?}",
                card.name
            );
            assert_eq!(
                (
                    state.decisions.len(),
                    state.focus.clone(),
                    state.input_buffer.clone(),
                    presentation::is_open(&state, Surface::Decision),
                ),
                before,
                "{}: {key:?} changed the card state",
                card.name
            );
            swept += 1;
        }
        assert!(swept > 120, "{}: the sweep covered {swept} keys", card.name);
    }
    // Positive control: a key WITH a row visibly changes the compared state.
    let (mut state, _clock, _hold) = open_ready(DecisionKind::PlanCard, PLAIN);
    let before = state.decisions.len();
    press(&mut state, ch('n'));
    assert_ne!(state.decisions.len(), before);
}

fn question_text(state: &TuiState) -> String {
    state
        .decisions
        .items()
        .find_map(|item| match &item.subject {
            DecisionSubject::Question(payload) => Some(payload.field.as_str().to_owned()),
            _ => None,
        })
        .expect("a question is queued")
}

/// `Ctrl+D` skips the question (its old `Esc` effect, explicit); `Esc` leaves the field and
/// then defers (the oneshot stays alive, the turn keeps waiting); the answer field types
/// and `Enter` sends.
#[test]
fn ctrl_d_skips_the_question_and_esc_never_does() {
    let (mut state, _clock, hold) = open_ready(DecisionKind::Question, "q?");
    let Hold::Question(mut rx) = hold else {
        panic!("a question holds its receiver")
    };
    for key in [Key::Esc, Key::Ctrl('c')] {
        let (mut s, _c, h) = open_ready(DecisionKind::Question, "q?");
        let Hold::Question(mut r) = h else { panic!() };
        assert_eq!(
            press(&mut s, key),
            InputAction::Consumed,
            "{key:?} leaves the field"
        );
        assert!(
            presentation::is_open(&s, Surface::Decision),
            "{key:?}: first press backs"
        );
        assert_eq!(press(&mut s, key), InputAction::Consumed, "{key:?} defers");
        assert!(!presentation::is_open(&s, Surface::Decision));
        assert_eq!(decision_queue::waiting(&s), 1);
        assert_eq!(
            r.try_recv(),
            Err(TryRecvError::Empty),
            "{key:?} must not skip"
        );
    }
    assert_eq!(press(&mut state, ch('h')), InputAction::Consumed);
    assert_eq!(question_text(&state), "h");
    assert_eq!(
        press(&mut state, Key::Enter),
        effect(DecisionEffect::QuestionSend)
    );
    assert_eq!(decision_queue::waiting(&state), 0);
    decision_queue::apply_resolved_action(&mut state, effect(DecisionEffect::QuestionSend));
    assert_eq!(
        rx.try_recv(),
        Ok("h".to_string()),
        "the answer reached the turn once"
    );
    let (mut skip, _c, h) = open_ready(DecisionKind::Question, "q?");
    let Hold::Question(mut r) = h else { panic!() };
    assert_eq!(
        press(&mut skip, Key::Ctrl('d')),
        effect(DecisionEffect::QuestionSkip),
        "Ctrl+D skips"
    );
    assert_eq!(decision_queue::waiting(&skip), 0);
    // The skip itself drops the reply when the host applies it.
    decision_queue::apply_resolved_action(&mut skip, effect(DecisionEffect::QuestionSkip));
    assert_eq!(
        r.try_recv(),
        Err(TryRecvError::Closed),
        "Ctrl+D dropped the sender"
    );
    // After leaving the answer field (QuestionIdle) Ctrl+D still skips, exactly once.
    let (mut idle, _c, h) = open_ready(DecisionKind::Question, "q?");
    let Hold::Question(mut r) = h else { panic!() };
    assert_eq!(press(&mut idle, Key::Esc), InputAction::Consumed);
    assert!(
        presentation::is_open(&idle, Surface::Decision),
        "field left, frame open"
    );
    assert_eq!(
        press(&mut idle, Key::Ctrl('d')),
        effect(DecisionEffect::QuestionSkip),
        "Ctrl+D skips from the idle question"
    );
    decision_queue::apply_resolved_action(&mut idle, effect(DecisionEffect::QuestionSkip));
    assert_eq!(
        r.try_recv(),
        Err(TryRecvError::Closed),
        "the receiver is Closed"
    );
    assert_eq!(decision_queue::waiting(&idle), 0);
    assert_ne!(
        press(&mut idle, Key::Ctrl('d')),
        effect(DecisionEffect::QuestionSkip),
        "a second Ctrl+D skips nothing"
    );
}

/// Sub-frame backs stay backs (LR-3): the skill inspect view returns to the trust item.
#[test]
fn sub_frame_backs_stay_backs() {
    for back in [Key::Esc, Key::Ctrl('c')] {
        let (mut state, _clock, _hold) = open_ready(DecisionKind::SkillTrust, "skill");
        assert_eq!(press(&mut state, ch('i')), InputAction::SkillTrustInspect);
        let key = first_item(&state, DecisionKind::SkillTrust);
        decision_queue::open_inspect(&mut state, &key, "line one\nline two".into());
        assert_eq!(press(&mut state, back), InputAction::Consumed);
        assert!(
            presentation::is_open(&state, Surface::Decision),
            "{back:?}: back, not close"
        );
        let item = state.decisions.items().next().expect("still queued");
        assert!(
            !item.deferred,
            "{back:?}: the item is undecided and undeferred"
        );
        assert!(matches!(item.validity, Validity::Live));
        match &item.subject {
            DecisionSubject::SkillTrust(payload) => assert!(!payload.inspecting, "{back:?}"),
            other => panic!("{other:?}"),
        }
        assert!(cue_records(&state).is_empty(), "a back posts no cue");
    }
}

/// The forget card's arrows move its focus.
#[test]
fn forget_arrows_move_the_focus() {
    let focused = |state: &TuiState| {
        state
            .decisions
            .items()
            .find_map(|item| match &item.subject {
                DecisionSubject::Forget(card) => Some((card.focused_index, card.selected_keys())),
                _ => None,
            })
            .expect("forget item")
    };
    let (mut state, _clock, _hold) = open_ready(DecisionKind::Forget, PLAIN);
    let before = focused(&state);
    assert!(is_inert(&press(&mut state, Key::Down)));
    assert_eq!(focused(&state).0, 1);
    assert!(is_inert(&press(&mut state, ch(' '))));
    assert_ne!(focused(&state).1, before.1);
    assert!(is_inert(&press(&mut state, Key::Up)));
    assert_eq!(focused(&state).0, 0);
    assert!(is_inert(&press(&mut state, ch('j'))));
    assert_eq!(focused(&state).0, 1);
    assert!(is_inert(&press(&mut state, ch('k'))));
    assert_eq!(focused(&state).0, 0);
    assert_eq!(press(&mut state, ch('n')), InputAction::ForgetDeclineAll);
}

/// The spawn gate's non-resolving rows edit the queued payload and keep the item.
#[test]
fn spawn_gate_cap_and_adjust_edit_the_queued_count_and_keep_the_item() {
    let adjusted = |state: &TuiState| -> Option<usize> {
        state
            .decisions
            .items()
            .find_map(|item| match &item.subject {
                DecisionSubject::SpawnGate(gate) => Some(gate.adjusted),
                _ => None,
            })
            .expect("the gate is queued")
    };
    let (mut state, _clock, _hold) = open_ready(DecisionKind::SpawnGate, PLAIN);
    assert_eq!(adjusted(&state), None);
    // A19 re-key: `t` caps at the threshold (2); `c` is Copy and never caps.
    assert!(is_inert(&press(&mut state, ch('t'))));
    assert_eq!(adjusted(&state), Some(2));
    assert!(is_inert(&press(&mut state, Key::Right)));
    assert_eq!(adjusted(&state), Some(3));
    assert!(is_inert(&press(&mut state, Key::Left)));
    assert!(is_inert(&press(&mut state, Key::Left)));
    assert_eq!(adjusted(&state), Some(1));
    assert!(
        state.decisions.holds(DecisionKind::SpawnGate),
        "nothing resolved"
    );
    let (mut copy, _c, _h) = open_ready(DecisionKind::SpawnGate, PLAIN);
    let _ = press(&mut copy, ch('c'));
    assert_eq!(adjusted(&copy), None, "`c` no longer caps");
}

/// The peer-add card body is shared with the CLI (which may not import the keymap), so its
/// `[y]` / `[n]` are hand-written there: they must still be the `Confirm` rows.
#[test]
fn the_shared_peer_add_card_copy_names_the_confirm_rows() {
    use rustain::adapters::tui::keymap::render::rail;
    use rustain::adapters::tui::keymap::table_card::CardCmd;
    let card = peer_pending("jun", None).card;
    let line = format!(
        "{} Confirm and pin  {} Cancel",
        rail(Context::DecisionConfirm, CardCmd::Yes),
        rail(Context::DecisionConfirm, CardCmd::No)
    );
    assert!(card.contains(&line), "{card}");
}

/// The proposal inherits the shared frame's Back/Defer keys on both hosts.
/// The by-value local witness is `story21_6_local_team_proposal_rows_decide_once_and_back_defers`.
#[test]
fn story21_6_team_proposal_inherits_decision_back_and_defer() {
    use rustain::adapters::tui::keymap::model::CommandId;
    use rustain::adapters::tui::keymap::table_overlay::OverlayCmd;
    for host in [Host::Local, Host::Attached] {
        for (key, expected) in [
            (Key::Esc, OverlayCmd::PaneEsc),
            (Key::Ctrl('c'), OverlayCmd::PaneBack),
        ] {
            match resolve(Context::TeamProposal, host, &[key]) {
                Resolved::Hit(row) => assert_eq!(row.command, CommandId::Overlay(expected)),
                other => panic!("{host:?} {key:?}: {other:?}"),
            }
        }
    }
}

/// Feedback edits in the frame; Back never decides or posts a cue, Send resolves once.
#[test]
fn the_feedback_field_types_erases_sends_and_backs_out() {
    for back in [Key::Esc, Key::Ctrl('c')] {
        let mut state = permission();
        assert_eq!(press_base(&mut state, ch('f')), InputAction::Consumed);
        assert_eq!(press_base(&mut state, ch('h')), InputAction::Consumed);
        assert_eq!(feedback(&state).as_str(), "h");
        assert_eq!(
            press_base(&mut state, Key::Backspace),
            InputAction::Consumed
        );
        assert_eq!(feedback(&state).as_str(), "");
        assert_eq!(press_base(&mut state, back), InputAction::Consumed);
        assert!(permission_payload(&state).feedback.is_none());
        assert_eq!(state.decisions.len(), 1);
        assert!(cue_records(&state).is_empty(), "a back posts no cue");
    }
    let mut state = permission();
    press_base(&mut state, ch('f'));
    press_base(&mut state, ch('h'));
    assert_eq!(
        press_base(&mut state, Key::Enter),
        InputAction::FeedbackInputSubmit
    );
    assert!(state.decisions.is_empty());
}

/// The question answer erases its last character (the field's readline `Backspace`), and
/// Enter on the closed field opens it again.
#[test]
fn the_question_answer_erases_its_last_character() {
    let (mut state, _clock, _hold) = open_ready(DecisionKind::Question, "q?");
    press(&mut state, ch('h'));
    press(&mut state, ch('i'));
    assert_eq!(question_text(&state), "hi");
    assert_eq!(press(&mut state, Key::Backspace), InputAction::Consumed);
    assert_eq!(question_text(&state), "h");
    // Esc closes the field (Enter then answers = opens it); the text stays.
    assert_eq!(press(&mut state, Key::Esc), InputAction::Consumed);
    assert_eq!(
        press(&mut state, Key::Enter),
        InputAction::Consumed,
        "Enter opens the field"
    );
    assert_eq!(question_text(&state), "h");
    assert_eq!(
        press(&mut state, Key::Enter),
        effect(DecisionEffect::QuestionSend),
        "and Enter in the open field sends"
    );
}

/// 6-C: a drill-down or a reorder in Chat is not touched by a deferred frame: `Esc` defers
/// the delegation frame (the chat sub-state stays), and the NEXT `Esc` goes back from the
/// drill-down or reorder as it always did, posting no cue.
#[test]
fn esc_goes_back_from_a_drill_down_or_reorder_after_the_frame_defers() {
    for (what, drill, reorder, back) in [
        ("drill-down", Some(3u32), None, InputAction::Consumed),
        ("reorder", None, Some(2u32), InputAction::TaskReorderCancel),
    ] {
        let (mut state, _clock, _hold) = open_ready(DecisionKind::Delegation, "reviewer");
        state.focus = FocusState::Chat;
        state.task_panel_state.drill_down_task = drill;
        state.task_panel_state.reorder_mode_for = reorder;
        assert_eq!(
            press(&mut state, Key::Esc),
            InputAction::Consumed,
            "{what}: Esc defers the frame"
        );
        assert_eq!(
            state.task_panel_state.drill_down_task, drill,
            "{what}: untouched"
        );
        assert_eq!(
            state.task_panel_state.reorder_mode_for, reorder,
            "{what}: untouched"
        );
        assert!(state.decisions.holds(DecisionKind::Delegation), "{what}");
        assert_eq!(
            press_base(&mut state, Key::Esc),
            back,
            "{what}: Esc goes back"
        );
        assert!(
            cue_records(&state).is_empty(),
            "{what}: a back posts no cue"
        );
    }
}

/// Every local row of `table_card` has a by-value witness: a `frame_cards()`
/// row, or one of the named tests below. A row added to the table without one fails here.
/// (`Consent` is witnessed in the attached tests.)
#[test]
fn every_table_card_row_has_a_witness() {
    use rustain::adapters::tui::keymap::table_card;
    let mut witnessed: Vec<(Context, Key)> = Vec::new();
    for card in frame_cards() {
        witnessed.extend(card.rows.iter().map(|(key, _)| (card.context, *key)));
    }
    witnessed.extend([
        // Team proposal frame witnesses in 21-6 K12 and the 19-21b lifecycle tests.
        (Context::TeamProposal, Key::Char('y')),
        (Context::TeamProposal, Key::Char('r')),
        (Context::TeamProposal, Key::Char('n')),
        // 6-E K13 `story21_6_ticket_rails_and_effects_are_distinct` (Enter/d/x by effect) and
        // `story21_6_ticket_selection_and_edit_keys_change_the_form` (Up/Down/Left/Right/e).
        (Context::Ticket, Key::Enter),
        (Context::Ticket, Key::Char('d')),
        (Context::Ticket, Key::Char('x')),
        (Context::Ticket, Key::Up),
        (Context::Ticket, Key::Down),
        (Context::Ticket, Key::Left),
        (Context::Ticket, Key::Right),
        (Context::Ticket, Key::Char('e')),
        // 6-E K14 `story21_6_reply_leader_lists_and_enter_returns_read_effect` (Enter) and
        // `story21_6_reply_dismiss_is_stable_until_a_new_count` (x).
        (Context::ReplyArrived, Key::Enter),
        (Context::ReplyArrived, Key::Char('x')),
        // `the_feedback_field_types_erases_sends_and_backs_out`, `the_question_*`.
        (Context::DecisionCompose, Key::Enter),
        (Context::DecisionCompose, Key::Esc),
        (Context::DecisionCompose, Key::Ctrl('c')),
        // `ctrl_d_skips_the_question_and_esc_never_does` (open field AND, after one Esc,
        // idle) and K7's `ctrl_d_after_leaving_the_answer_field_skips_once_and_closes_the_receiver`
        // / `question_and_idle_question_deciding_keys_need_a_plain_press` (raw Press only).
        (Context::Question, Key::Ctrl('d')),
        (Context::QuestionIdle, Key::Enter),
        (Context::QuestionIdle, Key::Ctrl('d')),
        // `sub_frame_backs_stay_backs`, `skill_inspect_scrolls_the_whole_file` (arrows AND
        // `j`/`k`), and the in-file `..._skill_inspect_j_and_k_scroll_to_the_same_rows_as_the_arrows`.
        (Context::SkillTrustInspect, Key::Esc),
        (Context::SkillTrustInspect, Key::Ctrl('c')),
        (Context::SkillTrustInspect, Key::Up),
        (Context::SkillTrustInspect, Key::Char('k')),
        (Context::SkillTrustInspect, Key::Down),
        (Context::SkillTrustInspect, Key::Char('j')),
        // `spawn_gate_cap_and_adjust_edit_the_queued_count_and_keep_the_item`.
        (Context::SpawnGate, Key::Char('t')),
        (Context::SpawnGate, Key::Left),
        (Context::SpawnGate, Key::Right),
        // `forget_arrows_move_the_focus`.
        (Context::Forget, Key::Down),
        (Context::Forget, Key::Char(' ')),
        (Context::Forget, Key::Char('j')),
        (Context::Forget, Key::Char('k')),
        (Context::Forget, Key::Up),
    ]);
    let unwitnessed: Vec<String> = table_card::ROWS
        .iter()
        .filter(|row| row.hosts.includes(Host::Local))
        .filter(|row| row.context != Context::Consent)
        .filter(|row| !witnessed.contains(&(row.context, row.keys[0])))
        .map(|row| format!("{:?} {:?}", row.context, row.keys))
        .collect();
    assert!(
        unwitnessed.is_empty(),
        "rows without a witness: {unwitnessed:?}"
    );
}

/// RU-25: the skill reader scrolls the WHOLE file (the 20-line cap is gone).
#[test]
fn skill_inspect_scrolls_the_whole_file() {
    use crate::conformance_21_6_decision_queue::support::{facts, paint, screen};
    let (mut state, clock, _hold) = open_ready(DecisionKind::SkillTrust, "big-skill");
    let key = first_item(&state, DecisionKind::SkillTrust);
    let body: String = (1..=120).map(|n| format!("skill line {n:03}\n")).collect();
    decision_queue::open_inspect(&mut state, &key, body);
    repaint_ready(&mut state, &clock);
    let top = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(top.contains("skill line 001"), "{top}");
    assert!(
        !top.contains("skill line 120"),
        "the last line is below the viewport"
    );
    for _ in 0..40 {
        press(&mut state, Key::PageDown);
        press(&mut state, Key::Down);
    }
    let bottom = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(
        bottom.contains("skill line 120"),
        "scrolled to the end of the whole file\n{bottom}"
    );
    // Positive control: Up scrolls back toward the top.
    for _ in 0..200 {
        press(&mut state, Key::Up);
    }
    let again = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert!(again.contains("skill line 001"), "{again}");
    // `j` / `k` are the same scroll as Down / Up: to the same bottom and top rows.
    for _ in 0..200 {
        press(&mut state, Key::Char('j'));
    }
    let bottom_j = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert_eq!(bottom_j, bottom, "`j` reaches the arrows' bottom rows");
    for _ in 0..200 {
        press(&mut state, Key::Char('k'));
    }
    let top_k = screen(&paint(&mut state, &facts("c1"), 80, 24));
    assert_eq!(top_k, again, "`k` reaches the arrows' top rows");
}

/// R5: two literals the keymap cannot generate (the domain layer may not import the adapter)
/// are pinned to their registry rows.
#[test]
fn domain_literals_name_the_registry_keys() {
    use rustain::adapters::tui::keymap::render::rail;
    use rustain::adapters::tui::keymap::table_card::CardCmd;
    use rustain::adapters::tui::keymap::table_permission::PermissionCmd;
    use rustain::domain::models::team_policy::IndividualPolicy;
    use rustain::domain::services::approval_presentation::MORE_MARKER;
    use rustain::domain::services::team_policy::{
        explain_effective_policy, resolve_effective_policy,
    };
    assert!(
        MORE_MARKER.ends_with(&format!(
            "{} Review first",
            rail(Context::Permission, PermissionCmd::Review)
        )),
        "{MORE_MARKER}"
    );
    let effective = resolve_effective_policy(&IndividualPolicy::default(), None, &[]);
    let explanation = explain_effective_policy(&effective, &[]);
    let guidance = explanation
        .rows
        .iter()
        .find(|row| row.key == "consent:none")
        .and_then(|row| row.guidance.clone())
        .expect("the empty consent projection explains itself");
    let always = format!("({} Always allow)", rail(Context::Consent, CardCmd::Always));
    assert!(guidance.contains(&always), "{guidance}");
}
