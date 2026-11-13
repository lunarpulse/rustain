//! Story 21.6 6-C: the queue-side behaviour of the card kinds (question, skill trust, the
//! plan family, delegation, spawn gate, agent kill): raise, FIFO, same-kind promotion,
//! `Esc` defer, stale, inert window, sanitized paint, height == paint, copy bound.

use std::sync::Arc;

use ratatui::prelude::*;
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::oneshot;

use super::*;
use crate::adapters::tui::fanout_spec::FanOutSpec;
use crate::adapters::tui::notifications::NotificationStore;
use crate::adapters::tui::tool_inspector::source::COPY_MAX_BYTES;
use crate::adapters::tui::widgets::decision_frame;
use crate::domain::clock::MockClock;
use crate::domain::models::plan::{PlanDeviationKind, PlanStatus, PlanTaskStatus};
use crate::domain::models::{AgentId, NotificationsConfig, Plan, SkillTrustResponse};
use crate::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};

const CONV: &str = "c1";

fn clocked() -> (TuiState, Arc<MockClock>) {
    let clock = Arc::new(MockClock::new(Instant::now()));
    let mut state = TuiState::new(80, 24);
    state.notifications =
        NotificationStore::from_config(&NotificationsConfig::default(), clock.clone());
    (state, clock)
}

fn facts(active: &str) -> LogFacts {
    LogFacts {
        active_conversation_id: Some(active.to_owned()),
        ..LogFacts::default()
    }
}

fn ready(state: &mut TuiState, clock: &MockClock) {
    note_painted(state);
    clock.advance(INERT_AFTER_PAINT);
}

