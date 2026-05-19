//! `/compact` slash command handler — extracted from event_loop.rs (Story 8.5 Option A).

use crate::adapters::tui::handlers::compaction;
use crate::adapters::tui::handlers::HandlerOutcome;
use crate::domain::events::CompactionPurpose;
use crate::infrastructure::runtime::app_context::AppContext;

pub fn handle_compact_slash(
    conversation: &crate::domain::models::Conversation,
    streaming: &crate::domain::models::StreamingState,
    state: &mut crate::adapters::tui::state::TuiState,
    provider: &std::sync::Arc<dyn crate::domain::ports::StreamingProvider>,
    config: &crate::domain::models::AppConfig,
    domain_tx: &tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
    app_context: &AppContext,
) -> HandlerOutcome {
    compaction::handle_trigger_compaction(
        conversation,
        streaming,
        state,
        provider,
        config,
        domain_tx,
        CompactionPurpose::Inline,
        app_context,
    )
}
