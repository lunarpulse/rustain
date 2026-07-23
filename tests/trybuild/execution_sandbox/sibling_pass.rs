// AC1 sibling-port PASS-TWIN (Story 17.3a, DF-14-5-3). Both `IsolationProvider`
// and `ExecutionSandbox` are usable as independent `dyn` trait objects — this
// compiles. It is the "pass" half of the compile_fail + pass-twin pair: it
// proves the ONLY failure cause in the sibling `sibling_fails.rs` is the
// attempt to substitute one trait object for the other (no shared super-trait,
// no upcast), not anything else about either trait.
#![allow(dead_code)]
use rustain::domain::ports::{ExecutionSandbox, IsolationProvider};

fn wants_isolation(_: &dyn IsolationProvider) {}
fn wants_sandbox(_: &dyn ExecutionSandbox) {}

fn main() {
    // Each trait object flows into its OWN consumer — no cross-substitution.
    // Referencing both as fn pointers proves both `dyn` types are well-formed
    // and object-safe, and that they are two distinct, independent seams.
    let _iso: fn(&dyn IsolationProvider) = wants_isolation;
    let _sbx: fn(&dyn ExecutionSandbox) = wants_sandbox;
}
