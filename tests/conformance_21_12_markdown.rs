//! Story 21.12 conformance: markdown structure, sanitizer and invariants
//! (slice 12-A pipeline core, slice 12-B tables and producers, slice 12-C links).
//!
//! One CI-wired module in the `tui` area binary (`tests/tui/main.rs`). Each
//! keystone drives the real chat-pane front doors (`render_with_search` for the
//! local host, `render_attached` for the attached host) into a `TestBackend`
//! buffer and reads the painted cells back; none asserts on a parse tree or
//! builds an expected row from the formula under test.
//!
//! * K1 lists (`k1_lists.rs`), K2 literal prose and K3 whitespace
//!   (`k2_k3_prose.rs`), K7 sanitizer (`k7_sanitizer.rs`), K8 literal user
//!   messages (`k8_literal_user.rs`);
//! * K4 tables (`k4_tables.rs`), K5 cell escaping and K6 summary truncation
//!   (`k5_k6_summary.rs`), K12 export (`k12_export.rs`);
//! * K10 height truth and the scoped AD-2112 gate, K11 robustness
//!   (`k10_k11_invariants.rs`);
//! * the DF-075 measurement (`k_measure.rs`, `#[ignore]`d).
//!
//! * K9 links and OSC 8, K13 host suffix (`k9_k13_links.rs`).

#[path = "conformance_21_12_markdown/support.rs"]
mod support;

#[path = "conformance_21_12_markdown/k1_lists.rs"]
mod k1_lists;

#[path = "conformance_21_12_markdown/k2_k3_prose.rs"]
mod k2_k3_prose;

#[path = "conformance_21_12_markdown/k7_sanitizer.rs"]
mod k7_sanitizer;

#[path = "conformance_21_12_markdown/k8_literal_user.rs"]
mod k8_literal_user;

#[path = "conformance_21_12_markdown/k4_tables.rs"]
mod k4_tables;

#[path = "conformance_21_12_markdown/k5_k6_summary.rs"]
mod k5_k6_summary;

#[path = "conformance_21_12_markdown/k12_export.rs"]
mod k12_export;

#[path = "conformance_21_12_markdown/k10_k11_invariants.rs"]
mod k10_k11_invariants;

#[path = "conformance_21_12_markdown/k9_k13_links.rs"]
mod k9_k13_links;

#[path = "conformance_21_12_markdown/k_measure.rs"]
mod k_measure;

/// DF-075 measurement (AC10): run once with
/// `cargo test --test tui conformance_21_12_markdown::k_measure_stream_cost -- --ignored --nocapture`.
#[test]
#[ignore = "DF-075 measurement: run once with --ignored --nocapture"]
fn k_measure_stream_cost() {
    k_measure::run();
}
