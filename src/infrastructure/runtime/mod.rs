pub mod agent_core;
pub mod app_context;
pub mod app_state;
pub mod approval_bridge;
pub mod artifact_bridge;
#[cfg(unix)]
pub(crate) mod attach_interrupt;
#[cfg(unix)]
pub mod attach_loop;
pub mod decision_effects;
pub(crate) mod error_notices;
pub mod event_bus;
pub mod event_loop;
pub mod history_bridge;
pub mod launchpad_facts;
pub mod mission_bridge;
pub(crate) mod notice_actions;
pub mod panel_bridge;
pub mod peer_bridge;
pub(crate) mod quit_guard;
pub mod recipient_view;
pub(crate) mod recovery_prompt;
pub mod room_bridge;
#[cfg(test)]
mod state_extraction_tests;
pub(crate) mod system_notice;
pub mod task_panel_bridge;
#[cfg(feature = "a2a")]
pub mod team_proposal_bridge;
#[cfg(unix)]
pub mod team_send_cli;
pub mod thread_open;
pub(crate) mod ticket_bridge;
pub mod transparency_bridge;
pub mod tui_turn_lifecycle;
pub mod turn;
pub mod turn_driver;
