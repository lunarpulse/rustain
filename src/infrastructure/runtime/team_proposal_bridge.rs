//! Standalone proposal lifecycle. Raising a card never performs recipient I/O.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::domain::events::AppEvent;
use crate::domain::models::{ProposalRaiseOutcome, TeamProposal};
use crate::domain::ports::TeamProposalSink;
use crate::infrastructure::runtime::event_bus::EventBus;

/// Registered conversation slots are the live operator surface. Removing the
/// slot also rejects a late raise from a turn being closed or reset.
pub struct StandaloneTeamProposalSink {
    conversations: Mutex<HashMap<String, Option<Arc<TeamProposal>>>>,
    events: Arc<EventBus>,
}

impl StandaloneTeamProposalSink {
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            conversations: Mutex::new(HashMap::new()),
            events,
        }
    }

    pub async fn attach_conversation(&self, conversation_id: &str) {
        let mut conversations = self.conversations.lock().await;
        if !conversations.contains_key(conversation_id) {
            conversations.insert(conversation_id.to_owned(), None);
        }
    }

    pub async fn pending(&self, conversation_id: &str) -> Option<Arc<TeamProposal>> {
        self.conversations
            .lock()
            .await
            .get(conversation_id)
            .and_then(Clone::clone)
    }

    /// One acquisition checks both bindings and destructively takes once.
    pub async fn take(
        &self,
        conversation_id: &str,
        proposal_id: &str,
    ) -> Option<Arc<TeamProposal>> {
        let mut conversations = self.conversations.lock().await;
        let pending = conversations.get_mut(conversation_id)?;
        if pending
            .as_ref()
            .is_some_and(|proposal| proposal.id == proposal_id)
        {
            pending.take()
        } else {
            None
        }
    }

    pub async fn withdraw(&self, conversation_id: &str) -> Option<Arc<TeamProposal>> {
        self.conversations
            .lock()
            .await
            .remove(conversation_id)
            .flatten()
    }
}

#[async_trait::async_trait]
impl TeamProposalSink for StandaloneTeamProposalSink {
    async fn raise(&self, proposal: Arc<TeamProposal>) -> ProposalRaiseOutcome {
        let mut conversations = self.conversations.lock().await;
        if self.events.domain_tx.is_closed() {
            conversations.remove(proposal.conversation_id.as_str());
            return ProposalRaiseOutcome::Unavailable;
        }
        let Some(pending) = conversations.get_mut(proposal.conversation_id.as_str()) else {
            return ProposalRaiseOutcome::Unavailable;
        };
        if pending.is_some() {
            return ProposalRaiseOutcome::Pending;
        }
        *pending = Some(proposal.clone());
        if self
            .events
            .emit_domain(AppEvent::TeamProposalReady { proposal })
            .is_err()
        {
            pending.take();
            return ProposalRaiseOutcome::Unavailable;
        }
        ProposalRaiseOutcome::Shown
    }
}

/// Consume the real ready event only while its exact snapshot is still pending.
pub async fn proposal_ready(
    state: &mut crate::adapters::tui::state::TuiState,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    proposal: Arc<TeamProposal>,
) {
    let Some(sink) = app_state.team_proposals.as_ref() else {
        return;
    };
    if sink
        .pending(&proposal.conversation_id)
        .await
        .is_some_and(|pending| Arc::ptr_eq(&pending, &proposal))
    {
        if state.team_proposal_conversation_id.as_deref() == Some(proposal.conversation_id.as_str())
        {
            state.team_proposal_visible = None;
        }
        state
            .pending_team_proposals
            .insert(proposal.conversation_id.clone(), proposal);
        state.needs_redraw = true;
    }
}

/// The common loop does no allocation or locking until the view changes.
pub async fn synchronize_surface(
    state: &mut crate::adapters::tui::state::TuiState,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    tabs: &crate::domain::models::tab::TabManager,
    conversation: &crate::domain::models::Conversation,
) {
    let Some(sink) = app_state.team_proposals.as_ref() else {
        return;
    };
    let active_conversation_id = (!tabs.active_tab().read_only).then_some(conversation.id.as_str());
    if state.team_proposal_conversation_id.as_deref() != active_conversation_id
        || state.team_proposal_surface_tab_count != tabs.tab_count()
    {
        let mut open = std::collections::HashSet::with_capacity(tabs.tab_count());
        if !tabs.active_tab().read_only {
            open.insert(conversation.id.as_str());
        }
        for tab in tabs
            .tabs()
            .iter()
            .filter(|tab| tab.id != tabs.active_tab_id() && !tab.read_only)
        {
            open.insert(tab.conversation.id.as_str());
        }
        let mut slots = sink.conversations.lock().await;
        slots.retain(|id, _| open.contains(id.as_str()));
        for id in &open {
            if !slots.contains_key(*id) {
                slots.insert((*id).to_owned(), None);
            }
        }
        state
            .pending_team_proposals
            .retain(|id, _| open.contains(id.as_str()));
        state.team_proposal_visible = None;
        state.team_proposal_conversation_id = active_conversation_id.map(str::to_owned);
        state.team_proposal_surface_tab_count = tabs.tab_count();
        state.needs_redraw = true;
    }
    if std::mem::take(&mut state.team_proposal_withdrawal_pending) {
        show_info(
            state,
            "team-proposal-status",
            "Proposal withdrawn with the conversation. Nothing was sent.".to_owned(),
        );
    }
}

