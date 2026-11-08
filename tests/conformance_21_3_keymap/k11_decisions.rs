//! K11 — decision surfaces through the local front door (Story 21.3 AC15-AC17, A8;
//! re-pointed by 21-6 slice 6-C). The cards 21-6 has moved onto the decision frame
//! (skill trust, plan approval / card / deviation, skip cascade, cancel plan, delegation,
//! spawn gate, agent kill, the question) are queue items raised by their real producers
//! and decided through the 21-4 router the event loop calls; the cards 6-D has yet to
//! move are still built the way their producer leaves them (slot set, focus as the event
//! loop sets it) and driven by real key events. The attached twins live in
//! `attach_keys.rs` / `attach_loop.rs` (`story21_3_attached_*`).
//!
//! * `every_frame_key_decides_once_by_value` / `every_card_key_decides_once_by_value` are
//!   the by-value records of every card key.
//! * `esc_and_ctrl_c_never_decide_a_card` — legacy cards: `Esc` / `Ctrl+C` post ONE cue.
//!   `esc_and_ctrl_c_defer_and_never_decide_a_frame_card` — frame cards: `Esc` defers,
//!   `Ctrl+C` backs one frame (defers), the item stays queued, counted and answerable.
//! * `typing_in_input_never_answers_a_card` — with any card pending, the keys typed in
//!   Input are text.
//! * `a_card_swallows_every_key_it_has_no_row_for` — the isolation sweep.

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, DecisionEffect, DecisionKind, DecisionSubject, Validity,
};
use rustain::adapters::tui::keymap::render::card_cue;
use rustain::adapters::tui::keymap::{Context, Host, Key, Resolved, resolve};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::presentation::{self, Surface};
use rustain::adapters::tui::state::{PendingConsolidationCard, PendingForgetCard, TuiState};
use rustain::domain::models::FocusState;
use rustain::domain::models::visual::{ConfirmationType, OverlayType};
use tokio::sync::oneshot::error::TryRecvError;

use super::support::feedback;
use super::support::{fresh, is_inert, permission, permission_payload, press as press_base};
use crate::conformance_21_6_decision_queue::raisers::{
    self, Hold, PLAIN, open_ready, press, repaint_ready,
};
use crate::conformance_21_11_display_width::k5b_cards::{
    apply_pending, peer_pending, retract_pending,
};

fn confirm(card: ConfirmationType) -> FocusState {
    FocusState::Overlay(OverlayType::Confirmation(card))
}

fn chat() -> TuiState {
    let mut state = fresh();
    state.focus = FocusState::Chat;
    state
}

fn overlay(card: ConfirmationType) -> TuiState {
    let mut state = fresh();
    state.focus = confirm(card);
    state
}

/// One legacy card (6-D has yet to move it): how it is built, whether it is still
/// pending, the context that owns its keys, every key it answers (by value) and whether
/// it consumes every other key.
struct Card {
    name: &'static str,
    context: Context,
    build: fn() -> TuiState,
    pending: fn(&TuiState) -> bool,
    rows: Vec<(Key, InputAction)>,
    /// Focus `Chat` cards that swallow every character (consolidation, forget).
    swallows_chars: bool,
    /// Overlay-focus cards consume every key without a row.
    overlay: bool,
}

fn ch(c: char) -> Key {
    Key::Char(c)
}

