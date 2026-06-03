#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
//!
//! ADR-10-4 (Story 11.0a). The **MESSAGE-tier** context assembler.
//!
//! Two-Ports boundary (architecture.md § "Context Assembly: Two Ports", 1125;
//! reject-guard at 1157): this port builds the provider wire payload (the *how*)
//! from an already-decorated `Conversation`. It **NEVER** selects or ranks
//! memory content — that is [`ContextPort`](crate::domain::ports::ContextPort),
//! the Content tier. It never does I/O (recall enters at the Content tier per
//! architecture.md:1153–1155), which is why [`ContextAssemblerPort::assemble`]
//! is **infallible and sync** (no `Result`, no `async`): both known impls
//! (`StaticPassthroughAssembler` here, `WindowingAssembler` in Story 11.6) are
//! pure, deterministic folds over the conversation.

use crate::domain::models::{AssembledContext, AssemblyBudget, Conversation};

/// Builds the per-turn provider wire messages (`Conversation -> Vec<Message>`).
///
/// This is the exact transform Story 11.6's `WindowingAssembler` replaces; the
/// 11.0a default `StaticPassthroughAssembler` is byte-identical to the legacy
/// inline `build_api_messages` call.
pub trait ContextAssemblerPort: Send + Sync {
    /// Build the provider wire messages for this turn from the conversation.
    ///
    /// `budget` is the whole-window token ceiling, reserved for Story 11.6
    /// (`WindowingAssembler` trims to it); `StaticPassthroughAssembler` **ignores**
    /// it and its `.messages` are byte-identical to the pre-port
    /// `build_api_messages` output. Returns [`AssembledContext`] carrying the wire
    /// messages plus the (reused) `AssembleDiagnostics`; passthrough returns
    /// `AssembleDiagnostics::default()`.
    fn assemble(&self, conversation: &Conversation, budget: AssemblyBudget) -> AssembledContext;
}