fn draw(state: &TuiState, f: &LogFacts, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = state.theme.clone();
    terminal
        .draw(|frame| {
            let area = Rect::new(0, 0, width, height.saturating_sub(4));
            decision_frame::render(frame, area, state, f, &theme, "Decision: test", None);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rail_text(state: &TuiState, f: &LogFacts) -> String {
    let item = current(state, f).expect("a focused item");
    rail_of(item)
        .iter()
        .map(|(key, label)| format!("{key} {label}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn plan() -> Plan {
    Plan {
        id: "p1".into(),
        title: "Ship".into(),
        tasks: vec![],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: None,
        origin_restrictions: None,
    }
}

fn pending_cascade() -> SkipCascadePending {
    SkipCascadePending {
        plan_id: "p1".into(),
        source_task: 1,
        source_prior_status: PlanTaskStatus::Pending,
        source_prior_error: None,
        downstream: vec![2, 3],
    }
}

/// The kinds that need no reply channel to be raised.
const PLAIN: [DecisionKind; 9] = [
    DecisionKind::SkillTrust,
    DecisionKind::PlanApproval,
    DecisionKind::PlanCard,
    DecisionKind::PlanDeviation,
    DecisionKind::SkipCascade,
    DecisionKind::CancelPlan,
    DecisionKind::Delegation,
    DecisionKind::SpawnGate,
    DecisionKind::AgentKill,
];

fn raise_plain(state: &mut TuiState, kind: DecisionKind) {
    match kind {
        DecisionKind::SkillTrust => {
            raise_user_skill_trust(
                state,
                CONV,
                "review".into(),
                "/w/skill.md".into(),
                "args".into(),
            );
        }
        DecisionKind::PlanApproval => {
            raise_plan_approval(
                state,
                PlanApprovalPayload {
                    conversation_id: CONV.into(),
                    plan_path: "/w/plan.md".into(),
                    contents: "a\nb".into(),
                    summary: "two".into(),
                },
            );
        }
        DecisionKind::PlanCard => {
            raise_plan_card(
                state,
                PlanCardPayload {
                    conversation_id: CONV.into(),
                    plan_id: "p1".into(),
                    plan_snapshot: plan(),
                },
            );
        }
        DecisionKind::PlanDeviation => {
            raise_plan_deviation(
                state,
                CONV,
                PlanDeviationPayload {
                    plan_id: "p1".into(),
                    kind: PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
                    original: 3,
                    changed: vec![2, 3],
                    summary: "2 task(s) auto-skipped".into(),
                },
            );
        }
        DecisionKind::SkipCascade => {
            raise_skip_cascade(state, CONV, pending_cascade());
        }
        DecisionKind::CancelPlan => {
            raise_cancel_plan(
                state,
                CONV,
                CancelPlanPayload {
                    plan_id: "p1".into(),
                    title: "Ship".into(),
                    pending: 2,
                    completed: 1,
                },
            );
        }
        DecisionKind::Delegation => {
            raise_delegation(
                state,
                PendingDelegationCard {
                    conversation_id: CONV.into(),
                    plan_id: "p1".into(),
                    task_number: 2,
                    suggestion: DelegationSuggestion {
                        task_number: 2,
                        agent_name: "reviewer".into(),
                        reason: DelegationReason::Heuristic,
                        auto_proceed: false,
                    },
                },
            );
        }
        DecisionKind::SpawnGate => {
            raise_spawn_gate(
                state,
                CONV,
                PendingSpawnGate {
                    spec: FanOutSpec::Identical {
                        count: 20,
                        prompt: "go".into(),
                    },
                    requested: 20,
                    threshold: 8,
                    adjusted: None,
                },
            );
        }
        DecisionKind::AgentKill => {
            raise_agent_kill(state, CONV, AgentId::new(), "reviewer".into());
        }
        other => unreachable!("{other:?} needs a reply channel"),
    }
}

fn question(state: &mut TuiState, text: &str) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    raise_question(state, CONV, "tu-1".into(), text.into(), tx);
    rx
}

fn key_of(state: &TuiState) -> DecisionKey {
    state.decisions.items().next().expect("an item").key()
}

#[test]
fn story21_6_each_card_kind_paints_its_registry_rail_and_keeps_a_body_row_at_60x16() {
    let table: [(DecisionKind, &str, &str); 9] = [
        (
            DecisionKind::SkillTrust,
            "[y] Trust  [n] Decline  [i] Inspect  [Esc] Later",
            "New project skill detected",
        ),
        (
            DecisionKind::PlanApproval,
            "[y] Approve  [a] Approve + auto-edit  [n] Reject  [e] Edit  [Esc] Later",
            "Plan Approval — plan.md",
        ),
        (
            DecisionKind::PlanCard,
            "[y] Approve  [e] Edit  [n] Reject  [Esc] Later",
            "Ship",
        ),
        (
            DecisionKind::PlanDeviation,
            "[y] Approve revised  [e] Edit  [n] Reject  [Esc] Later",
            "Plan deviation",
        ),
        (
            DecisionKind::SkipCascade,
            "[s] Skip cascade  [a] Continue anyway  [n] Cancel skip  [Esc] Later",
            "Task 1 skipped",
        ),
        (
            DecisionKind::CancelPlan,
            "[y] Cancel all  [n] Keep running  [Esc] Later",
            "task(s) remaining",
        ),
        (
            DecisionKind::Delegation,
            "[d] Delegate  [l] Run locally  [x] Cancel plan  [Esc] Later",
            "Delegate Task 2?",
        ),
        (
            DecisionKind::SpawnGate,
            "[Enter] Confirm  [t] Cap  [n] Cancel  [←/→] adjust  [Esc] Later",
            "spawn 20?",
        ),
        (
            DecisionKind::AgentKill,
            "[y] Confirm  [n] Decline  [Esc] Later",
            "Kill reviewer and its subtree?",
        ),
    ];
    let f = facts(CONV);
    for (kind, rail, body) in table {
        for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
            let (mut state, _clock) = clocked();
            raise_plain(&mut state, kind);
            assert!(
                presentation::is_open(&state, Surface::Decision),
                "{kind:?}: opens"
            );
            assert_eq!(rail_text(&state, &f), rail, "{kind:?}");
            let text = draw(&state, &f, width, height);
            for pair in rail.split("  ") {
                assert!(
                    text.contains(pair),
                    "{kind:?} at {width}x{height}: {pair:?} missing\n{text}"
                );
            }
            assert!(
                text.contains(body),
                "{kind:?} at {width}x{height}: a body row survives\n{text}"
            );
            let item = current(&state, &f).unwrap();
            assert!(decision_frame::body_rows(item, &state) >= 1, "{kind:?}");
        }
    }
}

#[test]
fn story21_6_every_deciding_key_resolves_once_and_hands_over_its_payload() {
    use DecisionEffect as E;
    let table: [(DecisionKind, char, DecisionEffect); 22] = [
        (DecisionKind::SkillTrust, 'y', E::SkillTrustAccept),
        (DecisionKind::SkillTrust, 'n', E::SkillTrustDecline),
        (DecisionKind::PlanApproval, 'y', E::PlanApproveNormal),
        (DecisionKind::PlanApproval, 'a', E::PlanApproveAutoEdit),
        (DecisionKind::PlanApproval, 'n', E::PlanReject),
        (DecisionKind::PlanCard, 'y', E::PlanCardApprove),
        (DecisionKind::PlanCard, 'e', E::PlanCardEdit),
        (DecisionKind::PlanCard, 'n', E::PlanCardReject),
        (DecisionKind::PlanDeviation, 'y', E::PlanDeviationApprove),
        (DecisionKind::PlanDeviation, 'e', E::PlanDeviationEdit),
        (DecisionKind::PlanDeviation, 'n', E::PlanDeviationReject),
        (DecisionKind::SkipCascade, 's', E::SkipCascadeSkip),
        (DecisionKind::SkipCascade, 'a', E::SkipCascadeContinue),
        (DecisionKind::SkipCascade, 'n', E::SkipCascadeCancel),
        (DecisionKind::CancelPlan, 'y', E::CancelPlanConfirm),
        (DecisionKind::CancelPlan, 'n', E::CancelPlanDecline),
        (DecisionKind::AgentKill, 'y', E::AgentKillConfirm),
        (DecisionKind::AgentKill, 'n', E::AgentKillDecline),
        (DecisionKind::Delegation, 'd', E::DelegateTask),
        (DecisionKind::Delegation, 'l', E::RunTaskLocally),
        (DecisionKind::Delegation, 'x', E::DelegationCardCancel),
        (DecisionKind::SpawnGate, 'n', E::SpawnGateCancel),
    ];
    let f = facts(CONV);
    for (kind, key, effect) in table {
        let (mut state, clock) = clocked();
        raise_plain(&mut state, kind);
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(key), &f),
            PaneOutcome::Effect(effect),
            "{kind:?} {key}"
        );
        assert!(state.decisions.is_empty(), "{kind:?}: the item left first");
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert!(
            state.decisions.take_resolved().is_some(),
            "{kind:?}: the payload waits for its effect arm"
        );
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(key), &f),
            PaneOutcome::Consumed,
            "{kind:?}: a second key runs nothing"
        );
    }
    // The spawn gate confirms with Enter.
    let (mut state, clock) = clocked();
    raise_plain(&mut state, DecisionKind::SpawnGate);
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Effect(DecisionEffect::SpawnGateConfirm)
    );
    assert!(matches!(
        state.decisions.take_resolved(),
        Some(DecisionSubject::SpawnGate(gate)) if gate.effective_n() == 20
    ));
}

#[test]
fn story21_6_retract_answer_transfers_the_armed_payload_once_and_restores_base() {
    for (key, effect) in [
        ('y', DecisionEffect::TeamRetractConfirm),
        ('n', DecisionEffect::TeamRetractDecline),
    ] {
        let (mut state, clock) = clocked();
        let payload = TeamRetractPayload {
            target: RetractTarget::Item {
                peer: "jun-dev".into(),
                item: "ri_x".into(),
            },
            conversation_id: CONV.into(),
            principal: None,
            task: Some("task-x".into()),
            card: "retract the selected item".into(),
            armed: true,
            request: None,
        };
        raise_team_retract(&mut state, payload.clone(), Responder::Local);
        ready(&mut state, &clock);
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(key), &facts(CONV)),
            PaneOutcome::Effect(effect)
        );
        assert_eq!(
            state.decisions.take_resolved(),
            Some(DecisionSubject::TeamRetract(Box::new(payload)))
        );
        assert!(state.decisions.take_resolved().is_none());
        assert_eq!(state.focus, FocusState::Input);
        assert!(!presentation::is_open(&state, Surface::Decision));
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(key), &facts(CONV)),
            PaneOutcome::Consumed
        );
    }
}

