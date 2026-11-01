//! Story 21.1 conformance: notification lifecycle (slice S1 keystones K1-K4,
//! K7, K8, K11a; slice S2 adds K10 and turns K11a into the K11 gate).
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Every
//! assertion reads a real rendered `Buffer` or enters through a public front
//! door (`handle_input`, `submit_message_for_test`, `notification_log`
//! ingress, `status_bar::render`, `toast_layer::render`); private host seams
//! (`system_notice` consumer, `notice_actions`, the attached key/draw path) are
//! covered by `story21_1_` tests next to them (`cargo test --lib story21_1_`).

#[path = "conformance_21_1_notification_lifecycle/support.rs"]
mod support;

#[path = "conformance_21_1_notification_lifecycle/k1_ttl.rs"]
mod k1_ttl;

#[path = "conformance_21_1_notification_lifecycle/k2_store.rs"]
mod k2_store;

#[path = "conformance_21_1_notification_lifecycle/k3_geometry.rs"]
mod k3_geometry;

#[path = "conformance_21_1_notification_lifecycle/k4_retention.rs"]
mod k4_retention;

#[path = "conformance_21_1_notification_lifecycle/k6_families.rs"]
mod k6_families;

#[path = "conformance_21_1_notification_lifecycle/k7_log.rs"]
mod k7_log;

#[path = "conformance_21_1_notification_lifecycle/k8_chords.rs"]
mod k8_chords;

#[path = "conformance_21_1_notification_lifecycle/k10_pane.rs"]
mod k10_pane;

#[path = "conformance_21_1_notification_lifecycle/k11_gate.rs"]
mod k11_gate;
