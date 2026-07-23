// Pass-twin for the AgentId seal proof (Story 17.1a AC1 / DD-4). Proves
// `AgentId` remains fully usable from outside the crate via its public
// constructors — the ONLY thing blocked by sealing the inner field is direct
// tuple-struct construction (see the `compile_fail` sibling). If this stopped
// compiling, the failure cause would be the type or access path, not the seal.
use rustain::domain::models::AgentId;

fn main() {
    let _a = AgentId::new();
    let _b = AgentId::parse("seg/child").unwrap();
    let _c = AgentId::from_peer_path("peer/child").unwrap();
    let _d = AgentId::root();
}