#[test]
fn story21_6_retract_resolution_restores_the_interrupted_frame() {
    let (mut state, clock) = clocked();
    presentation::reducer::open(&mut state, Surface::Help);
    let interrupted = state.modal_stack.top().unwrap().clone();
    raise_team_retract(
        &mut state,
        TeamRetractPayload {
            target: RetractTarget::Item {
                peer: "jun-dev".into(),
                item: "ri_x".into(),
            },
            conversation_id: CONV.into(),
            principal: None,
            task: None,
            card: "retract".into(),
            armed: true,
            request: None,
        },
        Responder::Local,
    );
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('y'), &facts(CONV)),
        PaneOutcome::Effect(DecisionEffect::TeamRetractConfirm)
    );
    assert_eq!(state.modal_stack.top(), Some(&interrupted));
    assert_eq!(state.focus, FocusState::Input);
}

#[test]
fn story21_6_disarmed_retract_cannot_confirm() {
    let (mut state, clock) = clocked();
    raise_team_retract(
        &mut state,
        TeamRetractPayload {
            target: RetractTarget::Item {
                peer: "jun-dev".into(),
                item: "ri_x".into(),
            },
            conversation_id: CONV.into(),
            principal: None,
            task: None,
            card: "Cannot verify on jun-dev's host.".into(),
            armed: false,
            request: None,
        },
        Responder::Local,
    );
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('y'), &facts(CONV)),
        PaneOutcome::Consumed
    );
    assert!(team_retract_pending(&state));
    assert!(state.decisions.take_resolved().is_none());
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('n'), &facts(CONV)),
        PaneOutcome::Effect(DecisionEffect::TeamRetractDecline)
    );
}

#[test]
fn story21_6_forget_selection_resolves_once_with_the_original_conversation() {
    let (mut state, clock) = clocked();
    let entry = |summary: &str| crate::domain::models::MemoryEntry {
        timestamp: chrono::Local::now(),
        summary: summary.into(),
        context: None,
    };
    raise_forget(
        &mut state,
        PendingForgetCard {
            conversation_id: CONV.into(),
            candidates: vec![(7, entry("a"), true), (9, entry("b"), false)],
            focused_index: 0,
        },
    );
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('y'), &facts(CONV)),
        PaneOutcome::Effect(DecisionEffect::ForgetAccept)
    );
    let Some(DecisionSubject::Forget(card)) = state.decisions.take_resolved() else {
        panic!("forget payload missing");
    };
    match forget_resolution(&card, true) {
        crate::domain::events::AppEvent::MemoryForgetResolved {
            conversation_id,
            keys,
        } => {
            assert_eq!(conversation_id, CONV);
            assert_eq!(keys, vec![7]);
        }
        _ => panic!("wrong event"),
    }
    assert!(state.decisions.take_resolved().is_none());
    assert!(!forget_pending(&state, CONV));
}

#[test]
fn story21_6_every_effect_maps_to_an_existing_action_or_the_decision_action() {
    use DecisionEffect as E;
    assert!(matches!(
        E::SkillTrustAccept.into_action(),
        InputAction::SkillTrustAccept
    ));
    assert!(matches!(
        E::PlanApproveAutoEdit.into_action(),
        InputAction::PlanApproveAutoEdit
    ));
    assert!(matches!(
        E::PlanCardEdit.into_action(),
        InputAction::PlanCardEdit
    ));
    assert!(matches!(
        E::DelegationCardCancel.into_action(),
        InputAction::DelegationCardCancel
    ));
    assert!(matches!(
        E::SpawnGateConfirm.into_action(),
        InputAction::SpawnGateConfirm
    ));
    for effect in [
        E::QuestionSend,
        E::QuestionSkip,
        E::PlanDeviationEdit,
        E::SkipCascadeCancel,
        E::CancelPlanConfirm,
        E::AgentKillDecline,
        E::SpawnGateCancel,
    ] {
        assert!(
            matches!(effect.into_action(), InputAction::Decision(inner) if inner == effect),
            "{effect:?}"
        );
    }
    assert!(E::SkillTrustInspect.keeps_item() && E::PlanRevise.keeps_item());
    assert!(!E::SkillTrustAccept.keeps_item());
}

#[test]
fn story21_6_esc_defers_every_card_kind_and_ctrl_c_backs_without_deciding() {
    let f = facts(CONV);
    for kind in PLAIN {
        for key in [PaneKey::Esc, PaneKey::CtrlC] {
            let (mut state, _clock) = clocked();
            raise_plain(&mut state, kind);
            // Setting an item aside is never inert: no paint is needed.
            assert_eq!(handle_key(&mut state, key, &f), PaneOutcome::Consumed);
            assert!(
                !presentation::is_open(&state, Surface::Decision),
                "{kind:?}"
            );
            let item = state.decisions.items().next().expect("still queued");
            assert!(item.deferred, "{kind:?}: deferred, never decided");
            assert_eq!(waiting(&state), 1);
            assert!(kind.deferrable());
        }
    }
}

#[test]
fn story21_6_card_keys_are_inert_until_the_frame_has_painted() {
    let f = facts(CONV);
    for kind in PLAIN {
        let (mut state, clock) = clocked();
        raise_plain(&mut state, kind);
        let key = match kind {
            DecisionKind::SkillTrust | DecisionKind::PlanApproval | DecisionKind::PlanCard => 'y',
            DecisionKind::PlanDeviation | DecisionKind::CancelPlan | DecisionKind::AgentKill => 'y',
            DecisionKind::SkipCascade => 's',
            DecisionKind::Delegation => 'd',
            _ => 'n',
        };
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(key), &f),
            PaneOutcome::Consumed,
            "{kind:?}: unpainted"
        );
        assert_eq!(state.decisions.len(), 1, "{kind:?}");
        ready(&mut state, &clock);
        assert!(
            matches!(
                handle_key(&mut state, PaneKey::Char(key), &f),
                PaneOutcome::Effect(_)
            ),
            "{kind:?}: painted"
        );
    }
}

