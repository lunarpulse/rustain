//! `RecallProviderPort` — the OPTIONAL external user-modeling seam (ADR-11-1,
//! declared by Story 11.5 but never sprinted; first lit up here in Story 12.1c
//! AC4 as the session-boundary `on_session_end` hook).
//!
//! ## Seam-first, minimal surface (ADR-11-3 rule 3)
//! This is a **sibling** port to [`MemoryPort`], NOT a widening of it
//! (architecture.md:1192 — "a new capability is a sibling seam, never a widening
//! of an existing port"). It models *external* recall/user-modeling (e.g. Honcho)
//! that a session can OPTIONALLY drive; the default is offline
//! ([`NoopRecallProvider`](crate::adapters::noop::NoopRecallProvider)).
//!
//! Story 12.1c declares **only** `on_session_end` — the one method the daemon's
//! `SessionBoundary` seam needs (architecture.md:1170-1173 also reserves
//! `prefetch`/`sync_turn` + a transition trio; those stay deferred to 11.5/12.2,
//! uncommented when their consumers land). Per the Epic 11 retro AI-11.1
//! discipline, we declare the trait HERE rather than leaving a dormant
//! commented-out body — and per the 12.1b "prove the seam is *called*" lesson,
//! the daemon invokes it unconditionally (the `Noop` makes the no-op explicit).

use crate::domain::models::ChatMessage;

/// Optional external recall / user-modeling provider, driven at session
/// boundaries. One per session; default offline (`NoopRecallProvider`).
#[async_trait::async_trait]
pub trait RecallProviderPort: Send + Sync {
    /// Called once when a session boundary fires (daemon `daily_reset` /
    /// `idle_timeout` / graceful `Shutdown`, Story 12.1c AC4). `transcript` is the
    /// conversation so far — **empty in the headless daemon** until Story 12.2
    /// attaches a message runtime (the daemon composes the memory port only). An
    /// implementation MUST NOT assume a non-empty transcript, and MUST NOT
    /// short-circuit on empty (the emptiness is the daemon's missing source, not a
    /// signal to skip — guarding against a baked-in `if empty { return }` that
    /// would later mask a real bug).
    async fn on_session_end(
        &self,
        transcript: &[ChatMessage],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