fn cards() -> Vec<Card> {
    let yn = |yes: InputAction, no: InputAction| vec![(ch('y'), yes), (ch('n'), no)];
    vec![
        Card {
            name: "consolidation",
            context: Context::Confirm,
            build: || {
                let mut s = chat();
                s.pending_consolidation_card = Some(PendingConsolidationCard {
                    conversation_id: "c1".into(),
                    proposals: vec![],
                });
                s
            },
            pending: |s| s.pending_consolidation_card.is_some(),
            rows: yn(
                InputAction::ConsolidateAcceptAll,
                InputAction::ConsolidateDeclineAll,
            ),
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "forget",
            context: Context::Forget,
            build: || {
                let mut s = chat();
                s.pending_forget_card = Some(PendingForgetCard {
                    conversation_id: "c1".into(),
                    candidates: vec![],
                    focused_index: 0,
                });
                s
            },
            pending: |s| s.pending_forget_card.is_some(),
            rows: vec![
                (ch('y'), InputAction::ForgetAcceptAll),
                (ch('n'), InputAction::ForgetDeclineAll),
                (ch(' '), InputAction::ForgetToggleSelection),
                (ch('j'), InputAction::ForgetNavigateDown),
                (ch('k'), InputAction::ForgetNavigateUp),
            ],
            swallows_chars: true,
            overlay: false,
        },
        Card {
            name: "artifact apply",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::ArtifactApply);
                s.pending_artifact_card = Some(apply_pending(vec!["a.rs".into()], false));
                s
            },
            pending: |s| s.pending_artifact_card.is_some(),
            rows: yn(InputAction::ApplyCardAccept, InputAction::ApplyCardDecline),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "peer add",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::PeerAdd);
                s.pending_peer_add = Some(peer_pending("jun", None));
                s
            },
            pending: |s| s.pending_peer_add.is_some(),
            rows: yn(InputAction::PeerAddConfirm, InputAction::PeerAddDecline),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "team retract",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::TeamRetract);
                s.pending_team_retract = Some(retract_pending("card"));
                s
            },
            pending: |s| s.pending_team_retract.is_some(),
            rows: yn(
                InputAction::TeamRetractConfirm,
                InputAction::TeamRetractDecline,
            ),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "fork",
            context: Context::Confirm,
            build: || {
                let mut s = overlay(ConfirmationType::Fork);
                s.pending_fork_index = Some(0);
                s
            },
            pending: |s| s.pending_fork_index.is_some(),
            rows: yn(InputAction::ForkConfirm, InputAction::ForkCancel),
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "rewind",
            context: Context::Rewind,
            build: || {
                let mut s = overlay(ConfirmationType::Rewind);
                s.pending_rewind_index = Some(0);
                s
            },
            pending: |s| s.pending_rewind_index.is_some(),
            rows: vec![
                (ch('y'), InputAction::RewindConfirm),
                (ch('f'), InputAction::RewindForkInstead),
                (ch('n'), InputAction::RewindCancel),
            ],
            swallows_chars: true,
            overlay: true,
        },
        Card {
            name: "export overwrite",
            context: Context::ExportOverwrite,
            build: || {
                let mut s = overlay(ConfirmationType::ExportOverwrite("out.md".into()));
                s.pending_export = Some(("out.md".into(), String::new()));
                s
            },
            pending: |s| {
                matches!(
                    s.focus,
                    FocusState::Overlay(OverlayType::Confirmation(
                        ConfirmationType::ExportOverwrite(_)
                    ))
                )
            },
            rows: vec![
                (ch('y'), InputAction::ConfirmExportOverwrite),
                (ch('Y'), InputAction::ConfirmExportOverwrite),
                (ch('n'), InputAction::CancelExportOverwrite),
                (ch('N'), InputAction::CancelExportOverwrite),
            ],
            swallows_chars: true,
            overlay: true,
        },
    ]
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
            &crate::conformance_21_6_decision_queue::support::facts("c1"),
            80,
            24,
        ),
    )
}

/// A legacy card (consolidation) under an open deviation frame: the frame owns every key,
/// so `y` decides the deviation; the consolidation card stays pending and decides after.
#[test]
fn consolidation_and_deviation_decide_their_own_card_the_frame_first() {
    let (mut state, _clock, _hold) = open_ready(DecisionKind::PlanDeviation, PLAIN);
    state.focus = FocusState::Chat;
    state.pending_consolidation_card = Some(PendingConsolidationCard {
        conversation_id: "c1".into(),
        proposals: vec![],
    });
    assert_eq!(
        press(&mut state, ch('y')),
        effect(DecisionEffect::PlanDeviationApprove),
        "the frame decides the deviation, not the consolidation card"
    );
    assert!(state.pending_consolidation_card.is_some(), "still pending");
    assert_eq!(
        press_base(&mut state, ch('y')),
        InputAction::ConsolidateAcceptAll,
        "with the frame gone the consolidation card decides"
    );
}

