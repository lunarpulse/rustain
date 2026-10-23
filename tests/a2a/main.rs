//! Area test binary `a2a` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 37 former `tests/*.rs` roots, compiled and linked once instead of 37 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[allow(dead_code, unused_imports)]
#[path = "../common/mod.rs"]
mod common;

#[path = "../a2a_card.rs"]
mod a2a_card;

#[path = "../a2a_client.rs"]
mod a2a_client;

#[path = "../a2a_composition.rs"]
mod a2a_composition;

#[path = "../a2a_fixtures.rs"]
mod a2a_fixtures;

#[path = "../a2a_jws.rs"]
mod a2a_jws;

#[path = "../a2a_lifecycle.rs"]
mod a2a_lifecycle;

#[path = "../a2a_provider.rs"]
mod a2a_provider;

#[path = "../a2a_server.rs"]
mod a2a_server;

#[path = "../a2a_trust_registry.rs"]
mod a2a_trust_registry;

#[path = "../conformance_17_1a_peer_harness.rs"]
mod conformance_17_1a_peer_harness;

#[path = "../conformance_17_1a_peer_wire.rs"]
mod conformance_17_1a_peer_wire;

#[path = "../conformance_17_1a_real_peer.rs"]
mod conformance_17_1a_real_peer;

#[path = "../conformance_17_2a_artifact_store.rs"]
mod conformance_17_2a_artifact_store;

#[path = "../conformance_17_2a_room_projection.rs"]
mod conformance_17_2a_room_projection;

#[path = "../conformance_18_3a_b_addressing.rs"]
mod conformance_18_3a_b_addressing;

#[path = "../conformance_18_3a_c_artifacts.rs"]
mod conformance_18_3a_c_artifacts;

#[path = "../conformance_18_3a_room.rs"]
mod conformance_18_3a_room;

#[path = "../conformance_18_3b_policy.rs"]
mod conformance_18_3b_policy;

#[path = "../conformance_18_3d_consent.rs"]
mod conformance_18_3d_consent;

#[path = "../conformance_18_4b_peer_surface.rs"]
mod conformance_18_4b_peer_surface;

#[path = "../conformance_18_4d_reach_and_ping.rs"]
mod conformance_18_4d_reach_and_ping;

#[path = "../conformance_19_15_semantic_types.rs"]
mod conformance_19_15_semantic_types;

#[path = "../conformance_19_16_recipient_item.rs"]
mod conformance_19_16_recipient_item;

#[path = "../conformance_19_16b_board.rs"]
mod conformance_19_16b_board;

#[path = "../conformance_19_16d_retract.rs"]
mod conformance_19_16d_retract;

#[path = "../conformance_19_16f_retract_surface.rs"]
mod conformance_19_16f_retract_surface;

#[path = "../conformance_19_17_a2a_fanout.rs"]
mod conformance_19_17_a2a_fanout;

#[path = "../conformance_19_18_a2a_reply_thread.rs"]
mod conformance_19_18_a2a_reply_thread;

#[path = "../conformance_19_18d0_a2a_typed_policy_principal.rs"]
mod conformance_19_18d0_a2a_typed_policy_principal;

#[path = "../conformance_19_19_a2a_consultation.rs"]
mod conformance_19_19_a2a_consultation;

#[path = "../conformance_19_20a_provenance.rs"]
mod conformance_19_20a_provenance;

#[path = "../conformance_19_20c_standalone.rs"]
mod conformance_19_20c_standalone;

#[path = "../conformance_19_21_a2a_distribution.rs"]
mod conformance_19_21_a2a_distribution;

#[path = "../conformance_19_28_parent_restriction_rail.rs"]
mod conformance_19_28_parent_restriction_rail;

#[path = "../conformance_a2a_architecture.rs"]
mod conformance_a2a_architecture;

#[path = "../conformance_transparency.rs"]
mod conformance_transparency;

#[path = "../conformance_transparency_surfacing.rs"]
mod conformance_transparency_surfacing;
