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
    /// Run this child in an isolated scratch workspace.
    ///
    /// Internal launch type: `false` omits the field to preserve byte-identical
    /// legacy serialization; consumer-facing output-schema rules do not apply.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub isolated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(isolated: bool) -> AgentLaunchSpec {
        AgentLaunchSpec {
            prompt: "p".into(),
            effective_model: "m".into(),
            tier: ModelTier::CheapAgentic,
            tools_allow: ToolPolicy::InheritFromParent,
            parent_ctx_tokens: 0,
            sandbox_override: None,
            parent_trace: None,
            isolated,
        }
    }

    #[test]
    fn isolated_false_serializes_byte_identical_without_key() {
        let json = serde_json::to_string(&spec(false)).unwrap();
        assert!(!json.contains("isolated"));
    }

    #[test]
    fn isolated_true_serializes_explicit_key_and_old_payload_defaults_false() {
        let json = serde_json::to_string(&spec(true)).unwrap();
        assert!(json.contains("\"isolated\":true"));

        let old_payload = r#"{
            "prompt":"p",
            "effective_model":"m",
            "tier":"cheap_agentic",
            "tools_allow":{"kind":"inherit_from_parent"},
            "parent_ctx_tokens":0,
            "sandbox_override":null,
            "parent_trace":null
        }"#;
        let decoded: AgentLaunchSpec = serde_json::from_str(old_payload).unwrap();
        assert!(!decoded.isolated);
    }
}
