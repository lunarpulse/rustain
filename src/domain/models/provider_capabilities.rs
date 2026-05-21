/// Static feature support declared by a capability provider.
///
/// Story 9.4 Phase A's capability matrix stub pattern-matches on this at session
/// handshake to return `Capability::Full` unconditionally per ADR-09-01 v2.2
/// §Phased Implementation. Per-provider differentiation lands in 9.4b Phase B.
///
/// See ADR-09-01 v2.2 §Phased Implementation for the full decomposition.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCapabilities {
    /// Whether the provider supports streaming responses.
    pub supports_streaming: bool,
    /// Whether the provider emits `notifications/tools/list_changed`.
    pub supports_list_changed: bool,
    /// Native LLM-level retrieval primitive, if any.
    /// Per-LLM-provider (Anthropic / OpenAI / Ollama), NOT per-MCP-server.
    /// `None` for MCP transport providers.
    pub supports_native_retrieval: Option<NativeRetrievalKind>,
    /// Maximum number of tools the provider can expose, if bounded.
    pub max_tool_count: Option<usize>,
    /// How the provider is connected.
    pub transport_kind: TransportKind,
}

/// Native LLM-level retrieval primitive kind.
///
/// `#[non_exhaustive]` allows adding new variants without breaking downstream
/// match sites (they must handle the wildcard `_ => ...` case).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeRetrievalKind {
    /// Anthropic native primitive (beta header `anthropic-beta: tool-search-2025-11-19`)
    AnthropicBm25_20251119,
    /// OpenAI native primitive (deferred — not GA as of 2026-05-20)
    OpenAiToolSearchVNext,
}

/// Transport kind for the capability provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Child process over stdio (stdin/stdout JSON-RPC).
    Stdio,
    /// HTTP transport.
    Http,
    /// Server-Sent Events transport.
    Sse,
    /// No transport layer — in-process provider (built-in tools, agent skills).
    InProcess,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clone_and_eq() {
        let caps = ProviderCapabilities {
            supports_streaming: false,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: TransportKind::Stdio,
        };
        let cloned = caps.clone();
        assert_eq!(caps, cloned);
        assert!(
            !format!("{:?}", caps).is_empty(),
            "Debug output should be non-empty"
        );
    }

    /// Compile-only documentation that `#[non_exhaustive]` is active:
    /// any `match` on `NativeRetrievalKind` without a wildcard arm fails to compile.
    #[test]
    fn test_native_retrieval_non_exhaustive() {
        let kind: Option<NativeRetrievalKind> = None;
        // This match includes a wildcard arm, which compiles.
        // Removing the wildcard would cause a compile error due to `#[non_exhaustive]`.
        #[allow(unreachable_patterns)]
        let _ = match kind {
            Some(NativeRetrievalKind::AnthropicBm25_20251119) => "anthropic",
            Some(NativeRetrievalKind::OpenAiToolSearchVNext) => "openai",
            Some(_) => "future", // required by #[non_exhaustive]
            None => "none",
        };
    }
}
