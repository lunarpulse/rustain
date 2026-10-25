pub mod agent_core;
pub mod app_context;
pub mod app_state;
pub mod approval_bridge;
pub mod artifact_bridge;
#[cfg(unix)]
pub mod attach_loop;
pub mod event_bus;
pub mod event_loop;
pub(crate) mod panel_bridge;
pub mod peer_bridge;
pub(crate) mod recipient_surface;
pub mod recipient_view;
pub mod room_bridge;
#[cfg(test)]
mod state_extraction_tests;
pub(crate) mod task_panel_bridge;
#[cfg(feature = "a2a")]
pub mod team_proposal_bridge;
#[cfg(unix)]
pub mod team_send_cli;
pub mod transparency_bridge;
pub mod turn;
pub mod turn_driver;
