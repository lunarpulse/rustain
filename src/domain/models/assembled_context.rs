//! `AssembledContext` + `AssemblyBudget` — the **Message-tier** boundary objects
//! returned/consumed by [`ContextAssemblerPort`](crate::domain::ports::ContextAssemblerPort)
//! (Story 11.0a, ADR-10-4).
//!
//! These are the Message-tier counterparts to the Content-tier `ContextBundle`
//! family in [`context_bundle`](crate::domain::models::context_bundle). The two
//! tiers are deliberately distinct (architecture.md § "Context Assembly: Two
//! Ports", 1125–1182):
//!
//! - **Content tier** (`ContextPort` → `ContextBundle`): selects/ranks *what*
//!   memory content enters the turn. Does I/O (recall). Owns `ContextBudget`.
//! - **Message tier** (`ContextAssemblerPort` → `AssembledContext`): builds the
//!   provider wire payload (*how*) from an already-decorated `Conversation`.
//!   Never selects content, never does I/O. Owns `AssemblyBudget`.
//!
//! Pure domain value objects: **serde-free** (mirrors `context_bundle.rs` — the
//! domain layer is depended on by everyone; persist via a private DTO only if
//! these ever hit disk/wire, which they do not).

use crate::domain::models::{AssembleDiagnostics, Message};

/// The whole-window token ceiling a [`ContextAssemblerPort`] assembly competes
/// for. A Message-tier newtype — **deliberately NOT** the Content-tier
/// [`ContextBudget`](crate::domain::models::ContextBudget), which the 11.4
/// author fenced off as "deliberately NOT the Message-tier budget"
/// (`context_bundle.rs:36`): the two measure different things (a memory-
/// injection slice vs. the whole-window ceiling) and conflating them is a
/// category error.
///
/// `StaticPassthroughAssembler` (Story 11.0a) **ignores** `budget`; Story 11.6's
/// `WindowingAssembler` trims the assembled window down to `max_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssemblyBudget {
    /// Max estimated tokens the assembled wire payload may occupy.
    pub max_tokens: usize,
}

impl AssemblyBudget {
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }
}

/// The boundary object [`ContextAssemblerPort::assemble`] returns: the provider
/// wire messages for a turn, plus structured assembly diagnostics.
///
/// Carries the **existing** [`AssembleDiagnostics`] (reused from
/// `context_bundle.rs`, not a fresh type) so Story 11.6 inherits the diagnostics
/// channel its `AC-11.6.6` requires with **zero trait churn**.
/// `StaticPassthroughAssembler` returns `AssembleDiagnostics::default()` and its
/// `messages` are byte-identical to the pre-port `build_api_messages` output;
/// Story 11.6 fills `group_count` / `tokens_saved_*` etc.
///
/// No `PartialEq` derive: [`Message`] does not implement `PartialEq` (it is the
/// provider wire type, compared by serialized payload, not structurally). The
/// 11.0a characterization test asserts byte-identity via `serde_json`.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    /// The provider wire messages for this turn.
    pub messages: Vec<Message>,
    /// Structured assembly diagnostics (passthrough → `default()`).
    pub diagnostics: AssembleDiagnostics,
}
