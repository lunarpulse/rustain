//! A2A AgentCard catalog change handler — Story 17.4a.

use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use crate::adapters::tui::state::TuiState;

pub async fn handle_a2a_catalog_changed(
    state: &mut TuiState,
    tools: &std::sync::Arc<dyn crate::domain::ports::ToolSetPort>,
    peer_id: &str,
    skill_count: usize,
) {
    tracing::info!(target: "a2a", %peer_id, %skill_count, "A2A AgentCard catalog changed");
    if let Some(composite) = tools.as_any().downcast_ref::<CompositeToolsetAdapter>()
        && let Err(error) = composite.populate_registry().await
    {
        tracing::debug!(%error, "populate_registry failed on A2aCatalogChanged");
    }
    state.needs_redraw = true;
}
