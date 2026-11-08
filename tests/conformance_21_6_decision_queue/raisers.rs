//! Slice 6-C fixtures shared by K3b/K7/K8 and the migrated card witnesses: one
//! raise per kind through its REAL producer entry point (the same fns the event
//! loop arms call), keys through `presentation::route` with the active tab in the
//! facts. Nothing here writes a slot, focus or the queue directly.
#![allow(dead_code)]

use std::path::PathBuf;

use rustain::adapters::tui::decision_queue::{
    self, CancelPlanPayload, DecisionKind, PendingDelegationCard, PendingSpawnGate,
    PlanApprovalPayload, PlanCardPayload, PlanDeviationPayload, Raised, SkipCascadePending,
};
use rustain::adapters::tui::fanout_spec::FanOutSpec;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::plan::{Plan, PlanDeviationKind, PlanStatus, PlanTaskStatus};
use rustain::domain::models::{AgentId, SkillTrustResponse};
use rustain::domain::services::delegation_decider::{DelegationReason, DelegationSuggestion};
use tokio::sync::oneshot;

/// Every kind slice 6-C moved into the queue.
pub const ALL_6C: [DecisionKind; 10] = [
    DecisionKind::Question,
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

/// What a producer handed to the runtime and the test keeps to observe the answer.
pub enum Hold {
    None,
    Question(oneshot::Receiver<String>),
    Trust(oneshot::Receiver<SkillTrustResponse>),
}

pub fn plan(title: &str) -> Plan {
    Plan {
        id: "p1".into(),
        title: title.into(),
        tasks: vec![],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: None,
    }
}

pub fn skip_cascade() -> SkipCascadePending {
    SkipCascadePending {
        plan_id: "p1".into(),
        source_task: 1,
        source_prior_status: PlanTaskStatus::Pending,
        source_prior_error: None,
        downstream: vec![2, 3],
    }
}

pub fn spawn_gate() -> PendingSpawnGate {
    PendingSpawnGate {
        spec: FanOutSpec::Identical {
            count: 5,
            prompt: "payload-safe-fixture".into(),
        },
        requested: 5,
        threshold: 2,
        adjusted: None,
    }
}

pub fn delegation(agent: &str) -> PendingDelegationCard {
    PendingDelegationCard {
        conversation_id: "c1".into(),
        plan_id: "p1".into(),
        task_number: 1,
        suggestion: DelegationSuggestion {
            task_number: 1,
            agent_name: agent.into(),
            reason: DelegationReason::ExplicitAgentMention,
            auto_proceed: false,
        },
    }
}

/// Raise one item of `kind` for conversation `c1` through its producer. `text` is the
/// model- or peer-originated string the frame paints (question, skill name, plan
/// title and summary, agent name), so the sanitize witnesses can pass hostile bytes.
pub fn raise(state: &mut TuiState, kind: DecisionKind, text: &str) -> (Raised, Hold) {
    match kind {
        DecisionKind::Question => {
            let (tx, rx) = oneshot::channel();
            let raised =
                decision_queue::raise_question(state, "c1", "tool-use-1".into(), text.into(), tx);
            (raised, Hold::Question(rx))
        }
        DecisionKind::SkillTrust => {
            let (tx, rx) = oneshot::channel();
            let raised = decision_queue::raise_model_skill_trust(
                state,
                text.into(),
                PathBuf::from("/skills/example/SKILL.md"),
                tx,
            );
            (raised, Hold::Trust(rx))
        }
        DecisionKind::PlanApproval => (
            decision_queue::raise_plan_approval(
                state,
                PlanApprovalPayload {
                    conversation_id: "c1".into(),
                    plan_path: PathBuf::from("plan.md"),
                    contents: format!("# {text}\n\n1. first\n2. second"),
                    summary: text.into(),
                },
            ),
            Hold::None,
        ),
        DecisionKind::PlanCard => (
            decision_queue::raise_plan_card(
                state,
                PlanCardPayload {
                    conversation_id: "c1".into(),
                    plan_id: "p1".into(),
                    plan_snapshot: plan(text),
                },
            ),
            Hold::None,
        ),
        DecisionKind::PlanDeviation => (
            decision_queue::raise_plan_deviation(
                state,
                "c1",
                PlanDeviationPayload {
                    plan_id: "p1".into(),
                    kind: PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
                    original: 3,
                    changed: vec![2],
                    summary: text.into(),
                },
            ),
            Hold::None,
        ),
        DecisionKind::SkipCascade => (
            decision_queue::raise_skip_cascade(state, "c1", skip_cascade()),
            Hold::None,
        ),
        DecisionKind::CancelPlan => (
            decision_queue::raise_cancel_plan(
                state,
                "c1",
                CancelPlanPayload {
                    plan_id: "p1".into(),
                    title: text.into(),
                    pending: 2,
                    completed: 1,
                },
            ),
            Hold::None,
        ),
        DecisionKind::Delegation => (
            decision_queue::raise_delegation(state, delegation(text)),
            Hold::None,
        ),
        DecisionKind::SpawnGate => (
            decision_queue::raise_spawn_gate(state, "c1", spawn_gate()),
            Hold::None,
        ),
        DecisionKind::AgentKill => (
            decision_queue::raise_agent_kill(
                state,
                "c1",
                AgentId::parse("spoke-1").expect("a valid agent id"),
                text.into(),
            ),
            Hold::None,
        ),
        other => panic!("{other:?} is not a 6-C kind"),
    }
}

/// A plain-ASCII body string every kind can carry.
pub const PLAIN: &str = "Plain body text";

/// Display controls a model could smuggle in: ESC/CSI, OSC 8, C1 and a bidi override.
pub const HOSTILE: &str = "x\u{1b}[2Jy\u{1b}]8;;http://evil\u{7}z\u{85}w\u{202e}v\u{9b}u";

/// The kind's frame title (`Decision: ...`), read from the production table.
pub fn title(kind: DecisionKind) -> &'static str {
    kind.frame_title()
}

