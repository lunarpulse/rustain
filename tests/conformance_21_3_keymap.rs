//! Story 21.3 conformance: the keymap registry (slice 3-A keystones K1-K4, K6).
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Local
//! legs enter through `convert_crossterm_event` + `app::handle_input`; the
//! attached legs, the `panel_bridge` K5 legs and the shadow-notice test live
//! next to their crate-private seams (`cargo test --lib story21_3_`).

#[path = "conformance_21_3_keymap/support.rs"]
pub(super) mod support;

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

#[path = "conformance_21_3_keymap/witness.rs"]
mod witness;

#[path = "conformance_21_3_keymap/k4_panels.rs"]
mod k4_panels;

#[path = "conformance_21_3_keymap/k4_overlays.rs"]
mod k4_overlays;

#[path = "conformance_21_3_keymap/k8_text_entry.rs"]
mod k8_text_entry;

#[path = "conformance_21_3_keymap/k10_modal_back.rs"]
mod k10_modal_back;

#[path = "conformance_21_3_keymap/k11_decisions.rs"]
mod k11_decisions;

#[path = "conformance_21_3_keymap/k20_review_items.rs"]
mod k20_review_items;

#[path = "conformance_21_3_keymap/k12_gate.rs"]
pub(crate) mod k12_gate;

#[path = "conformance_21_3_keymap/k9_dispatch.rs"]
mod k9_dispatch;

#[path = "conformance_21_3_keymap/k9_tool.rs"]
mod k9_tool;

#[path = "conformance_21_3_keymap/k9_block.rs"]
mod k9_block;
