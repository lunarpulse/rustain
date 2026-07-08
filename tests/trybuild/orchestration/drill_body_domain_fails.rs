// AC6 type-wall COMPILE_FAIL (Story 14.3). The drill body is an opaque DTO:
// its tuple field is `pub(crate)`, so external code (the trybuild test compiles
// as a separate crate) cannot read the raw payload directly. The sibling
// `drill_body_tui_pass.rs` compiles, proving the ONLY failure cause here is the
// private-field access — the same opaque `&DrillBody` value renders fine via
// `as_render_str()` there; here we reach for the raw inner `.0` and hit E0616.
use rustain::domain::models::orchestration::DrillBody;

fn main() {
    // The TUI receives an opaque DrillBody (handed out by the infra handle) and
    // must read it ONLY via `as_render_str()`. Reaching for the raw inner
    // payload `.0` directly MUST NOT compile — the field is `pub(crate)`, so
    // field access from outside the crate is E0616 (private field). The inner
    // String never leaks across the crate boundary.
    fn leak(body: &DrillBody) {
        let _raw = &body.0;
    }
    let _ = leak;
}
