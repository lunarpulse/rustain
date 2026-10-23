//! Area test binary `core` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 42 former `tests/*.rs` roots, compiled and linked once instead of 43 times.
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

#[path = "../abort.rs"]
mod abort;

#[path = "../anthropic_streaming.rs"]
mod anthropic_streaming;

#[path = "../autocomplete.rs"]
mod autocomplete;

#[path = "../color_detect.rs"]
mod color_detect;

#[path = "../compaction.rs"]
mod compaction;

#[path = "../conformance.rs"]
mod conformance;

#[path = "../conformance_cancel_race.rs"]
mod conformance_cancel_race;

#[path = "../conformance_cancellation.rs"]
mod conformance_cancellation;

#[path = "../conformance_provider.rs"]
mod conformance_provider;

#[path = "../conformance_reducer_interleaving.rs"]
mod conformance_reducer_interleaving;

#[path = "../conformance_secret_redaction.rs"]
mod conformance_secret_redaction;

#[path = "../context_rebuild.rs"]
mod context_rebuild;

#[path = "../cost_tracking.rs"]
mod cost_tracking;

#[path = "../domain_models.rs"]
mod domain_models;

#[path = "../event_loop.rs"]
mod event_loop;

#[path = "../file_scanner.rs"]
mod file_scanner;

#[path = "../golden_turn_serde.rs"]
mod golden_turn_serde;

#[path = "../init_wizard.rs"]
mod init_wizard;

#[path = "../integration_path_b_routing.rs"]
mod integration_path_b_routing;

#[path = "../keyboard.rs"]
mod keyboard;

#[path = "../logging.rs"]
mod logging;

#[path = "../message_builder.rs"]
mod message_builder;

#[path = "../navigation.rs"]
mod navigation;

#[path = "../panic_hook.rs"]
mod panic_hook;

#[path = "../passthrough_unchanged.rs"]
mod passthrough_unchanged;

#[path = "../ports_and_noop.rs"]
mod ports_and_noop;

#[path = "../project_context.rs"]
mod project_context;

#[path = "../prop_synonym_collisions.rs"]
mod prop_synonym_collisions;

#[path = "../property_reducer_order.rs"]
mod property_reducer_order;

#[path = "../regression_concat_prose_2026_04_22.rs"]
mod regression_concat_prose_2026_04_22;

#[path = "../regression_legacy_messages_silent_migration.rs"]
mod regression_legacy_messages_silent_migration;

#[path = "../security.rs"]
mod security;

#[path = "../signals.rs"]
mod signals;

#[path = "../streaming_e2e.rs"]
mod streaming_e2e;

#[path = "../system_communication.rs"]
mod system_communication;

#[path = "../terminal_lifecycle.rs"]
mod terminal_lifecycle;

#[path = "../title_generation.rs"]
mod title_generation;

#[path = "../trybuild_identity.rs"]
mod trybuild_identity;

#[path = "../trybuild_ownership.rs"]
mod trybuild_ownership;

#[path = "../turn_origin.rs"]
mod turn_origin;

#[path = "../turn_queue.rs"]
mod turn_queue;

#[path = "../view_state_fold_toggle_anchor.rs"]
mod view_state_fold_toggle_anchor;
