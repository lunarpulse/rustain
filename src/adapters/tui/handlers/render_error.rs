//! Render-error handler — cross-epic utility.
//!
//! `handle_render_error` returns `HandlerOutcome::Quiet` — aborts active turn
//! `JoinHandle`, resets streaming state, recovers terminal (may set
//! `state.should_quit` on recovery failure). No event emission, no internal spawn.
//!
//! 6 call sites in `event_loop.rs` dispatch arms (after each `render()` call).

#![allow(dead_code)]

use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::terminal::Tui;
use crate::domain::models::{StatusState, StreamingState};

use super::HandlerOutcome;

/// Handle a render failure: abort active turn, reset streaming, attempt terminal recovery.
pub fn handle_render_error(
    err: anyhow::Error,
    active_turn: &mut Option<tokio::task::JoinHandle<()>>,
    streaming: &mut StreamingState,
    state: &mut TuiState,
    terminal: &mut Tui,
) -> HandlerOutcome {
    tracing::error!("Render failed: {}", err);

    // Abort active turn if running
    if let Some(handle) = active_turn.take() {
        handle.abort();
    }

    // Reset streaming state
    streaming.is_streaming = false;
    streaming.phase = crate::domain::models::StreamingPhase::Idle;
    streaming.current_text_buffer.clear();
    streaming.current_blocks.clear();
    streaming.active_tool_calls.clear();

    state.status_before_flash = Some(state.status.clone());
    state.status = StatusState::Flash {
        message: format!("Render failed: {}", err),
        remaining_ms: state.theme.timing.status_flash_ms,
    };
    state.needs_redraw = true;

    // Attempt terminal recovery
    crate::adapters::tui::terminal::restore_terminal_raw();
    match crate::adapters::tui::terminal::setup(crate::adapters::tui::terminal::is_mouse_enabled())
    {
        Ok(new_terminal) => {
            *terminal = new_terminal;
            tracing::info!("Terminal recovered after render failure");
        }
        Err(recovery_err) => {
            tracing::error!("Terminal recovery failed: {}", recovery_err);
            state.should_quit = true;
        }
    }
    HandlerOutcome::Quiet
}
