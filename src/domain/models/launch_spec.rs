//! Agent launch specification — per-call resolution unit.
//!
//! Mirrors the KIMI-cli `AgentLaunchSpec.effective_model` pattern:
//! each sub-agent dispatch carries its own resolved model, tool allow-list,
//! and parent context size (Story 10.7).
//!
//! For the foreground turn (Story 7.1c) only `effective_model` is consumed;
//! the full struct is constructed as forward-compat documentation.

use serde::{Deserialize, Serialize};

/// Per-call specification for launching an agent or sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLaunchSpec {
    /// System prompt / instruction text for this call.
    pub prompt: String,
    /// The resolved model name after tier routing.
    pub effective_model: String,
    /// Tool names this agent is allowed to invoke.
    pub tools_allow: Vec<String>,
    /// Tokens consumed by the parent conversation context.
    pub parent_ctx_tokens: u32,
}
