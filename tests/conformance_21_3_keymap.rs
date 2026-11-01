//! Story 21.3 conformance: the keymap registry (slice 3-A keystones K1-K4, K6).
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Local
//! legs enter through `convert_crossterm_event` + `app::handle_input`; the
//! attached legs, the `panel_bridge` K5 legs and the shadow-notice test live
//! next to their crate-private seams (`cargo test --lib story21_3_`).

#[path = "conformance_21_3_keymap/support.rs"]
mod support;

#[path = "conformance_21_3_keymap/k1_governance.rs"]
mod k1_governance;

#[path = "conformance_21_3_keymap/k2_leader.rs"]
mod k2_leader;

#[path = "conformance_21_3_keymap/k3_generated.rs"]
mod k3_generated;

#[path = "conformance_21_3_keymap/k4_input.rs"]
mod k4_input;

#[path = "conformance_21_3_keymap/k4_chat.rs"]
mod k4_chat;

#[path = "conformance_21_3_keymap/k4_nav.rs"]
mod k4_nav;

#[path = "conformance_21_3_keymap/k4_permission.rs"]
mod k4_permission;

#[path = "conformance_21_3_keymap/k4_truth.rs"]
mod k4_truth;

#[path = "conformance_21_3_keymap/k6_docs.rs"]
mod k6_docs;

#[path = "conformance_21_3_keymap/k12_gate.rs"]
mod k12_gate;
