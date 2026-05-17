#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Conversation memory storage and retrieval.
///
/// Claudian equivalent: `src/core/memory/memoryManager.ts`
pub trait MemoryPort: Send + Sync {
    // v1.0: async fn store(&self, entry: MemoryEntry) -> Result<(), MemoryError>;
    // v1.0: async fn recent(&self, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError>;
    // v1.0: async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, MemoryError>;
    // Transition methods with defaults:
    // async fn prepare_detach(&self) -> Result<TransitionState, TransitionError> { Ok(TransitionState::empty("memory")) }
    // async fn receive_state(&self, state: TransitionState) -> Result<(), TransitionError> { Ok(()) }
    // async fn post_transition_verify(&self) -> Result<(), TransitionError> { Ok(()) }
}
