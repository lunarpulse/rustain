//! Stage item selection (tool calls, code blocks, tables); the runtime retains
//! tab fallthrough.
use crate::adapters::tui::{state::TuiState, widgets::chat_pane};
use crate::domain::models::{AnchorMode, Turn, turn::item_belongs_to_turn, view_state::ViewState};

#[derive(Debug, PartialEq, Eq)]
pub enum CycleOutcome {
    Cycled { item_id: String },
    Fallthrough,
}

/// Cycle the explicit turn, or the exact render-focused item's owning turn,
/// through every item of that turn in render order.
///
/// Only offscreen targets move the owning view. The runtime installs the
/// returned selection and synchronizes its scroll snapshots, as for J/K.
pub fn cycle_item(
    state: &mut TuiState,
    view: &mut ViewState,
    turns: &[Turn],
    open_turn: Option<&Turn>,
) -> CycleOutcome {
    let mut candidates = open_turn.into_iter().chain(turns.iter());
    let turn = if let Some(id) = &view.focused_turn {
        candidates.find(|turn| &turn.id == id)
    } else {
        state
            .focused_item_id
            .as_deref()
            .and_then(|focused| candidates.find(|turn| item_belongs_to_turn(turn, focused)))
    };
    let Some(turn) = turn else {
        return CycleOutcome::Fallthrough;
    };
    if chat_pane::effective_is_collapsed(turn, view) {
        return CycleOutcome::Fallthrough;
    }
    // Use the actual last-painted global header starts, including message
    // spacing and metadata prefixes; a turn-local cache offset is insufficient.
    let Some(render) = state.tab_render_states.get(&state.active_tab_id) else {
        return CycleOutcome::Fallthrough;
    };
    let items: Vec<&(usize, String)> = render
        .item_boundaries
        .iter()
        .filter(|(_, id)| item_belongs_to_turn(turn, id))
        .collect();
    if items.len() < 2 {
        return CycleOutcome::Fallthrough;
    }
    let current = items
        .iter()
        .position(|(_, id)| state.focused_item_id.as_deref() == Some(id.as_str()));
    let (header, item_id) = items[current.map_or(0, |index| (index + 1) % items.len())];
    let (header, item_id) = (*header, item_id.clone());
    if view.focused_turn.is_none() {
        view.set_focused_turn(Some(turn.id.clone()));
    }
    let viewport = usize::from(state.viewport_height);
    let max_offset = state.total_content_height.saturating_sub(viewport);
    let top = max_offset.saturating_sub(view.scroll_offset);
    if header < top || header >= top.saturating_add(viewport) {
        view.scroll_offset = max_offset.saturating_sub(header);
        view.mode = if view.scroll_offset == 0 {
            AnchorMode::Following
        } else {
            AnchorMode::Reading
        };
    }
    CycleOutcome::Cycled { item_id }
}
