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
pub trait ContextPort: Send + Sync {
    // v1.0: context assembly methods

    fn health_snapshot(&self) -> crate::domain::models::HealthSummary {
        crate::domain::models::HealthSummary::unknown()
    }
}