/// The rail key that RESOLVES (or, for the non-resolving kinds, the first rail key) and
/// the `[Esc]` label the kind paints: one by-value fact per kind.
pub fn deciding_key(kind: DecisionKind) -> char {
    match kind {
        DecisionKind::Question => '\n',
        DecisionKind::SkillTrust => 'n',
        DecisionKind::PlanApproval => 'n',
        DecisionKind::PlanCard => 'n',
        DecisionKind::PlanDeviation => 'n',
        DecisionKind::SkipCascade => 'n',
        DecisionKind::CancelPlan => 'n',
        DecisionKind::Delegation => 'x',
        DecisionKind::SpawnGate => 'n',
        DecisionKind::AgentKill => 'n',
        other => panic!("{other:?} is not a 6-C kind"),
    }
}

/// A second, distinct item of `kind` (other conversation, other plan/agent/skill/tool
/// id), so two queued items of one kind count twice.
pub fn raise_second(state: &mut TuiState, kind: DecisionKind) -> Hold {
    raise_second_in(state, kind, "c2")
}

/// [`raise_second`] for an explicit conversation (`"c1"` keeps it decidable where the first is).
pub fn raise_second_in(state: &mut TuiState, kind: DecisionKind, conversation: &str) -> Hold {
    match kind {
        DecisionKind::Question => {
            let (tx, rx) = oneshot::channel();
            decision_queue::raise_question(
                state,
                conversation,
                "tool-use-2".into(),
                "Second question?".into(),
                tx,
            );
            Hold::Question(rx)
        }
        DecisionKind::SkillTrust => {
            let (tx, rx) = oneshot::channel();
            decision_queue::raise_model_skill_trust(
                state,
                "second-skill".into(),
                PathBuf::from("/skills/second/SKILL.md"),
                tx,
            );
            Hold::Trust(rx)
        }
        DecisionKind::PlanApproval => {
            decision_queue::raise_plan_approval(
                state,
                PlanApprovalPayload {
                    conversation_id: conversation.into(),
                    plan_path: PathBuf::from("plan-two.md"),
                    contents: "# two".into(),
                    summary: "second".into(),
                },
            );
            Hold::None
        }
        DecisionKind::PlanCard => {
            let mut second = plan("second");
            second.id = "p2".into();
            decision_queue::raise_plan_card(
                state,
                PlanCardPayload {
                    conversation_id: conversation.into(),
                    plan_id: "p2".into(),
                    plan_snapshot: second,
                },
            );
            Hold::None
        }
        DecisionKind::PlanDeviation => {
            decision_queue::raise_plan_deviation(
                state,
                conversation,
                PlanDeviationPayload {
                    plan_id: "p2".into(),
                    kind: PlanDeviationKind::AutoSkipBlockedTasks { source_task: 1 },
                    original: 3,
                    changed: vec![2],
                    summary: "second".into(),
                },
            );
            Hold::None
        }
        DecisionKind::SkipCascade => {
            let mut second = skip_cascade();
            second.plan_id = "p2".into();
            decision_queue::raise_skip_cascade(state, conversation, second);
            Hold::None
        }
        DecisionKind::CancelPlan => {
            decision_queue::raise_cancel_plan(
                state,
                conversation,
                CancelPlanPayload {
                    plan_id: "p2".into(),
                    title: "second".into(),
                    pending: 1,
                    completed: 0,
                },
            );
            Hold::None
        }
        DecisionKind::Delegation => {
            let mut second = delegation("second");
            second.conversation_id = conversation.into();
            second.plan_id = "p2".into();
            decision_queue::raise_delegation(state, second);
            Hold::None
        }
        DecisionKind::SpawnGate => {
            decision_queue::raise_spawn_gate(state, conversation, spawn_gate());
            Hold::None
        }
        DecisionKind::AgentKill => {
            decision_queue::raise_agent_kill(
                state,
                conversation,
                AgentId::parse("spoke-2").expect("a valid agent id"),
                "second".into(),
            );
            Hold::None
        }
        other => panic!("{other:?} is not a 6-C kind"),
    }
}

