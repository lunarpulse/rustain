#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Context assembly and injection for agent conversations.
///
/// Claudian equivalent: N/A (v1.0+ port, no Claudian counterpart)
// 2026-05-19 — Story 8.5 added health_snapshot() with default HealthSummary::unknown() impl
// following additive-with-defaults discipline. No existing adapters needed changes.
// Real metrics ship with real adapters in Epic 12.
// 2026-06-02 — Story 11.4 lit up the Content-tier assemble() surface as a
// defaulted method (additive-with-defaults). NoOpContext inherits the default
// empty bundle (the regression canary); MemoryContextAdapter is the first real
// override. assemble() returns a ContextBundle, NEVER Vec<Message> — selecting
// *what* content is the Content tier's job; building the wire payload stays with
// the existing inline assembly (Two-Ports boundary, architecture.md:1157).
#[async_trait::async_trait]
pub trait ContextPort: Send + Sync {
    // v1.0: context assembly methods

    /// Assemble the memory/context to inject for a turn, given the just-submitted
    /// user message (`query`) and the remaining token `budget`. Default returns an
    /// empty bundle so `NoOpContext` and any future adapter keep compiling
    /// untouched (additive-with-defaults, SDK-stability mandate above).
    async fn assemble(
        &self,
        _query: &str,
        _budget: crate::domain::models::ContextBudget,
    ) -> Result<crate::domain::models::ContextBundle, crate::domain::errors::ContextError> {
        Ok(crate::domain::models::ContextBundle::empty())
    }

    fn health_snapshot(&self) -> crate::domain::models::HealthSummary {
        crate::domain::models::HealthSummary::unknown()
    }
}