#[test]
fn story21_6_a_withdrawn_plan_family_item_turns_stale_and_close_removes_it() {
    let f = facts(CONV);
    for kind in [
        DecisionKind::PlanCard,
        DecisionKind::PlanDeviation,
        DecisionKind::SkipCascade,
        DecisionKind::CancelPlan,
        DecisionKind::Delegation,
    ] {
        let (mut state, clock) = clocked();
        raise_plain(&mut state, kind);
        ready(&mut state, &clock);
        withdraw_plan(&mut state, "p1");
        let item = state.decisions.items().next().expect("the open item stays");
        assert_eq!(
            item.validity,
            Validity::Stale(NO_LONGER_PENDING),
            "{kind:?}"
        );
        let text = draw(&state, &f, 80, 24);
        assert!(
            text.contains("This decision is no longer pending."),
            "{text}"
        );
        assert!(text.contains("[Esc] Close"), "{text}");
        for key in ['y', 'n', 'e', 's', 'a', 'd', 'l', 'x'] {
            assert_eq!(
                handle_key(&mut state, PaneKey::Char(key), &f),
                PaneOutcome::Consumed,
                "{kind:?} {key}"
            );
        }
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(
            handle_key(&mut state, PaneKey::Esc, &f),
            PaneOutcome::Consumed
        );
        assert!(state.decisions.is_empty(), "{kind:?}: Close removes it");
        assert_eq!(state.decisions.last_defer_at(), None);
    }
    // The kill confirmation goes stale when the agent is gone.
    let (mut state, _clock) = clocked();
    let agent = AgentId::new();
    raise_agent_kill(&mut state, CONV, agent.clone(), "reviewer".into());
    withdraw_agent_kill(&mut state, &agent);
    assert!(matches!(
        state.decisions.items().next().map(|item| item.validity),
        Some(Validity::Stale(_))
    ));
}

#[test]
fn story21_6_an_item_for_another_tab_waits_counted_and_is_never_answered_there() {
    let (mut state, clock) = clocked();
    raise_plain(&mut state, DecisionKind::PlanCard);
    ready(&mut state, &clock);
    let elsewhere = facts("c2");
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('y'), &elsewhere),
        PaneOutcome::Consumed,
        "the key reaches no answer in the wrong tab"
    );
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(waiting(&state), 1, "it still counts");
    assert!(current(&state, &elsewhere).is_none());
}

#[test]
fn story21_6_model_skill_trust_prompts_queue_fifo_and_promote_from_the_base() {
    let (mut state, clock) = clocked();
    let f = LogFacts::default();
    let (tx_a, mut rx_a) = oneshot::channel();
    let (tx_b, mut rx_b) = oneshot::channel();
    raise_model_skill_trust(&mut state, "a".into(), "/w/a.md".into(), tx_a);
    raise_model_skill_trust(&mut state, "b".into(), "/w/b.md".into(), tx_b);
    let names: Vec<String> = state
        .decisions
        .items()
        .map(|item| item.summary.clone())
        .collect();
    assert_eq!(names, ["a", "b"], "FIFO by arrival");
    assert_eq!(state.decisions.count(DecisionKind::SkillTrust), 2);
    ready(&mut state, &clock);
    let first = current(&state, &f).unwrap().summary.clone();
    assert_eq!(first, "a");
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('y'), &f),
        PaneOutcome::Effect(DecisionEffect::SkillTrustAccept)
    );
    let Some(DecisionSubject::SkillTrust(payload)) = state.decisions.take_resolved() else {
        panic!("the model prompt's payload")
    };
    assert!(payload.reply_model(SkillTrustResponse::Accepted));
    assert_eq!(rx_a.try_recv().unwrap(), SkillTrustResponse::Accepted);
    // Answered from the base with nothing beneath: the next prompt opens at once (A9).
    assert_eq!(
        current(&state, &f).map(|item| item.summary.as_str()),
        Some("b")
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    assert!(rx_b.try_recv().is_err(), "b is still waiting");
}

#[test]
fn story21_6_a_second_user_driven_skill_prompt_is_dropped_while_one_waits() {
    let (mut state, _clock) = clocked();
    assert!(
        raise_user_skill_trust(&mut state, CONV, "a".into(), "/w/a.md".into(), "".into()).is_some()
    );
    assert!(
        raise_user_skill_trust(&mut state, CONV, "b".into(), "/w/b.md".into(), "".into()).is_none(),
        "the legacy drop"
    );
    assert_eq!(state.decisions.count(DecisionKind::SkillTrust), 1);
    let (tx, _rx) = oneshot::channel();
    raise_model_skill_trust(&mut state, "m".into(), "/w/m.md".into(), tx);
    assert_eq!(
        state.decisions.count(DecisionKind::SkillTrust),
        2,
        "model prompts queue"
    );
    let drained = drain_skill_trust(&mut state);
    assert_eq!(drained.len(), 2);
}

#[test]
fn story21_6_skill_inspect_is_a_sub_context_that_scrolls_the_whole_file() {
    let (mut state, clock) = clocked();
    let f = LogFacts::default();
    let (tx, _rx) = oneshot::channel();
    raise_model_skill_trust(&mut state, "review".into(), "/w/skill.md".into(), tx);
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('i'), &f),
        PaneOutcome::Effect(DecisionEffect::SkillTrustInspect),
        "the host reads the file off the paint path"
    );
    assert_eq!(state.decisions.len(), 1, "inspecting decides nothing");
    let (key, path) = inspect_target(&state).expect("a prompt whose file is unread");
    assert_eq!(path, std::path::PathBuf::from("/w/skill.md"));
    let content = (0..200)
        .map(|n| format!("line-{n:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    open_inspect(&mut state, &key, content);
    assert!(inspect_target(&state).is_none(), "read once");
    assert_eq!(rail_text(&state, &f), "[Esc] Back  [PgUp/PgDn] scroll");
    assert!(draw(&state, &f, 80, 24).contains("Inspect skill: \"review\""));
    // The deciding keys do nothing while the file is read.
    for key in ['y', 'n'] {
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(key), &f),
            PaneOutcome::Consumed
        );
    }
    assert_eq!(state.decisions.len(), 1);
    // The whole file scrolls (no 20-line cap): Down and PgDn move the body.
    assert_eq!(
        handle_key(&mut state, PaneKey::Down, &f),
        PaneOutcome::Consumed
    );
    assert_eq!(state.decisions.scroll(), 1);
    assert_eq!(
        handle_key(&mut state, PaneKey::PageDown, &f),
        PaneOutcome::Consumed
    );
    assert_eq!(state.decisions.scroll(), 6);
    for _ in 0..60 {
        handle_key(&mut state, PaneKey::PageDown, &f);
    }
    let item = current(&state, &f).unwrap();
    assert_eq!(decision_frame::body_rows(item, &state), 201);
    assert!(
        draw(&state, &f, 80, 24).contains("line-199"),
        "the last line is reachable"
    );
    // Esc goes back to the undecided prompt; a second `i` needs no host read.
    assert_eq!(
        handle_key(&mut state, PaneKey::Esc, &f),
        PaneOutcome::Consumed
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    assert_eq!(
        rail_text(&state, &f),
        "[y] Trust  [n] Decline  [i] Inspect  [Esc] Later"
    );
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('i'), &f),
        PaneOutcome::Consumed
    );
    assert_eq!(rail_text(&state, &f), "[Esc] Back  [PgUp/PgDn] scroll");
    // Ctrl+C is back one step too, then it defers.
    assert_eq!(
        handle_key(&mut state, PaneKey::CtrlC, &f),
        PaneOutcome::Consumed
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    assert_eq!(
        handle_key(&mut state, PaneKey::CtrlC, &f),
        PaneOutcome::Consumed
    );
    assert!(!presentation::is_open(&state, Surface::Decision));
    // A frame opens on the prompt, never inside the reader.
    open_inspect(&mut state, &key, "x".into());
    presentation::reducer::close(&mut state, Surface::Decision);
    assert!(open(&mut state, &key));
    assert_eq!(
        rail_text(&state, &f),
        "[y] Trust  [n] Decline  [i] Inspect  [Esc] Later"
    );
}

