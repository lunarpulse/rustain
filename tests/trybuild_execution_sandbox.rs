//! trybuild runner for the `ExecutionSandbox` sibling-port proof (Story 17.3a,
//! AC1 / DF-14-5-3).
//!
//! A lone `compile_fail` is decorative — the `pass`-twin proves the ONLY
//! failure cause is the sibling cross-substitution, not anything else about
//! either trait (party ruling F2; mirrors `tests/trybuild_orchestration.rs`).
//! The compile_fail hands a `&dyn ExecutionSandbox` to a
//! `fn(&dyn IsolationProvider)` (E0308: no shared super-trait ⇒ no upcast); the
//! pass-twin proves both traits are usable as independent `dyn` objects.
#[test]
fn ac1_execution_sandbox_is_a_sibling_of_isolation_provider() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/execution_sandbox/sibling_pass.rs");
    t.compile_fail("tests/trybuild/execution_sandbox/sibling_fails.rs");
}
