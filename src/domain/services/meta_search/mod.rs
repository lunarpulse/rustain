//! Pure-domain logic for the shared meta-search infrastructure (Story 9.7
//! Phase B). All functions here are pure (no I/O, no async, no global
//! state) so they live in `domain/services/` not `infrastructure/search/`.
//!
//! ADR-09-02 v2 §Pinned file layout names this path as
//! `src/services/meta_search/` — `domain/services/meta_search/` is the
//! Rustain-canonical path per hexagonal §Dependency rule (pure logic with no
//! adapter imports). Document this divergence in the ADR-09-02 v1.2
//! amendment (Task 14 — non-blocking).

pub mod terse;

pub use terse::compute_terse;
