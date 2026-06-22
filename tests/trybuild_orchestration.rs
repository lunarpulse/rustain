//! trybuild runner for the AC5 type-wall proof (Story 14.3).
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
#[test]
fn ac5_window_handle_type_wall() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/orchestration/inline_body_pass.rs");
    t.compile_fail("tests/trybuild/orchestration/inline_body_fails.rs");
}
