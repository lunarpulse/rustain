//! Handler-naming reflection test per ADR-08-01 §D6.4 + Story 8.0a AC-3.
//!
//! **Phase 5 disposition:** the architectural ratchets (handler count, spawn-stays
//! invariant, dispatch arms in event_loop.rs) consolidated into
//! `tests/conformance.rs::test_handler_naming_reflection`. The conformance test is
//! the canonical location for these checks — it groups with sibling architectural
//! ratchets (hex isolation, BYPASS, std_sync_lock).
//!
//! What's NOT yet implemented (deferred to follow-up):
//!
//! - **Strict AppEvent variant → handler naming binding** per ADR-08-01 §D6.4:
//!   "`fn handle_<snake_case(VariantName)>` must exist for every variant that
//!    has a handler." Most of Story 8.0a's 18 extracted handlers are
//!    InputAction-driven, not AppEvent-driven (e.g., `handle_apply_scroll_intent`,
//!    `handle_apply_bookmark_toggle`). A strict 1:1 AppEvent mapping doesn't
//!    fit the current handler set. The rule applies once handlers that handle
//!    *AppEvent* variants directly are extracted (Phase 4-late / Epic 8 work).
//!
//! - **`syn::parse_file` introspection of `AppEvent` enum** — would emit the
//!   full list of variants and cross-check against handlers/ fn names. Requires
//!   adding `syn` as a dev-dependency, which Story 8.0a didn't budget for.
//!
//! See `tests/conformance.rs::test_handler_naming_reflection` for the
//! invariants that ARE enforced today.

#[test]
#[ignore = "Phase 5 — ratchet logic consolidated in tests/conformance.rs::test_handler_naming_reflection. \
            See module-level doc comment for deferred strict-naming rule."]
fn handler_naming_reflection_test_stub() {
    // Intentionally empty. The reflection invariants for Story 8.0a are
    // enforced by `tests/conformance.rs::test_handler_naming_reflection`.
    //
    // To run that test:
    //   cargo test --test conformance test_handler_naming_reflection
    //
    // To implement the full AppEvent-variant naming check later:
    //   1. Add `syn = { version = "2.0", features = ["full"] }` as a dev-dep.
    //   2. Parse `src/domain/events.rs` with `syn::parse_file`.
    //   3. Walk the `AppEvent` enum's variants.
    //   4. For each variant marked as "handled" (registry to be defined), assert
    //      `fn handle_<snake_case(variant)>` exists under `src/adapters/tui/handlers/`.
    //   5. Also assert the module file naming matches the variant family prefix
    //      (e.g., `compaction.rs` for `CompactionComplete` + `CompactionStart`).
}