/// A fresh 80x24 state with one painted item of `kind` whose inert window has passed:
/// the state a user sees when the rail is live. Keys: [`press`].
pub fn open_ready(
    kind: DecisionKind,
    text: &str,
) -> (
    TuiState,
    std::sync::Arc<rustain::domain::clock::MockClock>,
    Hold,
) {
    let (mut state, clock) = super::support::clocked(80, 24);
    let (_raised, hold) = raise(&mut state, kind, text);
    super::support::paint(&mut state, &super::support::facts("c1"), 80, 24);
    super::support::ready(&mut state, &clock);
    (state, clock, hold)
}

/// A key through the local front door the way the event loop dispatches it: the 21-4
/// router first (with the active tab `c1` in the facts), `handle_input` for the rest.
pub fn press(
    state: &mut TuiState,
    key: rustain::adapters::tui::keymap::Key,
) -> rustain::adapters::tui::app::InputAction {
    use rustain::adapters::tui::app::{InputAction, handle_input};
    use rustain::adapters::tui::keymap::Host;
    use rustain::adapters::tui::presentation;
    let Some(event) = crate::conformance_21_3_keymap::support::decode(key) else {
        return InputAction::Ignored;
    };
    match presentation::route(state, &event, Host::Local, &super::support::facts("c1")) {
        Some(effect) => effect.into_action(),
        None => handle_input(state, &event),
    }
}

/// Paint the frame (the first paint starts the inert clock) and let the window pass.
pub fn repaint_ready(state: &mut TuiState, clock: &rustain::domain::clock::MockClock) {
    super::support::paint(state, &super::support::facts("c1"), 80, 24);
    super::support::ready(state, clock);
}

/// The kind's deciding key as a registry key (the question decides with `Enter`).
pub fn deciding(kind: DecisionKind) -> rustain::adapters::tui::keymap::Key {
    use rustain::adapters::tui::keymap::Key;
    if kind == DecisionKind::Question {
        Key::Enter
    } else {
        Key::Char(deciding_key(kind))
    }
}
