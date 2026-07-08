// AC5 type-wall PASS-TWIN (Story 14.3). The symbolic handle exposes compact
// metadata (agent_id / label / status / salience); reading any of those
// compiles. This is the "pass" half of the compile_fail + pass-twin pair — it
// proves the ONLY failure cause in the sibling `inline_body_fails.rs` is the
// inlined-body attempt (a non-existent `body` field), not anything else about
// the handle.
use rustain::domain::models::agent_id::AgentId;
use rustain::domain::models::node_state::NodeState;
use rustain::infrastructure::orchestrator::SpokeHandle;

fn main() {
    let handle = SpokeHandle {
        agent_id: AgentId("a".into()),
        label: "alpha".into(),
        status: NodeState::Completed,
        salience: "found 3 races".into(),
    };
    // The prompt-build scope reads compact metadata ONLY — this compiles,
    // proving the type-wall permits symbolic composition.
    assert_eq!(handle.label, "alpha");
    assert_eq!(handle.salience, "found 3 races");
    assert_eq!(handle.status, NodeState::Completed);
}