#[test]
fn story21_6_plan_revise_keeps_the_plan_queued_and_the_editor_contents_show() {
    let (mut state, clock) = clocked();
    // A plan approval is decidable only in the tab that proposed it.
    let f = facts(CONV);
    raise_plain(&mut state, DecisionKind::PlanApproval);
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('e'), &f),
        PaneOutcome::Effect(DecisionEffect::PlanRevise)
    );
    assert_eq!(
        state.decisions.len(),
        1,
        "opening the editor decides nothing"
    );
    let (key, path) = plan_approval_target(&state).unwrap();
    assert_eq!(path, std::path::PathBuf::from("/w/plan.md"));
    set_plan_contents(&mut state, &key, "revised line".into());
    assert!(draw(&state, &f, 80, 24).contains("revised line"));
}

#[test]
fn story21_6_the_spawn_gate_adjusts_and_caps_in_place_and_stays_queued() {
    let (mut state, clock) = clocked();
    let f = facts(CONV);
    raise_plain(&mut state, DecisionKind::SpawnGate);
    ready(&mut state, &clock);
    // `adjust_right` is bounded by the legacy fork-join spawn cap (8).
    for (key, expect) in [
        (PaneKey::Left, 19usize),
        (PaneKey::Left, 18),
        (PaneKey::Right, 8),
        (PaneKey::Left, 7),
        (PaneKey::Char('t'), 8),
    ] {
        assert_eq!(
            handle_key(&mut state, key, &f),
            PaneOutcome::Consumed,
            "{key:?}"
        );
        let Some(DecisionSubject::SpawnGate(gate)) =
            state.decisions.items().next().map(|i| &i.subject)
        else {
            panic!("gate")
        };
        assert_eq!(gate.effective_n(), expect, "{key:?}");
        assert_eq!(state.decisions.len(), 1);
    }
    assert!(draw(&state, &f, 80, 24).contains("spawn 8?"));
    // `c` is Copy on a decision frame now, never Cap.
    assert!(matches!(
        handle_key(&mut state, PaneKey::Char('c'), &f),
        PaneOutcome::Copy(text) if text.contains("spawn 8?")
    ));
    assert_eq!(state.decisions.len(), 1);
}

#[test]
fn story21_6_skip_cascade_continue_is_a_not_c() {
    let (mut state, clock) = clocked();
    let f = facts(CONV);
    raise_plain(&mut state, DecisionKind::SkipCascade);
    ready(&mut state, &clock);
    assert!(matches!(
        handle_key(&mut state, PaneKey::Char('c'), &f),
        PaneOutcome::Copy(_)
    ));
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('a'), &f),
        PaneOutcome::Effect(DecisionEffect::SkipCascadeContinue)
    );
}

#[test]
fn story21_6_a_question_edits_its_own_field_never_the_composer() {
    let (mut state, clock) = clocked();
    let f = LogFacts::default();
    let mut rx = question(&mut state, "What name?");
    state.input_buffer = "draft".into();
    state.cursor_position = 5;
    assert!(presentation::is_open(&state, Surface::Decision));
    assert_eq!(
        rail_text(&state, &f),
        "[Enter] Send  [Ctrl+D] Skip question  [Esc] Back"
    );
    // Typing needs no inert window; `c` types, it never copies, in the field.
    for c in ['h', 'i', 'c'] {
        assert_eq!(
            handle_key(&mut state, PaneKey::Char(c), &f),
            PaneOutcome::Consumed
        );
    }
    assert_eq!(state.input_buffer, "draft");
    assert_eq!(state.cursor_position, 5);
    let DecisionSubject::Question(payload) = &state.decisions.items().next().unwrap().subject
    else {
        panic!("question")
    };
    assert_eq!(payload.field.as_str(), "hic");
    assert!(draw(&state, &f, 80, 24).contains("Answer: hic▋"));
    // A paste is one line, in the field.
    assert!(paste(&mut state, "\n!"));
    assert_eq!(state.input_buffer, "draft");
    // Enter inside the inert window is dropped; the typed text survives.
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Consumed
    );
    assert_eq!(state.decisions.len(), 1);
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Effect(DecisionEffect::QuestionSend)
    );
    assert!(state.decisions.is_empty());
    apply_resolved_action(
        &mut state,
        InputAction::Decision(DecisionEffect::QuestionSend),
    );
    assert_eq!(rx.try_recv().unwrap(), "hic !");
    assert_eq!(
        state.input_buffer, "draft",
        "the composer is byte-identical"
    );
}

