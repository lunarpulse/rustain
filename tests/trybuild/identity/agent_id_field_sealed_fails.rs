// Story 17.1a AC1 / DD-4 — the `AgentId` inner field is SEALED (private).
// Forging an identity by direct tuple-struct construction from outside the
// crate must fail to compile (E0423). The sibling
// `agent_id_field_sealed_pass.rs` proves the type stays fully usable via its
// constructors — only the private-field construction path is blocked. Mutant:
// re-exposing `pub struct AgentId(pub String)` makes this file compile → the
// trybuild runner goes RED.
use rustain::domain::models::AgentId;

fn main() {
    // Attempting to forge an identity primitive at the trust root.
    let _forged = AgentId(String::from("forge"));
}
