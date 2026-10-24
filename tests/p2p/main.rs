//! Area test binary `p2p` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 9 former `tests/*.rs` roots, compiled and linked once instead of 9 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[path = "../conformance_18_4a_p2p_topic.rs"]
mod conformance_18_4a_p2p_topic;

#[path = "../conformance_18_10_p2p_topic_received.rs"]
mod conformance_18_10_p2p_topic_received;

#[path = "../conformance_p2p_architecture.rs"]
mod conformance_p2p_architecture;

#[path = "../conformance_p2p_dial_verdict.rs"]
mod conformance_p2p_dial_verdict;

#[path = "../conformance_p2p_ingress.rs"]
mod conformance_p2p_ingress;

#[path = "../conformance_p2p_peer_admission.rs"]
mod conformance_p2p_peer_admission;

#[path = "../conformance_p2p_reach.rs"]
mod conformance_p2p_reach;

#[path = "../conformance_p2p_relay.rs"]
mod conformance_p2p_relay;

#[path = "../conformance_p2p_relay_server.rs"]
mod conformance_p2p_relay_server;

#[path = "../conformance_p2p_transport.rs"]
mod conformance_p2p_transport;
