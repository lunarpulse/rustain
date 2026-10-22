//! Story 19.20c production-path K01/K05. Fixtures supply provider outputs and
//! capture executor input; pending approvals, decisions, and presentation are
//! always produced by the real local turn, scheduler, bridge, and TUI dispatcher.
#[path = "conformance_19_20c_standalone/harness.rs"]
mod harness;
#[path = "conformance_19_20c_standalone/k01_exact.rs"]
mod k01_exact;
#[path = "conformance_19_20c_standalone/k05_ownership.rs"]
mod k05_ownership;
#[path = "conformance_19_20c_standalone/k20a_resolution.rs"]
mod k20a_resolution;
