//! Agent launch specification — per-call resolution unit.
//!
//! Mirrors the KIMI-cli `AgentLaunchSpec.effective_model` pattern:
//! each sub-agent dispatch carries its own resolved model, tool allow-list,
//! and parent context size (Story 10.7).
//!
//! For the foreground turn (Story 7.1c) only `effective_model` is consumed;
//! the full struct is constructed as forward-compat documentation.

use crate::domain::models::{ModelTier, ToolPolicy, TraceContext};
use serde::{Deserialize, Serialize};

/// Per-call specification for launching an agent or sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLaunchSpec {
    /// System prompt / instruction text for this call.
    pub prompt: String,
    /// The resolved model name after tier routing.
    pub effective_model: String,
    /// Cost/quality tier for model selection.
    pub tier: ModelTier,
    /// Tool policy for this agent (narrow-only inheritance).
    pub tools_allow: ToolPolicy,
    /// Tokens consumed by the parent conversation context.
    pub parent_ctx_tokens: u32,
    /// Optional sandbox override (narrowing-only per ADR-10-3).
    pub sandbox_override: Option<crate::domain::models::SandboxPolicy>,
    /// Optional W3C Trace Context for distributed tracing.
    pub parent_trace: Option<TraceContext>,
}
