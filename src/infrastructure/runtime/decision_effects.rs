//! The synchronous bodies of the interim decision effects (Story 21.6 K3b).
//!
//! The event loop's `InputAction::Decision` arm owns only the I/O (spawning the
//! compaction, saving the fresh conversation, persisting the budget pause); what
//! each effect *changes* and which request it issues lives here, so the keystone
//! drives the same code the arm runs.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::adapters::tui::decision_queue::DecisionEffect;
use crate::adapters::tui::handlers::{self, HandlerOutcome};
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::state::TuiState;
use crate::domain::events::{AppEvent, CompactionPurpose};
use crate::domain::models::tab::TabManager;
use crate::domain::models::{
    AppConfig, Conversation, FocusState, SessionManager, StatusState, StreamingState,
    generate_conversation_id,
};
use crate::domain::ports::{ProviderInfoPort, StreamingProvider};
use crate::domain::services::session_index::SessionIndex;
use crate::domain::services::turn_queue::TurnQueue;
use crate::infrastructure::runtime::event_loop::{load_active_tab, save_active_tab};
use crate::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;

/// `RecoveryNewSession`: the conversation becomes a fresh one (new identity, no
/// messages). The caller persists it.
pub fn start_new_session(conversation: &mut Conversation, state: &mut TuiState) {
    conversation.messages.clear();
    conversation.title = String::new();
    conversation.id = generate_conversation_id();
    conversation.session_id = Some(generate_conversation_id());
    conversation.created_at = crate::domain::models::session_meta::now_unix();
    conversation.updated_at = crate::domain::models::session_meta::now_unix();
    conversation.last_response_at = None;
    conversation.usage = None;
    if state.modal_stack.is_empty() {
        reducer::set_base(state, FocusState::Input);
    }
}

/// `CarryoverAccept` / `ContextCompact`: ask for a compaction of the active
/// conversation (`Carryover` / `Inline`). The caller spawns the returned request.
#[allow(clippy::too_many_arguments)]
pub fn request_compaction(
    effect: DecisionEffect,
    state: &mut TuiState,
    conversation: &Conversation,
    streaming: &StreamingState,
    provider: &Arc<dyn StreamingProvider>,
    config: &AppConfig,
    domain_tx: &mpsc::UnboundedSender<AppEvent>,
    info: &dyn ProviderInfoPort,
) -> HandlerOutcome {
    let purpose = match effect {
        DecisionEffect::CarryoverAccept => CompactionPurpose::Carryover,
        DecisionEffect::ContextCompact => CompactionPurpose::Inline,
        other => unreachable!("{other:?} does not compact"),
    };
    handlers::compaction::handle_trigger_compaction(
        conversation,
        streaming,
        state,
        provider,
        config,
        domain_tx,
        purpose,
        info,
    )
}

/// `CarryoverDecline` (Story 7.4 AC10): open a fresh tab immediately, no carryover.
#[allow(clippy::too_many_arguments)]
pub fn open_fresh_tab(
    turn_lifecycle: &mut TuiTurnLifecycle,
    tab_manager: &mut TabManager,
    conversation: &mut Conversation,
    streaming: &mut StreamingState,
    session_manager: &mut SessionManager,
    state: &mut TuiState,
    turn_queue: &mut TurnQueue,
    session_index: &mut SessionIndex,
) {
    turn_lifecycle.interrupt_active(
        tab_manager,
        conversation,
        streaming,
        &mut state.pending_resolved_model,
    );
    *streaming = StreamingState::default();
    save_active_tab(
        tab_manager,
        conversation,
        streaming,
        session_manager,
        state,
        turn_queue,
    );
    tab_manager.create_tab();
    load_active_tab(
        tab_manager,
        conversation,
        streaming,
        session_manager,
        state,
        turn_queue,
    );
    state.input_buffer.clear();
    state.cursor_position = 0;
    reducer::settle_stage_base(state, FocusState::Input);
    state.status = StatusState::Idle;
    state.active_agent_name = None;
    session_index.set_open(&conversation.id, true);
    session_index.set_active(Some(&conversation.id));
}

/// `BudgetContinue`: hold for this day and limit (A18).
pub fn budget_continue(state: &mut TuiState, info: &dyn ProviderInfoPort) {
    handlers::budget::acknowledge_daily_budget(state, info);
}

/// `BudgetSwitchCheaper`: the arm only opens the model selector (no model is chosen
/// for the user).
pub fn budget_switch_cheaper(
    state: &mut TuiState,
    info: &dyn ProviderInfoPort,
    config: &AppConfig,
) {
    handlers::model_switch::open_selector(state, info, config, None);
}

/// `BudgetPause`, after the dismissal was persisted: suppress the warning until
/// `until_unix` and withdraw the pending decision.
pub fn apply_budget_pause(state: &mut TuiState, until_unix: i64) {
    if let Some(db) = state.daily_budget.as_mut() {
        db.dismissed_until_unix = until_unix;
    }
    handlers::budget::handle_clear_daily_budget_warning(state);
}
