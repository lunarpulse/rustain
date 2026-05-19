//! `/config` slash command handler — extracted from event_loop.rs (Story 8.5 Option A).

use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::infrastructure::runtime::event_bus::EventBus;

pub fn handle_config_slash(
    state: &mut TuiState,
    cmd_arg: Option<&str>,
    event_bus: &EventBus,
) {
    if cmd_arg.is_some_and(|a| a.eq_ignore_ascii_case("reload")) {
        event_bus.emit_domain(AppEvent::ConfigReload);
    } else {
        if !matches!(state.status, crate::domain::models::StatusState::Flash { .. }) {
            state.status_before_flash = Some(state.status.clone());
        }
        state.status = crate::domain::models::StatusState::Flash {
            message: "/config reload — reload configuration from disk".to_string(),
            remaining_ms: state.theme.timing.status_flash_ms,
        };
        state.needs_redraw = true;
    }
}