#[test]
fn story21_6_esc_leaves_the_answer_field_then_defers_and_reopening_keeps_the_text() {
    let (mut state, clock) = clocked();
    let f = LogFacts::default();
    let mut rx = question(&mut state, "What name?");
    ready(&mut state, &clock);
    for c in "ab".chars() {
        handle_key(&mut state, PaneKey::Char(c), &f);
    }
    // First Esc leaves the field; the question stays open and undecided.
    assert_eq!(
        handle_key(&mut state, PaneKey::Esc, &f),
        PaneOutcome::Consumed
    );
    assert!(presentation::is_open(&state, Surface::Decision));
    assert_eq!(
        rail_text(&state, &f),
        "[Enter] Answer  [Ctrl+D] Skip question  [Esc] Later"
    );
    assert!(draw(&state, &f, 80, 24).contains("Answer: ab"));
    assert!(
        !draw(&state, &f, 80, 24).contains('▋'),
        "no caret while the field is closed"
    );
    // Enter opens the field again (no effect); Esc, Esc defers.
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Consumed
    );
    assert_eq!(
        rail_text(&state, &f),
        "[Enter] Send  [Ctrl+D] Skip question  [Esc] Back"
    );
    handle_key(&mut state, PaneKey::Esc, &f);
    assert_eq!(
        handle_key(&mut state, PaneKey::Esc, &f),
        PaneOutcome::Consumed
    );
    assert!(!presentation::is_open(&state, Surface::Decision));
    assert!(state.decisions.items().next().unwrap().deferred);
    assert!(
        rx.try_recv().is_err(),
        "the turn keeps waiting: never cancelled, never answered"
    );
    // Reopened from the list: the field is open again with the text kept; Enter sends once.
    let key = key_of(&state);
    assert!(open(&mut state, &key));
    ready(&mut state, &clock);
    assert_eq!(
        rail_text(&state, &f),
        "[Enter] Send  [Ctrl+D] Skip question  [Esc] Back"
    );
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Effect(DecisionEffect::QuestionSend)
    );
    apply_resolved_action(
        &mut state,
        InputAction::Decision(DecisionEffect::QuestionSend),
    );
    assert_eq!(rx.try_recv().unwrap(), "ab");
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Consumed,
        "answered once"
    );
}

#[test]
fn story21_6_an_empty_answer_sends_nothing_and_ctrl_d_skips() {
    let (mut state, clock) = clocked();
    let f = LogFacts::default();
    let mut rx = question(&mut state, "What name?");
    ready(&mut state, &clock);
    assert_eq!(
        handle_key(&mut state, PaneKey::Enter, &f),
        PaneOutcome::Consumed
    );
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(
        handle_key(&mut state, PaneKey::CtrlD, &f),
        PaneOutcome::Effect(DecisionEffect::QuestionSkip)
    );
    apply_resolved_action(
        &mut state,
        InputAction::Decision(DecisionEffect::QuestionSkip),
    );
    assert!(
        matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
        "the turn gets a RecvError"
    );
}

#[test]
fn story21_6_a_question_whose_turn_is_gone_turns_stale_through_the_tick() {
    let (mut state, _clock) = clocked();
    let f = LogFacts::default();
    let rx = question(&mut state, "What name?");
    assert!(!tick(&mut state), "a live question changes nothing");
    drop(rx);
    assert!(tick(&mut state), "the dead turn asks for the redraw");
    let item = state
        .decisions
        .items()
        .next()
        .expect("the open frame keeps it");
    assert_eq!(item.validity, Validity::Stale(NO_LONGER_PENDING));
    assert!(draw(&state, &f, 80, 24).contains("This decision is no longer pending."));
    assert_eq!(
        handle_key(&mut state, PaneKey::Esc, &f),
        PaneOutcome::Consumed
    );
    assert!(state.decisions.is_empty());
    // Not open: it simply leaves the queue (prune_closed path).
    let (mut state, _clock) = clocked();
    let rx = question(&mut state, "Again?");
    presentation::reducer::close(&mut state, Surface::Decision);
    drop(rx);
    prune_closed(&mut state, &LogFacts::default());
    assert!(state.decisions.is_empty());
}

#[test]
fn story21_6_draining_questions_abandons_every_reply() {
    let (mut state, _clock) = clocked();
    let mut rx = question(&mut state, "What name?");
    drain_questions(&mut state, CONV);
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Closed)
    ));
    assert!(
        state.decisions.items().all(|item| item.is_stale()),
        "an open frame turns stale"
    );
}