/// Retire before resetting/closing, also fencing a queued ready event.
pub async fn withdraw_conversation(
    state: &mut crate::adapters::tui::state::TuiState,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    conversation_id: &str,
) {
    let retired = if let Some(sink) = app_state.team_proposals.as_ref() {
        sink.withdraw(conversation_id).await.is_some()
    } else {
        false
    };
    let cached = state
        .pending_team_proposals
        .remove(conversation_id)
        .is_some();
    if state.team_proposal_conversation_id.as_deref() == Some(conversation_id) {
        state.team_proposal_withdrawal_pending |= retired || cached;
        state.team_proposal_visible = None;
        state
            .tab_render_state(state.active_tab_id)
            .team_proposal_reviews
            .retain(|_, proposal| proposal.conversation_id != conversation_id);
    }
    state.team_proposal_surface_tab_count = 0;
    state.needs_redraw = true;
}

pub async fn withdraw_all(
    state: &mut crate::adapters::tui::state::TuiState,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
) {
    if let Some(sink) = app_state.team_proposals.as_ref() {
        let mut slots = sink.conversations.lock().await;
        state.team_proposal_withdrawal_pending |= state
            .team_proposal_conversation_id
            .as_ref()
            .and_then(|id| slots.get(id))
            .is_some_and(Option::is_some);
        slots.clear();
    }
    state.pending_team_proposals.clear();
    state.team_proposal_visible = None;
    state.team_proposal_surface_tab_count = 0;
    for render in state.tab_render_states.values_mut() {
        render.team_proposal_reviews.clear();
    }
    state.needs_redraw = true;
}

/// The card's key handler supplies the decision; Review is not an approval.
pub async fn proposal_action(
    state: &mut crate::adapters::tui::state::TuiState,
    app_state: &crate::infrastructure::runtime::app_state::AppState,
    active_conversation_id: &str,
    conversation_id: &str,
    proposal_id: &str,
    decision: Option<crate::domain::models::TeamProposalDecision>,
) {
    if active_conversation_id != conversation_id
        || state.team_proposal_conversation_id.as_deref() != Some(conversation_id)
        || !state.input_buffer.is_empty()
        || crate::adapters::tui::app::proposal_higher_confirmation_active(state)
        || !state.team_proposal_visible.as_ref().is_some_and(|visible| {
            visible.id == proposal_id
                && visible.conversation_id == conversation_id
                && state
                    .pending_team_proposals
                    .get(conversation_id)
                    .is_some_and(|pending| Arc::ptr_eq(pending, visible))
        })
    {
        stale(state);
        return;
    }
    let Some(sink) = app_state.team_proposals.as_ref() else {
        stale(state);
        return;
    };
    if decision.is_none() {
        let Some(proposal) = sink
            .pending(conversation_id)
            .await
            .filter(|proposal| proposal.id == proposal_id)
        else {
            stale(state);
            return;
        };
        let block_id = format!("team-proposal-review-{proposal_id}");
        show_info(
            state,
            &block_id,
            format!(
                "Proposed message (not sent) · {}",
                crate::domain::models::semantic_message_type_metadata(proposal.message_type).token
            ),
        );
        state
            .tab_render_state(state.active_tab_id)
            .team_proposal_reviews
            .insert(block_id, proposal);
        state.auto_snapshot = true;
        return;
    }
    let Some(proposal) = sink.take(conversation_id, proposal_id).await else {
        stale(state);
        return;
    };
    state.pending_team_proposals.remove(conversation_id);
    state.team_proposal_visible = None;
    state.needs_redraw = true;
    match decision.expect("review returned above") {
        crate::domain::models::TeamProposalDecision::Send => {
            super::transparency_bridge::start_proposed_team_send(
                state,
                conversation_id,
                app_state,
                proposal,
            );
        }
        crate::domain::models::TeamProposalDecision::Dismiss => {
            show_info(
                state,
                "team-proposal-status",
                "Proposal dismissed. Nothing was sent.".to_owned(),
            );
        }
    }
}

fn stale(state: &mut crate::adapters::tui::state::TuiState) {
    show_info(
        state,
        "team-proposal-status",
        "That proposal is no longer waiting. Nothing was sent.".to_owned(),
    );
}

fn show_info(state: &mut crate::adapters::tui::state::TuiState, id: &str, message: String) {
    crate::adapters::tui::handlers::team_command::show_team_send(state, id, message);
}
