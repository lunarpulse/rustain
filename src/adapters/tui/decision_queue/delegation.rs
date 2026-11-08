//! Delegation, the spawn gate and the agent kill confirmation as queue items
//! (Story 21.6 6-C). The payloads moved here from `TuiState` /
//! `AgentPanelState` (AD-2110). All three belong to one conversation: their effects act
//! on the active tab, so an item for another tab waits, counted.

use super::{
    Arrival, DecisionDraft, DecisionKind, DecisionSubject, NO_LONGER_PENDING, Raised, raise,
    withdraw_key,
};
use crate::adapters::tui::fanout_spec::FanOutSpec;
use crate::adapters::tui::state::TuiState;
use crate::domain::models::AgentId;
use crate::domain::models::orchestration::FORK_JOIN_SPAWN_CAP;
use crate::domain::models::tab::ConversationId;
use crate::domain::services::delegation_decider::DelegationSuggestion;

/// Story 10.5: a delegation suggestion awaiting `d` / `l` / `x`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelegationCard {
    pub conversation_id: ConversationId,
    pub plan_id: String,
    pub task_number: u32,
    pub suggestion: DelegationSuggestion,
}

/// A `/fanout` above the learned threshold awaiting confirmation (Story 14.3a).
#[derive(Debug, Clone)]
pub struct PendingSpawnGate {
    pub spec: FanOutSpec,
    pub requested: usize,
    pub threshold: usize,
    pub adjusted: Option<usize>,
}

/// A gate is the same decision while its request is: the spec itself has no equality.
impl PartialEq for PendingSpawnGate {
    fn eq(&self, other: &Self) -> bool {
        self.requested == other.requested
            && self.threshold == other.threshold
            && self.adjusted == other.adjusted
    }
}

impl Eq for PendingSpawnGate {}

impl PendingSpawnGate {
    #[must_use]
    pub fn effective_n(&self) -> usize {
        self.adjusted.unwrap_or(self.requested)
    }

    pub fn adjust_left(&mut self) {
        self.adjusted = Some(self.effective_n().saturating_sub(1).max(1));
    }

    pub fn adjust_right(&mut self) {
        self.adjusted = Some(
            self.effective_n()
                .saturating_add(1)
                .min(FORK_JOIN_SPAWN_CAP),
        );
    }

    pub fn cap_at_threshold(&mut self) {
        self.adjusted = Some(self.threshold.max(1).min(self.requested));
    }
}

/// Killing an agent and its subtree awaiting confirmation (the inspector's `x`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKillPayload {
    pub agent_id: AgentId,
    /// The agent's display label (its subagent type), as the inspector named it.
    pub label: String,
}

/// Raise a delegation suggestion (Automatic arrival, keyed to its plan task).
pub fn raise_delegation(state: &mut TuiState, card: PendingDelegationCard) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Delegation, "")
        .conversation(card.conversation_id.clone())
        .source(format!("{}:{}", card.plan_id, card.task_number))
        .summary(format!("task {}", card.task_number))
        .subject(DecisionSubject::Delegation(Box::new(card)));
    raise(state, draft, Arrival::Automatic)
}

/// Raise the spawn gate of a `/fanout` (the user asked: Requested arrival). One gate per
/// conversation: a second request updates it in place.
pub fn raise_spawn_gate(
    state: &mut TuiState,
    conversation_id: &str,
    gate: PendingSpawnGate,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::SpawnGate, "")
        .conversation(conversation_id)
        .summary("/fanout")
        .subject(DecisionSubject::SpawnGate(Box::new(gate)));
    raise(state, draft, Arrival::Requested)
}

/// Raise the kill confirmation of `agent_id` (the user asked: Requested arrival).
pub fn raise_agent_kill(
    state: &mut TuiState,
    conversation_id: &str,
    agent_id: AgentId,
    label: String,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::AgentKill, "")
        .conversation(conversation_id)
        .source(agent_id.as_str().to_owned())
        .summary(label.clone())
        .subject(DecisionSubject::AgentKill(Box::new(AgentKillPayload {
            agent_id,
            label,
        })));
    raise(state, draft, Arrival::Requested)
}

