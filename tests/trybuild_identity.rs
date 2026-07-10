//! trybuild runner for the `AgentId` field-seal proof (Story 17.1a, AC1/DD-4).
//!
//! Mirrors the Story 14.6 `trybuild_ownership.rs` pattern: a lone
//! `compile_fail` is decorative, so a `pass`-twin proves the ONLY failure
//! cause is the private-field construction attempt. `agent_id_field_sealed_fails.rs`
//! attempts `AgentId(String::from("forge"))` from outside the crate and hits
//! E0423 (cannot initialize a tuple struct with a private field);
//! `agent_id_field_sealed_pass.rs` exercises every public constructor, proving
//! the type is fully usable externally while all direct-construction (forgery)
//! paths are blocked — the AC1 mutant "re-expose the field" would flip this RED.

#[test]
fn ac1_agent_id_field_is_sealed() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/identity/agent_id_field_sealed_pass.rs");
    t.compile_fail("tests/trybuild/identity/agent_id_field_sealed_fails.rs");
}
