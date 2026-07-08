pub mod agent;
pub mod run;
pub mod translate;

pub use run::run_acp;

/// Returns `true` when the session id was minted by the ACP adapter.
pub fn is_acp_session_id(session_id: &str) -> bool {
    session_id.starts_with("acp-")
}

/// Derive the wire `SessionId` string from a durable conversation id (DD-2).
///
/// An ACP session **is** a conversation: the session id is the conversation id
/// prefixed with `acp-`. Bijective by construction — the conversation id is a
/// unique nanoid **and** the on-disk file key, so no persisted index is needed
/// and the mapping survives a process restart. Resolution is the inverse
/// ([`conversation_id_from_acp_session_id`]).
pub fn format_acp_session_id(conversation_id: &str) -> String {
    format!("acp-{conversation_id}")
}

/// Inverse of [`format_acp_session_id`]: strip the `acp-` prefix to recover the
/// durable conversation id. Returns `None` for an id the ACP adapter did not
/// mint (orphan / cross-transport id — AC8 fail-closed).
pub fn conversation_id_from_acp_session_id(session_id: &str) -> Option<&str> {
    session_id.strip_prefix("acp-")
}
