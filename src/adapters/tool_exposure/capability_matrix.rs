//! `CapabilityMatrix` — the Strategy×Provider compatibility table.
//!
//! # Phase A (Story 9.4 — THIS STORY)
//!
//! Phase A stub: `query(&self, _strategy, _provider)` returns `Capability::Full`
//! UNCONDITIONALLY for every provider per ADR-09-01 v2.2 §Per-provider wiring
//! Phase A note. The 3 enum variants exist (`Full`, `Degraded`, `Incompatible`)
//! but only `Full` is constructible in Phase A — the others are reserved for
//! Phase B per ADR §Decision (forward-compat type — Phase B amends behavior,
//! NOT signatures).
//!
//! # Phase B (Story 9.7 — DEFERRED)
//!
//! Phase B amends the matrix per ADR §Per-provider wiring table:
//! - Anthropic × MetaSearch: `Full` when beta header active, `Degraded` (fallback
//!   to client-side BM25) otherwise.
//! - OpenAI × MetaSearch: `Degraded` (client-side BM25 + locally-emulated
//!   `tool_search` until OpenAI ships GA primitive).
//! - Ollama × MetaSearch: `Full` when model supports tool calling (`llama3.1+`),
//!   `Incompatible` otherwise.

use crate::domain::models::provider_capabilities::ProviderCapabilities;

use super::ExposureKind;

/// Per-provider strategy compatibility level.
///
/// # Phase A
///
/// Only `Full` is constructible (the stub returns it for every Strategy ×
/// Provider combination). `Degraded` and `Incompatible` are reserved for
/// Phase B differentiation per ADR-09-01 v2.2 §Per-provider wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Capability {
    /// Strategy fully supported on this provider.
    Full,
    /// Strategy partially supported — RESERVED for Phase B (e.g., Anthropic
    /// MetaSearch without beta header falling back to client-side BM25).
    Degraded,
    /// Strategy incompatible with provider — fails at session handshake per
    /// ADR §Capability matrix (never mid-turn). RESERVED for Phase B (e.g.,
    /// Ollama MetaSearch on models without tool calling).
    Incompatible,
}

/// Strategy × Provider compatibility table.
///
/// Phase A's `query` returns `Capability::Full` for every combination — the
/// `StaticFullExposure` passthrough is universally compatible. Phase B
/// (Story 9.7) replaces this with per-provider lookups.
#[derive(Debug, Default)]
pub struct CapabilityMatrix;

impl CapabilityMatrix {
    pub fn new() -> Self {
        Self
    }

    /// Look up the compatibility level for a (strategy, provider) pair.
    ///
    /// Phase A: returns `Capability::Full` UNCONDITIONALLY per
    /// ADR-09-01 v2.2 §Per-provider wiring Phase A note. Per-provider
    /// differentiation lands in Story 9.7 Phase B.
    pub fn query(&self, _strategy: ExposureKind, _provider: &ProviderCapabilities) -> Capability {
        Capability::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::provider_capabilities::TransportKind;

    fn test_caps(transport: TransportKind) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_list_changed: true,
            supports_native_retrieval: None,
            max_tool_count: None,
            transport_kind: transport,
        }
    }

    #[test]
    fn test_phase_a_returns_full_for_all_providers() {
        let matrix = CapabilityMatrix::new();
        for transport in [
            TransportKind::Stdio,
            TransportKind::Http,
            TransportKind::Sse,
            TransportKind::InProcess,
        ] {
            assert_eq!(
                matrix.query(ExposureKind::StaticFull, &test_caps(transport)),
                Capability::Full,
                "Phase A stub must return Full for every provider (transport={:?})",
                transport
            );
        }
    }

    #[test]
    fn test_phase_a_returns_full_for_reserved_meta_search_variant() {
        // Phase A: even MetaSearch returns Full from the stub. This is OK
        // because no Phase A impl constructs MetaSearch (the config parser
        // rejects "meta-search" at startup per AC-9-4-5); the stub's
        // unconditional Full is the trivial-correctness baseline that Phase B
        // refines.
        let matrix = CapabilityMatrix::new();
        assert_eq!(
            matrix.query(ExposureKind::MetaSearch, &test_caps(TransportKind::Stdio)),
            Capability::Full
        );
    }
}
