//! Stage invocation selection; the runtime retains sidebar/tab fallthrough.
use crate::adapters::tui::{state::TuiState, widgets::chat_pane};
use crate::domain::models::{
    AnchorMode, Turn, TurnPart,
    turn::{tool_call_id_for, tool_call_id_matches},
    view_state::ViewState,
};

#[derive(Debug, PartialEq, Eq)]
pub enum CycleOutcome {
    Cycled { tool_id: String },
    Fallthrough,
}

/// Cycle the explicit turn, or the exact render-focused call's owning turn.
///
/// Only offscreen targets move the owning view. The runtime installs the
/// returned selection and synchronizes its scroll snapshots, as for J/K.
pub fn cycle_invocation(
    state: &mut TuiState,
    view: &mut ViewState,
    turns: &[Turn],
    open_turn: Option<&Turn>,
) -> CycleOutcome {
    let turn = if let Some(id) = &view.focused_turn {
        open_turn
            .into_iter()
            .chain(turns.iter())
            .find(|turn| &turn.id == id)
    } else {
        state.focused_tool_id.as_deref().and_then(|focused| {
            open_turn.into_iter().chain(turns.iter()).find(|turn| {
                turn.parts.iter().any(|part| {
                    matches!(part, TurnPart::ToolInvocation { id, .. }
                        if tool_call_id_matches(&turn.id, *id, focused))
                })
            })
        })
    };
    let Some(turn) = turn else {
        return CycleOutcome::Fallthrough;
    };
    if chat_pane::effective_is_collapsed(turn, view) {
        return CycleOutcome::Fallthrough;
    }
    let invocations = || {
        turn.parts.iter().filter_map(|part| match part {
            TurnPart::ToolInvocation { id, .. } => Some(*id),
            _ => None,
        })
    };
    let mut count = 0;
    let mut current = None;
    for (index, id) in invocations().enumerate() {
        count += 1;
        if state
            .focused_tool_id
            .as_deref()
            .is_some_and(|focused| tool_call_id_matches(&turn.id, id, focused))
        {
            current = Some(index);
        }
    }
    if count < 2 {
        return CycleOutcome::Fallthrough;
    }
    let next = current.map_or(0, |index| (index + 1) % count);
    let id = invocations()
        .nth(next)
        .expect("the counted invocation exists");
    // Use the actual last-painted global header start, including message
    // spacing and metadata prefixes; a turn-local cache offset is insufficient.
    let Some(render) = state.tab_render_states.get(&state.active_tab_id) else {
        return CycleOutcome::Fallthrough;
    };
    let Some((header, _)) = render
        .tool_block_boundaries
        .iter()
        .find(|(_, tool_id)| tool_call_id_matches(&turn.id, id, tool_id))
    else {
        return CycleOutcome::Fallthrough;
    };
    if view.focused_turn.is_none() {
        view.set_focused_turn(Some(turn.id.clone()));
    }
    let viewport = usize::from(state.viewport_height);
    let max_offset = state.total_content_height.saturating_sub(viewport);
    let top = max_offset.saturating_sub(view.scroll_offset);
    if *header < top || *header >= top.saturating_add(viewport) {
        view.scroll_offset = max_offset.saturating_sub(*header);
        view.mode = if view.scroll_offset == 0 {
            AnchorMode::Following
        } else {
            AnchorMode::Reading
        };
    }
    CycleOutcome::Cycled {
        tool_id: tool_call_id_for(&turn.id, id),
    }
}
