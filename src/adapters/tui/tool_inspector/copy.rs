//! Key-time canonical copy effect; no payload snapshot and no Chat sentinel.

use crate::adapters::tui::{
    clipboard::ClipboardResult,
    handlers::clipboard::{notify_copy, notify_failure},
    presentation::Surface,
    state::TuiState,
};
use crate::domain::{models::Conversation, services::reducer::ReducerState};

use super::{ToolFrameKey, project, source};

/// Resolve exactly the top frame's current target/tab against its owning
/// conversation. A valid source is handled even when empty, refused or failed.
/// Missing targets, non-tool frames and conversation mismatches have no effect.
pub fn copy_source(
    state: &mut TuiState,
    conversation: &Conversation,
    reducer: Option<&ReducerState>,
    copy: &dyn Fn(&str) -> ClipboardResult,
) -> bool {
    let Some(frame) = state.modal_stack.top() else {
        return false;
    };
    let Surface::ToolInspector(ToolFrameKey(id)) = frame.surface else {
        return false;
    };
    if frame.id != id {
        return false;
    }
    let Some(tool) = state.tool_inspectors.get(&id) else {
        return false;
    };
    if tool.target.conversation_id != conversation.id {
        return false;
    }
    // Only small identity is cloned to release the state borrow before notices.
    let target = tool.target.clone();
    let tab = tool.tab;
    let open_turn = reducer.and_then(|reducer| reducer.open_turn.as_ref());
    let view = if let Some(reducer) = reducer {
        project::project(&conversation.turns, open_turn, reducer, &target)
    } else {
        project::without_tail(&conversation.turns, open_turn, &target)
    };
    let Some(view) = view else {
        return false;
    };
    match source::copy_text(&view, tab) {
        Ok(text) => notify_copy(state, &text, copy),
        Err(error) => notify_failure(state, error.message()),
    }
    true
}
