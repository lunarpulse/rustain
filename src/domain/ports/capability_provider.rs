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

#[cfg(test)]
mod a2a_conformance {
    #[test]
    fn a2a_domain_model_remains_transport_and_wire_free() {
        let domain = include_str!("../models/a2a_peer_spec.rs");
        for forbidden in [
            "reqwest",
            "serde_jcs",
            "ed25519_dalek",
            "base64::",
            "AgentCardView",
            "A2aClientAdapter",
        ] {
            assert!(
                !domain.contains(forbidden),
                "A2A domain model contains forbidden dependency {forbidden:?}"
            );
        }
    }

    #[test]
    fn a2a_cpa_slot_is_filled_and_composed() {
        let provider = include_str!("../../adapters/a2a/provider.rs");
        assert!(provider.contains("impl CapabilityProvider for A2aProvider"));
        assert!(provider.contains("fn protocol(&self) -> &str"));
        assert!(provider.contains("\"a2a\""));
        assert!(provider.contains("17.4b"));

        let composite = include_str!("../../adapters/composite_toolset_adapter.rs");
        assert!(composite.contains("a2a_provider: std::sync::OnceLock"));
        assert!(composite.contains("pub fn set_a2a_provider"));
        assert!(composite.contains("discover_and_register_all(a2a_provider.as_ref(), \"a2a\")"));
    }

    #[test]
    fn a2a_wire_types_absent_from_entire_src_domain() {
        // AC4 [K], CI-enforced. The full-domain grep guard in tests/conformance.rs
        // does NOT run under `cargo test --lib` (.github/workflows/ci.yml runs only
        // the lib target), so replicate the whole-`src/domain` scan here. Scans
        // `use`/`extern` lines only — comments and doc mentions (e.g. the `reqwest`
        // reference in errors.rs) are fine. ed25519_dalek and base64 are deliberately
        // NOT forbidden: pure crypto is allowed in domain (capability_token.rs,
        // authority_ledger.rs). Forbidden set is exactly AC4's: A2A adapter/wire
        // types plus reqwest/serde_jcs.
        const FORBIDDEN: [&str; 5] = [
            "reqwest",
            "serde_jcs",
            "adapters::a2a",
            "AgentCardView",
            "A2aClientAdapter",
        ];
        fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_rs(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let domain = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain");
        let mut files = Vec::new();
        collect_rs(&domain, &mut files);
        assert!(!files.is_empty(), "no .rs files found under src/domain");

        let mut violations = Vec::new();
        for file in &files {
            let content = std::fs::read_to_string(file).unwrap_or_default();
            for (idx, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if !(trimmed.starts_with("use ") || trimmed.starts_with("extern crate")) {
                    continue;
                }
                for token in FORBIDDEN {
                    if trimmed.contains(token) {
                        violations.push(format!(
                            "{}:{} — forbidden A2A/transport import `{token}`: {trimmed}",
                            file.display(),
                            idx + 1
                        ));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "A2A wire types must not appear in src/domain:\n{}",
            violations.join("\n")
        );
    }
}