#[test]
fn drafted_inline_cards_that_swallow_i_preserve_the_draft_until_a_decision() {
    // Legacy cards still swallow `i` without touching the draft.
    for card in cards()
        .into_iter()
        .filter(|c| ["consolidation", "forget"].contains(&c.name))
    {
        let mut state = (card.build)();
        state.input_buffer = "preserved draft".into();
        let action = press_base(&mut state, ch('i'));
        assert_eq!(action, InputAction::Consumed, "{}", card.name);
        assert_eq!(state.focus, FocusState::Chat);
        assert_eq!(state.input_buffer, "preserved draft");
        assert!((card.pending)(&state));
    }
    // The frame cards do the same: `i` is no row (skill trust excepted), swallowed.
    for card in frame_cards().into_iter().filter(|c| {
        [
            "spawn gate",
            "plan deviation",
            "cancel plan",
            "skip cascade",
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
fn every_card_key_decides_once_by_value() {
    for card in cards() {
        for (key, want) in &card.rows {
            let mut state = (card.build)();
            assert_eq!(
                press_base(&mut state, *key),
                *want,
                "{}: {key:?}",
                card.name
            );
        }
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
fn esc_and_ctrl_c_never_decide_a_card() {
    for card in cards() {
        let want_cue = card_cue(card.context, Host::Local);
        for key in [Key::Esc, Key::Ctrl('c')] {
            let mut state = (card.build)();
            let focus = state.focus.clone();
            let action = press_base(&mut state, key);
            assert!(
                is_inert(&action),
                "{}: {key:?} returned {action:?}",
                card.name
            );
            assert!(
                (card.pending)(&state),
                "{}: {key:?} resolved the card",
                card.name
            );
            assert_eq!(state.focus, focus, "{}: {key:?} moved the focus", card.name);
            assert_eq!(
                cue_records(&state),
                vec![want_cue.clone()],
                "{}: {key:?} cue",
                card.name
            );
            // Positive control: the explicit key still decides, once.
            let (first, want) = &card.rows[0];
            assert_eq!(
                press_base(&mut state, *first),
                *want,
                "{}: answerable after {key:?}",
                card.name
            );
        }
    }
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
    for card in cards().iter().filter(|c| !c.overlay) {
        for (key, answer) in &card.rows {
            let Key::Char(c) = key else { continue };
            let mut state = (card.build)();
            state.focus = FocusState::Input;
            let action = press_base(&mut state, *key);
            assert_ne!(&action, answer, "{}: {c:?} answered from Input", card.name);
            assert_eq!(
                state.input_buffer,
                c.to_string(),
                "{}: {c:?} is text",
                card.name
            );
            assert!(
                (card.pending)(&state),
                "{}: {c:?} resolved the card",
                card.name
            );
        }
    }
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
    for card in cards() {
        for key in Key::universe() {
            let own = card.rows.iter().any(|(k, _)| *k == key);
            let reserved = matches!(key, Key::Esc | Key::Ctrl('c') | Key::Ctrl('q'));
            let chars_only = !card.overlay && !matches!(key, Key::Char(_));
            if own || reserved || chars_only || (!card.swallows_chars && !card.overlay) {
                continue;
            }
            let mut state = (card.build)();
            let focus = state.focus.clone();
            let action = press_base(&mut state, key);
            assert!(
                is_inert(&action),
                "{}: {key:?} returned {action:?}",
                card.name
            );
            assert!(
                (card.pending)(&state),
                "{}: {key:?} resolved the card",
                card.name
            );
            assert_eq!(state.focus, focus, "{}: {key:?} moved the focus", card.name);
            assert_eq!(state.input_buffer, "", "{}: {key:?} typed", card.name);
        }
    }
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
    decision_queue::apply_resolved_action(
        &mut state,
        effect(DecisionEffect::QuestionSend),
        presentation::reducer::restore_focus_after_modal,
    );
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
    decision_queue::apply_resolved_action(
        &mut skip,
        effect(DecisionEffect::QuestionSkip),
        presentation::reducer::restore_focus_after_modal,
    );
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
    decision_queue::apply_resolved_action(
        &mut idle,
        effect(DecisionEffect::QuestionSkip),
        presentation::reducer::restore_focus_after_modal,
    );
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
    let forget = cards().into_iter().find(|c| c.name == "forget").unwrap();
    let mut state = (forget.build)();
    assert_eq!(press_base(&mut state, Key::Down), InputAction::Consumed);
    assert_eq!(press_base(&mut state, Key::Up), InputAction::Consumed);
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
        rail(Context::Confirm, CardCmd::Yes),
        rail(Context::Confirm, CardCmd::No)
    );
    assert!(card.contains(&line), "{card}");
}

/// Legacy card cues still name their negative key inside the toast budget (the frame
/// cards have no cue: `Esc` defers them).
#[test]
fn every_cue_names_its_negative_key_inside_the_toast_budget() {
    use rustain::adapters::tui::keymap::render::{CUE_BUDGET_CELLS, is_negative};
    use rustain::adapters::tui::keymap::resolve::rows_of;
    use rustain::adapters::tui::keymap::table_card;
    use rustain::adapters::tui::text_geometry::cells;
    assert_eq!(CUE_BUDGET_CELLS, 74, "80 columns minus the notice prefix");
    let mut contexts: Vec<Context> = Vec::new();
    for row in table_card::ROWS {
        contexts.push(row.context);
    }
    contexts.dedup();
    // The shared parent, the field/sub-frame contexts and every decision-frame context
    // answer `Esc` with a defer or a back, never a cue.
    let cued = |context: &&Context| {
        !matches!(
            **context,
            Context::Card
                | Context::DecisionCompose
                | Context::Question
                | Context::QuestionIdle
                | Context::SkillTrust
                | Context::SkillTrustInspect
                | Context::PlanApproval
                | Context::PlanDecision
                | Context::SkipCascade
                | Context::Delegation
                | Context::SpawnGate
                | Context::DecisionConfirm
                | Context::Consent
        )
    };
    let mut checked = 0;
    for context in contexts.iter().filter(cued) {
        for host in [Host::Local, Host::Attached] {
            let labelled: Vec<_> = rows_of(*context, host)
                .filter(|row| !row.label.is_empty())
                .collect();
            let cue = card_cue(*context, host);
            for row in labelled.iter().filter(|row| is_negative(row.command)) {
                let item = format!("[{}] {}", row.long_label(), row.label);
                let at = cue.find(&item).expect("the cue names its negative key");
                let end = cells(&cue[..at + item.len()]);
                assert!(
                    end <= CUE_BUDGET_CELLS,
                    "{context:?}/{host:?}: `{item}` ends at cell {end}: {cue}"
                );
                checked += 1;
            }
            assert!(
                labelled.is_empty() || labelled.iter().any(|row| is_negative(row.command)),
                "{context:?}/{host:?} has no negative key: {cue}"
            );
        }
    }
    assert!(checked >= 6, "{checked} legacy negative keys checked");
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

/// Every local row of `table_card` has a by-value witness: a `cards()` / `frame_cards()`
/// row, or one of the named tests below. A row added to the table without one fails here.
/// (`TeamProposal` is witnessed in `app.rs` under `a2a` and `Consent` in the attached tests.)
#[test]
fn every_table_card_row_has_a_witness() {
    use rustain::adapters::tui::keymap::table_card;
    let mut witnessed: Vec<(Context, Key)> = Vec::new();
    for card in cards() {
        witnessed.extend(card.rows.iter().map(|(key, _)| (card.context, *key)));
    }
    for card in frame_cards() {
        witnessed.extend(card.rows.iter().map(|(key, _)| (card.context, *key)));
    }
    witnessed.extend([
        // `esc_and_ctrl_c_never_decide_a_card`, once per legacy card.
        (Context::Card, Key::Esc),
        (Context::Card, Key::Ctrl('c')),
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
        (Context::Forget, Key::Up),
    ]);
    let unwitnessed: Vec<String> = table_card::ROWS
        .iter()
        .filter(|row| row.hosts.includes(Host::Local))
        .filter(|row| !matches!(row.context, Context::TeamProposal | Context::Consent))
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
