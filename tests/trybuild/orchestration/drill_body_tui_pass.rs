// AC6 type-wall PASS-TWIN (Story 14.3). The drill body that a TUI renders is an
// opaque DTO: its tuple field is `pub(crate)` (never constructible or readable
// from outside the crate) and its sole public surface is `as_render_str()`.
// Naming the type and calling the render accessor on an opaque value compiles —
// proving the ONLY failure cause in the sibling `drill_body_domain_fails.rs` is
// the private-field access, not anything else about the type.
//
// The TUI never constructs a DrillBody; it receives one from the infra handle
// (`ForkJoinRun::drill_body`, which is crate-private). We mirror that by taking
// the value as a parameter — the legitimate opaque-render path.
use rustain::domain::models::orchestration::{DrillBody, DrillId};

/// The legitimate render scope: opaque DTO in, compact render string out.
fn render_drill(body: &DrillBody) -> String {
    body.as_render_str().to_string()
}

fn main() {
    // DrillBody is nameable from outside the crate (it is `pub`) and its sole
    // public accessor `as_render_str` compiles. Pinning the function value keeps
    // this test live without constructing the DTO.
    let _render: fn(&DrillBody) -> String = render_drill;
    // DrillId — the opaque lookup key returned by drill_source — is likewise
    // nameable. Bound in a type position to prove reachability without a value.
    let _id: Option<&DrillId> = None;
}
