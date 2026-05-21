use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::models::ToolResult;
use crate::domain::models::capability::Capability;
use crate::domain::models::capability::CapabilityError;
use crate::domain::models::capability_id::CapabilityId;
use crate::domain::models::provider_capabilities::ProviderCapabilities;

/// Contract for a pluggable capability provider.
///
/// # Lifecycle
///
/// `discover → register → invoke → render`
///
/// Implementations are discovered via `discover()`, registered into the
/// `CapabilityRegistry` by `CompositeToolsetAdapter`, and invoked via
/// `invoke()` when the LLM requests a tool call.
///
/// # Provider types
///
/// | Protocol | Implementation | Story |
/// |----------|---------------|-------|
/// | `"mcp"` | `McpProvider` (wraps `McpClientAdapter`) | 9.3a |
/// | `"builtin"` | `BuiltinProvider` (refactor of `ToolSetAdapter`) | 9.3b |
/// | `"skill"` | `SkillsProvider` (refactor of skill executor) | 9.3b |
/// | `"a2a"` | `A2aProvider` | Epic 14 |
/// | `"subagent"` | `SubagentProvider` | Epic 10 |
///
/// # Design decisions
///
/// - **4 methods, not 5:** No `activate()` step. MCP + builtin providers have no
///   activation step; SkillsProvider in 9.3b will delegate to `SkillActivator`
///   (existing) rather than expose activation on the trait. (Decision Gate 3.1)
/// - **No `PermissionChain` integration:** The trait focuses on capabilities,
///   not authorization. Permission checks live in `ToolScheduler` and use the
///   existing `permission_chain` module.
///
/// # Related
///
/// - FR48: "Capability Provider Architecture (CPA) trait and MCP registration"
/// - `src/domain/models/capability_registry.rs` — the registry that holds capabilities
/// - `src/adapters/mcp/mcp_provider.rs` — the MCP implementation
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Stable protocol identifier — used by `CapabilityId` namespace (AC-3).
    ///
    /// Returns one of: `"mcp"`, `"builtin"`, `"skill"`, `"a2a"`, `"subagent"`.
    fn protocol(&self) -> &str;

    /// Provider's static feature support (AC-9-3a-6 export).
    ///
    /// Pattern-matched by Story 9.4 Phase A capability matrix at session handshake.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Discover capabilities currently exposed by this provider.
    ///
    /// For `McpProvider`: reads `McpClientAdapter::cached_tools()` and projects
    /// per ADR-06-08. Pure read of in-memory state; NO I/O on the hot path
    /// (the actual MCP `tools/list` network call is owned by
    /// `McpClientAdapter::refresh_cached_tools` per Story 9.2 AC-6).
    async fn discover(&self) -> Result<Vec<Capability>, CapabilityError>;

    /// Invoke a capability with input and cancellation token.
    ///
    /// Returns the same `ToolResult` shape that `ToolSetPort::execute` already
    /// produces (zero conversion overhead — 9.3a uses domain `ToolResult` directly
    /// rather than introducing a parallel result type).
    async fn invoke(
        &self,
        capability_id: &CapabilityId,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, CapabilityError>;
}
