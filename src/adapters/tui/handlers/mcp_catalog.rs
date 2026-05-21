//! MCP catalog change handler — Story 9.2.
//!
//! Extracted from event_loop.rs per ADR-08-01 §D4.

use crate::adapters::command_registry::CommandRegistry;
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::autocomplete::AutocompleteKind;
use crate::infrastructure::runtime::event_loop::populate_autocomplete_suggestions;

/// Handle MCP catalog change event.
/// Refreshes autocomplete if dropdown is open with McpMention filter.
#[cfg(feature = "mcp")]
pub async fn handle_mcp_catalog_changed(
    state: &mut TuiState,
    command_registry: &mut CommandRegistry,
    workspace_path: &std::path::Path,
    tools: &std::sync::Arc<dyn crate::domain::ports::ToolSetPort>,
    server_id: &str,
    tool_count: usize,
) {
    tracing::info!(target: "mcp", %server_id, %tool_count, "MCP tool catalog changed");
    // P-17, P-27: Refresh autocomplete if dropdown is open with McpMention
    if state.autocomplete.active && state.autocomplete.kind == AutocompleteKind::McpMention {
        populate_autocomplete_suggestions(state, command_registry, workspace_path, tools).await;
    }
    state.needs_redraw = true;
}
