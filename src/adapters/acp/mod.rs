pub mod agent;
pub mod run;
pub mod translate;

pub use run::run_acp;

/// Returns `true` when the session id was minted by the ACP adapter.
pub fn is_acp_session_id(session_id: &str) -> bool {
    session_id.starts_with("acp-")
}
