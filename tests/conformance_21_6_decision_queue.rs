//! Story 21.6 conformance: the decision layer and the Needs-you queue.
//!
//! Slice 6-A keystones K1 (queue store, chip), K2 (RU-6 arrival) and K3 (frame,
//! inert window, Esc table, stale, restore). One CI-wired module in the `tui`
//! area binary (`tests/tui/main.rs`). Every assertion reads a real rendered
//! `Buffer` (`presentation::compose`) or enters through a public front door (the
//! producers, `presentation::route`, `handle_input`); private seams (the quit
//! guard's fact gathering, the attached rig) are covered by `story21_6_` tests
//! next to them (`cargo test --lib story21_6_`).

#[path = "conformance_21_6_decision_queue/support.rs"]
pub(crate) mod support;

#[path = "conformance_21_6_decision_queue/k1_queue.rs"]
mod k1_queue;

#[path = "conformance_21_6_decision_queue/k2_arrival.rs"]
mod k2_arrival;

#[path = "conformance_21_6_decision_queue/k3_frame.rs"]
mod k3_frame;

#[path = "conformance_21_6_decision_queue/k4_permission.rs"]
mod k4_permission;

#[path = "conformance_21_6_decision_queue/k6_list.rs"]
mod k6_list;

#[path = "conformance_21_6_decision_queue/raisers.rs"]
pub(crate) mod raisers;

#[path = "conformance_21_6_decision_queue/k3b_effects.rs"]
mod k3b_effects;

#[path = "conformance_21_6_decision_queue/k7_question.rs"]
mod k7_question;

#[path = "conformance_21_6_decision_queue/k8_invariance.rs"]
mod k8_invariance;

#[path = "conformance_21_6_decision_queue/k16_patch.rs"]
mod k16_patch;
