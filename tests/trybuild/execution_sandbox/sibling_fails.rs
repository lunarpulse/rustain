// AC1 sibling-port COMPILE_FAIL (Story 17.3a, DF-14-5-3). `ExecutionSandbox` is
// a SIBLING of `IsolationProvider`, sharing no super-trait — so a
// `&dyn ExecutionSandbox` cannot be handed where a `&dyn IsolationProvider` is
// wanted. No upcast exists ⇒ E0308 mismatched types. The sibling
// `sibling_pass.rs` compiles, proving the ONLY failure cause here is the
// cross-substitution (a leaky super-trait would make this compile and would be
// the R2-forcing premature abstraction §1A warns about).
#![allow(dead_code)]
use rustain::domain::ports::{ExecutionSandbox, IsolationProvider};

fn wants_isolation(_: &dyn IsolationProvider) {}

fn hand_sandbox_where_isolation_wanted(sandbox: &dyn ExecutionSandbox) {
    // MUST NOT compile: ExecutionSandbox does not implement / upcast to
    // IsolationProvider. If this ever compiled, the sibling seam would have
    // collapsed into a super-trait.
    wants_isolation(sandbox);
}

fn main() {}
