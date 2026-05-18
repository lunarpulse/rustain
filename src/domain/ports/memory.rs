#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Conversation memory storage and retrieval.
///
/// Claudian equivalent: `src/core/memory/memoryManager.ts`
#[async_trait::async_trait]
pub trait MemoryPort: Send + Sync {
    // v1.0 reserved methods (commented out):
    // - store(&self, entry: MemoryEntry) -> Result<(), MemoryError>
    // - recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError>
    // - search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError>
    // 2026-05-17 — Story 8.4 added prepare_detach/receive_state/post_transition_verify (warm tier)
    // following the additive-with-defaults discipline. No existing adapters needed changes.
    async fn prepare_detach(&self) -> Result<crate::domain::models::TransitionState, crate::domain::errors::TransitionError> {
        Ok(crate::domain::models::TransitionState::empty("memory"))
    }
    async fn receive_state(&self, _state: crate::domain::models::TransitionState) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
    async fn post_transition_verify(&self) -> Result<(), crate::domain::errors::TransitionError> {
        Ok(())
    }
}
