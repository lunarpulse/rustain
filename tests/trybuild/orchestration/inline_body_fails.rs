// AC5 type-wall COMPILE_FAIL (Story 14.3). The symbolic handle a synthesis
// builds from (`SpokeHandle`) carries NO body field — inlining a full payload
// into the prompt is a type error, not a render rule. The sibling
// `inline_body_pass.rs` compiles, proving the ONLY failure cause here is the
// inlined-body attempt (the same handle reads its compact metadata fine).
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
    // This MUST NOT compile — `SpokeHandle` has no `body` field. The full
    // payload lives in the `ResultStore` side-table (drill-on-open), never on
    // the handle that enters the prompt window.
    let _inlined_body: &str = handle.body;
}