/// The agent's kill confirmation is moot: the agent is gone.
pub fn withdraw_agent_kill(state: &mut TuiState, agent_id: &AgentId) {
    let key = state
        .decisions
        .items()
        .find(|item| item.kind == DecisionKind::AgentKill && item.source == agent_id.as_str())
        .map(super::DecisionItem::key);
    if let Some(key) = key {
        withdraw_key(state, &key, NO_LONGER_PENDING);
    }
}

/// The agent cache was refreshed: every kill confirmation whose agent is no longer in it
/// is moot (an inspector merely closed over a live agent keeps its deferred decision).
pub fn withdraw_gone_agent_kills(state: &mut TuiState) {
    let gone: Vec<AgentId> = state
        .decisions
        .items()
        .filter_map(|item| match &item.subject {
            DecisionSubject::AgentKill(kill)
                if !state
                    .agent_panel_state
                    .cached_entries
                    .iter()
                    .any(|entry| entry.agent_id == kill.agent_id) =>
            {
                Some(kill.agent_id.clone())
            }
            _ => None,
        })
        .collect();
    for agent_id in &gone {
        withdraw_agent_kill(state, agent_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(requested: usize, threshold: usize) -> PendingSpawnGate {
        PendingSpawnGate {
            spec: FanOutSpec::Identical {
                count: requested,
                prompt: "t".into(),
            },
            requested,
            threshold,
            adjusted: None,
        }
    }

    #[test]
    fn story21_6_the_gate_adjusts_inside_one_and_the_spawn_cap() {
        let mut g = gate(20, 8);
        g.adjust_left();
        assert_eq!(g.effective_n(), 19);
        g.cap_at_threshold();
        assert_eq!(g.effective_n(), 8);
        let mut low = gate(1, 8);
        low.adjust_left();
        assert_eq!(low.effective_n(), 1, "never below one");
        let mut high = gate(FORK_JOIN_SPAWN_CAP, 8);
        high.adjust_right();
        assert_eq!(
            high.effective_n(),
            FORK_JOIN_SPAWN_CAP,
            "never above the cap"
        );
    }

    #[test]
    fn story21_6_a_kill_confirmation_retires_when_its_agent_leaves_the_cache() {
        use crate::domain::models::NodeState;
        use crate::domain::models::subagent_view::{AgentRowView, OwnershipKind};
        let row = |agent_id: &AgentId| AgentRowView {
            isolated: false,
            agent_id: agent_id.clone(),
            parent_id: AgentId::root(),
            subagent_type: "reviewer".into(),
            spawned_at: 0,
            depth: 1,
            current_status: NodeState::Running,
            ownership: OwnershipKind::Owned,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        };
        let (live, gone) = (AgentId::new(), AgentId::new());
        let mut state = TuiState::new(80, 24);
        raise_agent_kill(&mut state, "c1", live.clone(), "reviewer".into());
        raise_agent_kill(&mut state, "c1", gone.clone(), "reviewer".into());
        state.agent_panel_state.cached_entries = vec![row(&live), row(&gone)];
        withdraw_gone_agent_kills(&mut state);
        assert_eq!(state.decisions.len(), 2, "both agents are still cached");
        // The cache refresh no longer lists `gone`.
        state.agent_panel_state.cached_entries = vec![row(&live)];
        withdraw_gone_agent_kills(&mut state);
        assert!(
            state
                .decisions
                .items()
                .filter(|item| !item.is_stale())
                .all(|item| item.source == live.as_str()),
            "the gone agent's confirmation is retired (stale or removed)"
        );
        assert!(state.decisions.count_live(DecisionKind::AgentKill) <= 1);
        assert!(
            state
                .decisions
                .items()
                .any(|item| item.source == live.as_str() && !item.is_stale()),
            "the live agent's deferred decision stays"
        );
    }
}