#[test]
fn story21_6_copy_is_bounded_at_the_copy_limit_and_keeps_the_original_bytes() {
    let f = LogFacts::default();
    for (len, copied) in [(COPY_MAX_BYTES, true), (COPY_MAX_BYTES + 1, false)] {
        let (mut state, _clock) = clocked();
        let _rx = question(&mut state, &"x".repeat(len));
        // The field is open and types `c`: Esc leaves it, then `c` copies.
        handle_key(&mut state, PaneKey::Esc, &f);
        let outcome = handle_key(&mut state, PaneKey::Char('c'), &f);
        match (copied, outcome) {
            (true, PaneOutcome::Copy(text)) => assert_eq!(text.len(), COPY_MAX_BYTES),
            (false, PaneOutcome::Consumed) => {}
            (_, other) => panic!("{len}: {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(state.decisions.len(), 1, "a copy decides nothing");
    }
    // A sanitized paint, an original copy.
    let (mut state, _clock) = clocked();
    let _rx = question(&mut state, "Name?\u{1b}[31m red\u{7}");
    let text = draw(&state, &f, 80, 24);
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    assert!(text.contains("Name?"));
    handle_key(&mut state, PaneKey::Esc, &f);
    assert_eq!(
        handle_key(&mut state, PaneKey::Char('c'), &f),
        PaneOutcome::Copy("Name?\u{1b}[31m red\u{7}".into())
    );
}

#[test]
fn story21_6_every_frame_copies_on_c_and_a_text_field_does_not() {
    let f = facts(CONV);
    for kind in PLAIN {
        let (mut state, _clock) = clocked();
        raise_plain(&mut state, kind);
        // Copy is never inert: no paint is needed.
        assert!(
            matches!(handle_key(&mut state, PaneKey::Char('c'), &f), PaneOutcome::Copy(text) if !text.is_empty()),
            "{kind:?}"
        );
        assert_eq!(state.decisions.len(), 1);
    }
}

#[test]
fn story21_6_the_plan_card_marker_is_a_queue_query() {
    let (mut state, _clock) = clocked();
    assert!(!plan_card_pending(&state, "p1"));
    raise_plain(&mut state, DecisionKind::PlanCard);
    assert!(plan_card_pending(&state, "p1"));
    assert!(!plan_card_pending(&state, "other"));
    assert_eq!(plan_card_snapshot(&state, "p1").unwrap().title, "Ship");
    presentation::reducer::close(&mut state, Surface::Decision);
    withdraw_plan_card(&mut state, "p1");
    assert!(!plan_card_pending(&state, "p1"));
    assert!(state.decisions.is_empty());
}

#[test]
fn story21_6_untrusted_card_text_is_sanitized_at_paint() {
    let f = facts(CONV);
    let (mut state, _clock) = clocked();
    raise_plan_deviation(
        &mut state,
        CONV,
        PlanDeviationPayload {
            plan_id: "p1".into(),
            kind: PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
            original: 3,
            changed: vec![2],
            summary: "evil\u{1b}[2Jtext".into(),
        },
    );
    let text = draw(&state, &f, 80, 24);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(
        text.contains("eviltext"),
        "the complete cleaned summary paints: {text}"
    );
    let (mut state, _clock) = clocked();
    raise_agent_kill(&mut state, CONV, AgentId::new(), "bad\u{7}name".into());
    let text = draw(&state, &f, 80, 24);
    assert!(!text.contains('\u{7}'));
    assert!(
        text.contains("Kill badname and its subtree?"),
        "the complete cleaned label paints: {text}"
    );
}

#[test]
fn story21_6_the_decision_contexts_are_children_with_the_rekeyed_rows() {
    use crate::adapters::tui::keymap::table_card::CardCmd;
    use crate::adapters::tui::keymap::{CommandId, Host, Key, Resolved, resolve};
    for context in [
        Context::QuestionIdle,
        Context::SkillTrust,
        Context::SkillTrustInspect,
        Context::PlanApproval,
        Context::PlanDecision,
        Context::SkipCascade,
        Context::Delegation,
        Context::SpawnGate,
        Context::DecisionConfirm,
        Context::Forget,
        Context::Rewind,
        Context::ExportOverwrite,
    ] {
        assert_eq!(context.parents(), &[Context::Decision], "{context:?}");
        for key in [
            Key::Ctrl('q'),
            Key::PageUp,
            Key::PageDown,
            Key::Ctrl('x'),
            Key::Ctrl('p'),
        ] {
            assert!(
                matches!(resolve(context, Host::Local, &[key]), Resolved::Hit(row) if row.context == Context::Decision),
                "{context:?} {key:?}"
            );
        }
    }
    assert_eq!(Context::Question.parents(), &[Context::DecisionCompose]);
    let hit = |context, key| match resolve(context, Host::Local, &[key]) {
        Resolved::Hit(row) => Some((row.context, row.command)),
        _ => None,
    };
    assert_eq!(
        hit(Context::SpawnGate, Key::Char('t')),
        Some((Context::SpawnGate, CommandId::Card(CardCmd::Cap)))
    );
    assert_eq!(
        hit(Context::SkipCascade, Key::Char('a')),
        Some((Context::SkipCascade, CommandId::Card(CardCmd::Continue)))
    );
    // `c` is the parent's Copy row in every card context, never Cap or Continue.
    for context in [
        Context::SpawnGate,
        Context::SkipCascade,
        Context::SkillTrust,
    ] {
        assert_eq!(
            hit(context, Key::Char('c')).map(|(owner, _)| owner),
            Some(Context::Decision)
        );
    }
    // The field contexts type `c`.
    assert_eq!(hit(Context::Question, Key::Char('c')), None);
    assert_eq!(hit(Context::DecisionCompose, Key::Char('c')), None);
    assert_eq!(
        hit(Context::Question, Key::Ctrl('d')),
        Some((Context::Question, CommandId::Card(CardCmd::Skip)))
    );
    assert_eq!(
        hit(Context::Question, Key::Enter).map(|(_, command)| command),
        Some(CommandId::Card(CardCmd::Send))
    );
    assert_eq!(
        hit(Context::QuestionIdle, Key::Enter).map(|(_, command)| command),
        Some(CommandId::Card(CardCmd::Answer))
    );
    // The chat-scroll rows of skill trust ended with the card.
    assert_eq!(
        hit(Context::SkillTrust, Key::Char('j')),
        None,
        "j is no chat scroll on a decision frame"
    );
}

fn raw_key(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    kind: crossterm::event::KeyEventKind,
) -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code, modifiers, kind,
    ))
}

