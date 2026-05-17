//! Event-loop handler module — Story 8.0a extraction destination.
//!
//! By-feature submodules + `shared.rs` per ADR-08-01 §D4. See:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-08-01-handler-extraction-pattern.md`
//! - `_bmad-output/implementation-artifacts/8-0a-event-loop-handler-extraction.md`
//!
//! Domain isolation invariant: this module MUST NOT import anything from
//! `crate::infrastructure::*`. Enforced by `tests/conformance.rs` and by
//! `tools/ci/check-handler-extraction-scope.sh`.
//!
//! Spawn-stays invariant (ADR-08-01 §D2 / §D8.2): this module MUST NOT
//! reference `tokio::spawn`, `task_tracker.spawn`, `TaskTracker`, or
//! `CancellationToken`. Spawns are constructed at the dispatch site in
//! `event_loop.rs::run()` from data payloads returned via `HandlerOutcome`.

use std::sync::Arc;

use crate::domain::events::{AppEvent, CompactionPurpose};
use crate::domain::ports::StreamingProvider;

// By-feature submodules (Task 2 scaffolding — empty stubs until Phase 2/4).
pub mod bookmark;
pub mod budget;
pub mod compaction;
pub mod context_warning;
pub mod model_switch;
pub mod notice;
pub mod render_error;
pub mod scroll;
pub mod search;
pub mod shared;
pub mod usage_panel;

/// Handler-to-dispatch contract per ADR-08-01 §D1.
///
/// Helpers extracted from `event_loop.rs::run()` return a `HandlerOutcome`
/// describing what side-effect (if any) the dispatch arm should perform.
///
/// LOAD-BEARING INVARIANT: payloads are DATA — never `BoxFuture`, never
/// `impl Future`, never any future-typed value. Futures are constructed
/// at the dispatch site so the cancellation-token-tree topology
/// (ADR-06-03) stays observably anchored in `event_loop.rs::run()`. A
/// handler that holds or returns a future has taken the spawn out of the
/// dispatch arm, which silently dissolves the "spawn stays in
/// event_loop.rs" guarantee.
///
/// Adding a 5th variant requires preserving this invariant — or amending
/// ADR-08-01 with an explicit justification for why it no longer holds.
#[allow(dead_code)] // populated by Phase 2+ extractions; full use in Phase 4
pub enum HandlerOutcome {
    /// Pure state mutation; no side-effect to perform.
    Quiet,
    /// Emit an `AppEvent` via `EventBus::emit_domain` (preserves dual-channel contract per ADR-06-06).
    Notify(AppEvent),
    /// Caller spawns a follow-up task. The payload is DATA — never a `BoxFuture`.
    /// See `SpawnRequest` for the per-shape data payloads.
    RequestSpawn(SpawnRequest),
    /// Specialized spawn for the compaction path, whose lifecycle and join-handle
    /// storage diverge enough from generic spawns to warrant a distinct variant.
    RequestCompaction(CompactionPayload),
}

/// Spawn-shape variants per the 3 spawn-bearing handlers identified in
/// Story 8.0a Task 0 bucketing (DGI-C per handler-bucket.md). Resolved as
/// an in-scope refinement of ADR-08-01 §D1 — Winston sign-off 2026-05-16.
///
/// Adding a new variant: prototype the source handler first (per ADR-08-01
/// §D7 prototype-first rule), then add the variant + its dispatch-arm
/// pattern documentation comment.
#[allow(dead_code)]
pub enum SpawnRequest {
    /// Health-check pattern from `apply_model_switch` (formerly `event_loop.rs:9842`).
    ///
    /// **Dispatch-arm pattern:**
    /// ```text
    /// tokio::spawn(async move { provider.health_check().await })
    /// ```
    HealthCheck {
        provider_id: String,
        model_id: String,
        provider: Arc<dyn StreamingProvider>,
    },
    /// Scheduled-event pattern from `apply_open_cross_search_result`
    /// (formerly `event_loop.rs:7897` — peek-highlight expiry).
    ///
    /// `deadline` is an absolute tokio::time::Instant (not a Duration) so
    /// the caller computes `now + delay` at the handler site, preserving
    /// the existing semantics from the peek-expiry code path.
    ///
    /// **Dispatch-arm pattern:**
    /// ```text
    /// tokio::spawn(async move {
    ///     tokio::time::sleep_until(deadline).await;
    ///     let _ = tx.send(event); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS — preserved from pre-extraction
    /// })
    /// ```
    ScheduledEvent {
        deadline: tokio::time::Instant,
        event: AppEvent,
    },
}

/// Payload for the compaction spawn — extracted verbatim from `spawn_compaction`
/// (formerly `event_loop.rs:9567`) parameter list. Per ADR-08-01 §D1, payloads
/// carry data only — the `tokio::spawn(...)` future is constructed at the
/// dispatch site so the `event_loop.rs` spawn-topology stays grep-able.
///
/// **Dispatch-arm pattern (Task 3 prototype target):**
/// ```text
/// tokio::spawn(async move {
///     // body that was in spawn_compaction (lines 9577-9612), preserved verbatim
/// })
/// ```
#[allow(dead_code)]
pub struct CompactionPayload {
    pub provider: Arc<dyn StreamingProvider>,
    pub model: String,
    pub history_text: String,
    pub conversation_id: String,
    pub first_kept_message_id: Option<String>,
    pub pre_tokens: u32,
    pub purpose: CompactionPurpose,
    pub domain_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
}
