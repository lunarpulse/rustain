//! `/config` slash command handler — extracted from event_loop.rs (Story 8.5 Option A).

use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::infrastructure::runtime::event_bus::EventBus;

pub fn handle_config_slash(state: &mut TuiState, cmd_arg: Option<&str>, event_bus: &EventBus) {
    if cmd_arg.is_some_and(|a| a.eq_ignore_ascii_case("reload")) {
        let _ = event_bus.emit_domain(AppEvent::ConfigReload);
    } else {
        if !matches!(
            state.status,
            crate::domain::models::StatusState::Flash { .. }
        ) {
            state.status_before_flash = Some(state.status.clone());
        }
        state.status = crate::domain::models::StatusState::Flash {
            message: "/config reload — reload configuration from disk".to_string(),
            remaining_ms: state.theme.timing.status_flash_ms,
        };
        state.needs_redraw = true;
    }
}

pub(crate) fn apply_mode_command(
    state: &mut TuiState,
    argument: Option<&str>,
    mode: crate::domain::models::PermissionMode,
    event_bus: &EventBus,
) {
    use crate::domain::models::PermissionMode;
    match argument.map(str::trim) {
        None => {
            let current = match mode {
                PermissionMode::Plan => "Plan",
                PermissionMode::Normal => "Normal",
                PermissionMode::AutoEdit => "AutoEdit",
                PermissionMode::Yolo => "YOLO",
            };
            command_flash(
                state,
                format!(
                    "Current mode: {} — use /mode <plan|normal|autoedit|yolo> to switch",
                    current
                ),
            );
        }
        Some(arg) => {
            if let Some(target) =
                crate::domain::services::permission_chain::parse_mode_arg(Some(arg))
            {
                let _ = event_bus.emit_domain(AppEvent::SetPermissionMode(target));
            } else {
                command_flash(
                    state,
                    argument_error(
                        "Unknown mode: ",
                        arg,
                        ". Use plan, normal, autoedit, or yolo",
                    ),
                );
            }
        }
    }
}

pub(crate) fn apply_plan_command(
    state: &mut TuiState,
    argument: Option<&str>,
    mode: crate::domain::models::PermissionMode,
    event_bus: &EventBus,
) {
    use crate::domain::models::PermissionMode;
    match argument.map(str::trim) {
        None => {
            let current = if mode == PermissionMode::Plan {
                "Plan"
            } else {
                "not Plan"
            };
            command_flash(
                state,
                format!(
                    "Plan mode is {} — use /plan on|off|toggle to switch",
                    current
                ),
            );
        }
        Some(arg) => {
            let target = if arg.eq_ignore_ascii_case("on") {
                Some(PermissionMode::Plan)
            } else if arg.eq_ignore_ascii_case("off") {
                Some(PermissionMode::Normal)
            } else if arg.eq_ignore_ascii_case("toggle") {
                Some(if mode == PermissionMode::Plan {
                    PermissionMode::Normal
                } else {
                    PermissionMode::Plan
                })
            } else {
                None
            };
            if let Some(target) = target {
                let _ = event_bus.emit_domain(AppEvent::SetPermissionMode(target));
            } else {
                command_flash(
                    state,
                    argument_error("Unknown /plan argument: ", arg, ". Use on, off, or toggle"),
                );
            }
        }
    }
}

fn argument_error(prefix: &str, argument: &str, suffix: &str) -> String {
    let mut message = String::with_capacity(prefix.len() + argument.len() + suffix.len());
    message.push_str(prefix);
    message.extend(
        argument
            .chars()
            .map(|character| character.to_ascii_lowercase()),
    );
    message.push_str(suffix);
    message
}

fn command_flash(state: &mut TuiState, message: String) {
    use crate::domain::models::StatusState;
    if !matches!(state.status, StatusState::Flash { .. }) {
        state.status_before_flash = Some(state.status.clone());
    }
    state.status = StatusState::Flash {
        message,
        remaining_ms: state.theme.timing.status_flash_ms,
    };
    state.needs_redraw = true;
}