/// 6C-L2-03: every new context's deciding keys, from the raw terminal event through
/// `app::handle_operator_input` (the guard that sees the kind and modifiers before
/// conversion drops them). Question/QuestionIdle Enter and Ctrl+D are in K7.
#[test]
fn story21_6_raw_ingress_every_deciding_key_needs_a_plain_press() {
    use crate::adapters::tui::app::{
        convert_crossterm_event, frame_ignores_raw_key, handle_operator_input,
    };
    use crate::adapters::tui::keymap::Host;
    use DecisionEffect as E;
    use crossterm::event::{KeyCode as K, KeyEventKind, KeyModifiers as M};
    let c = K::Char;
    // (kind, key, the effect a plain Press runs; None = the key edits the item in place)
    let table: [(DecisionKind, K, Option<DecisionEffect>); 33] = [
        (DecisionKind::SkillTrust, c('y'), Some(E::SkillTrustAccept)),
        (DecisionKind::SkillTrust, c('n'), Some(E::SkillTrustDecline)),
        (DecisionKind::SkillTrust, c('i'), Some(E::SkillTrustInspect)),
        (
            DecisionKind::PlanApproval,
            c('y'),
            Some(E::PlanApproveNormal),
        ),
        (
            DecisionKind::PlanApproval,
            c('a'),
            Some(E::PlanApproveAutoEdit),
        ),
        (DecisionKind::PlanApproval, c('n'), Some(E::PlanReject)),
        (DecisionKind::PlanApproval, c('e'), Some(E::PlanRevise)),
        (DecisionKind::PlanCard, c('y'), Some(E::PlanCardApprove)),
        (DecisionKind::PlanCard, c('e'), Some(E::PlanCardEdit)),
        (DecisionKind::PlanCard, c('n'), Some(E::PlanCardReject)),
        (
            DecisionKind::PlanDeviation,
            c('y'),
            Some(E::PlanDeviationApprove),
        ),
        (
            DecisionKind::PlanDeviation,
            c('e'),
            Some(E::PlanDeviationEdit),
        ),
        (
            DecisionKind::PlanDeviation,
            c('n'),
            Some(E::PlanDeviationReject),
        ),
        (DecisionKind::SkipCascade, c('s'), Some(E::SkipCascadeSkip)),
        (
            DecisionKind::SkipCascade,
            c('a'),
            Some(E::SkipCascadeContinue),
        ),
        (
            DecisionKind::SkipCascade,
            c('n'),
            Some(E::SkipCascadeCancel),
        ),
        (DecisionKind::Delegation, c('d'), Some(E::DelegateTask)),
        (DecisionKind::Delegation, c('l'), Some(E::RunTaskLocally)),
        (
            DecisionKind::Delegation,
            c('x'),
            Some(E::DelegationCardCancel),
        ),
        (DecisionKind::SpawnGate, K::Enter, Some(E::SpawnGateConfirm)),
        (DecisionKind::SpawnGate, c('t'), None),
        (DecisionKind::SpawnGate, c('n'), Some(E::SpawnGateCancel)),
        (DecisionKind::CancelPlan, c('y'), Some(E::CancelPlanConfirm)),
        (DecisionKind::CancelPlan, c('n'), Some(E::CancelPlanDecline)),
        (DecisionKind::AgentKill, c('y'), Some(E::AgentKillConfirm)),
        (DecisionKind::AgentKill, c('n'), Some(E::AgentKillDecline)),
        // Upper-case deciders are the same keys (a SHIFT press is ignored below).
        (DecisionKind::SkillTrust, c('Y'), None),
        (DecisionKind::PlanApproval, c('A'), None),
        (DecisionKind::SkipCascade, c('S'), None),
        (DecisionKind::Delegation, c('L'), None),
        (DecisionKind::SpawnGate, c('T'), None),
        (DecisionKind::AgentKill, c('N'), None),
        (DecisionKind::PlanDeviation, c('E'), None),
    ];
    let ignored = [
        (M::NONE, KeyEventKind::Repeat),
        (M::NONE, KeyEventKind::Release),
        (M::SHIFT, KeyEventKind::Press),
        (M::ALT, KeyEventKind::Press),
        (M::CONTROL, KeyEventKind::Press),
    ];
    let f = facts(CONV);
    let mouse = crate::domain::models::MouseConfig::default();
    // A model skill prompt has a receiver, so "no grant delivered" is observable.
    let build = |kind: DecisionKind| {
        let (mut state, clock) = clocked();
        let mut grant = None;
        if kind == DecisionKind::SkillTrust {
            let (tx, rx) = oneshot::channel();
            raise_model_skill_trust(&mut state, "review".into(), "/w/skill.md".into(), tx);
            grant = Some(rx);
        } else {
            raise_plain(&mut state, kind);
        }
        ready(&mut state, &clock);
        (state, grant)
    };
    for (kind, code, effect) in table {
        let uppercase = matches!(code, K::Char(ch) if ch.is_ascii_uppercase());
        for (modifiers, key_kind) in ignored {
            // A plain Ctrl+X press is the registered leader chord, never a modified decision
            // key (6E-L1-04): it reaches the leader instead of being swallowed.
            if matches!(code, K::Char('x' | 'X'))
                && modifiers == M::CONTROL
                && key_kind == KeyEventKind::Press
            {
                continue;
            }
            let (mut state, mut grant) = build(kind);
            let at = format!("{kind:?} {code:?} {modifiers:?} {key_kind:?}");
            let event = raw_key(code, modifiers, key_kind);
            assert!(frame_ignores_raw_key(&state, &event), "{at}");
            let domain = convert_crossterm_event(&event, &mouse).expect("a key converts");
            assert_eq!(
                handle_operator_input(&mut state, &event, &domain),
                None,
                "{at}"
            );
            assert_eq!(state.decisions.len(), 1, "{at}");
            assert!(state.decisions.take_resolved().is_none(), "{at}");
            assert!(
                grant.as_mut().is_none_or(|rx| rx.try_recv().is_err()),
                "{at}: no grant delivered"
            );
        }
        if uppercase {
            continue;
        }
        // Positive control: the unmodified Press is not dropped by the guard ...
        let (mut state, _grant) = build(kind);
        let event = raw_key(code, M::NONE, KeyEventKind::Press);
        let at = format!("{kind:?} {code:?} plain press");
        assert!(!frame_ignores_raw_key(&state, &event), "{at}");
        let domain = convert_crossterm_event(&event, &mouse).expect("a key converts");
        assert!(
            handle_operator_input(&mut state, &event, &domain).is_some(),
            "{at}"
        );
        // ... and decides on the tab it belongs to (the event loop routes with the facts).
        let (mut state, _grant) = build(kind);
        let routed = presentation::route(&mut state, &domain, Host::Local, &f);
        match effect {
            Some(effect) => assert_eq!(
                routed,
                Some(presentation::RouteEffect::Action(effect.into_action())),
                "{at}"
            ),
            None => assert_eq!(routed, Some(presentation::RouteEffect::Consumed), "{at}"),
        }
    }
}

/// 6C-L2-05: `j`/`k` scroll the skill file reader exactly like the arrows, to the same
/// bottom and top rows.
#[test]
fn story21_6_skill_inspect_j_and_k_scroll_to_the_same_rows_as_the_arrows() {
    let f = LogFacts::default();
    let run = |down: PaneKey, up: PaneKey| {
        let (mut state, clock) = clocked();
        let (tx, _rx) = oneshot::channel();
        raise_model_skill_trust(&mut state, "review".into(), "/w/skill.md".into(), tx);
        ready(&mut state, &clock);
        handle_key(&mut state, PaneKey::Char('i'), &f);
        let (key, _path) = inspect_target(&state).expect("the file is unread");
        let content = (0..200)
            .map(|n| format!("line-{n:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        open_inspect(&mut state, &key, content);
        handle_key(&mut state, down, &f);
        let one_down = state.decisions.scroll();
        for _ in 0..400 {
            handle_key(&mut state, down, &f);
        }
        let bottom = state.decisions.scroll();
        let bottom_text = draw(&state, &f, 80, 24);
        handle_key(&mut state, up, &f);
        let one_up = state.decisions.scroll();
        for _ in 0..400 {
            handle_key(&mut state, up, &f);
        }
        (
            one_down,
            bottom,
            bottom_text.contains("line-199"),
            one_up,
            state.decisions.scroll(),
        )
    };
    // 201 rows (header + 200 lines): the last reachable offset is 200.
    assert_eq!(run(PaneKey::Down, PaneKey::Up), (1, 200, true, 199, 0));
    assert_eq!(
        run(PaneKey::Char('j'), PaneKey::Char('k')),
        (1, 200, true, 199, 0)
    );
}
