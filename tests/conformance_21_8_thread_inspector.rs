//! Story 21.8 slice 8-A conformance: the S-THREAD window, local-host halves of the
//! keystones K1 (open), K2 (trap and exits), K3 (invariance), K4 (identity and body),
//! K6 (LocalOnly offers no writer row) and K7 (paging, clamp, layout cache).
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Every item is a
//! real received recipient item appended through the production room journal; the window
//! is opened through the `/team view <id>` command front door, keys travel as crossterm
//! events through `convert_crossterm_event` and `handle_operator_input`, and every
//! assertion reads painted cells, the one frame on the stack, the composer or the
//! journal. The attached halves are in-file tests next to the attached host
//! (`cargo test --lib story21_8_`); the K6 cutover gate is `tests/**/k6_gate.rs`.
#[path = "conformance_21_8_thread_inspector/support.rs"]
pub(crate) mod support;

#[path = "conformance_21_8_thread_inspector/k1_open.rs"]
mod k1_open;

#[path = "conformance_21_8_thread_inspector/k2_trap.rs"]
mod k2_trap;

#[path = "conformance_21_8_thread_inspector/k3_invariance.rs"]
mod k3_invariance;

#[path = "conformance_21_8_thread_inspector/k4_identity.rs"]
mod k4_identity;

#[path = "conformance_21_8_thread_inspector/k6_context_local.rs"]
mod k6_context_local;

#[path = "conformance_21_8_thread_inspector/k7_nav_cutover.rs"]
mod k7_nav_cutover;

#[path = "conformance_21_8_thread_inspector/k8_markdown.rs"]
mod k8_markdown;

#[path = "conformance_21_8_thread_inspector/k11_copy.rs"]
mod k11_copy;

#[path = "conformance_21_8_thread_inspector/k12_search.rs"]
mod k12_search;

#[path = "conformance_21_8_thread_inspector/k13_viewport.rs"]
mod k13_viewport;

#[path = "conformance_21_8_thread_inspector/k15_sent.rs"]
mod k15_sent;

#[path = "conformance_21_8_thread_inspector/k16_retract.rs"]
mod k16_retract;

#[path = "conformance_21_8_thread_inspector/k18_summary.rs"]
mod k18_summary;
