//! trybuild runner for the orchestration type-wall proofs (Story 14.3).
//!
//! A lone `compile_fail` is decorative — the `pass`-twin proves the ONLY
//! failure cause is the inlining attempt (Murat mandate). The runtime size-
//! differential (1MB vs 10MB body → same prompt-byte bound) lives alongside
//! the type-wall proof as a unit test in
//! `src/infrastructure/orchestrator/mod.rs` (`ac5_runtime_size_differential_…`):
//! it has access to the crate-private `ResultStore` + `Window::new` ctor, so
//! it can build the symbolic window + side-table pair and assert the prompt-
//! byte bound is body-invariant (a mutant inlining the body would differ by
//! ~9MB). The compile-time and runtime halves together close the AC5 keystone.
//!
//! AC6 adds the analogous pair for `DrillBody`: the DTO is opaque outside the
//! crate (`pub(crate)` field, sole `as_render_str()` accessor), so the
//! pass-twin names the type + calls the accessor and the compile_fail-twin
//! proves direct `.0` access is E0616 (private field).
#[test]
fn ac5_window_handle_type_wall() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/orchestration/inline_body_pass.rs");
    t.compile_fail("tests/trybuild/orchestration/inline_body_fails.rs");
}

/// AC6 type-wall proof: the drill body is an opaque DTO. The `pass`-twin
/// (`drill_body_tui_pass.rs`) names `DrillBody`/`DrillId` and calls the sole
/// public accessor `as_render_str()`; the `compile_fail`-twin
/// (`drill_body_domain_fails.rs`) attempts to read the raw `.0` field directly,
/// which is E0616 because the field is `pub(crate)`. The pair proves the ONLY
/// failure cause is the private-field access — exactly the visibility boundary
/// that keeps the full payload out of the prompt.
#[test]
fn ac6_drill_body_visibility_wall() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/orchestration/drill_body_tui_pass.rs");
    t.compile_fail("tests/trybuild/orchestration/drill_body_domain_fails.rs");
}
