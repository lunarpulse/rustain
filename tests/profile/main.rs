//! Area test binary `profile` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 13 former `tests/*.rs` roots, compiled and linked once instead of 13 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[path = "../adapter_override_e2e.rs"]
mod adapter_override_e2e;

#[path = "../composition_event_tx.rs"]
mod composition_event_tx;

#[path = "../conformance_adapter_composition.rs"]
mod conformance_adapter_composition;

#[path = "../conformance_adapter_override.rs"]
mod conformance_adapter_override;

#[path = "../conformance_config_reload.rs"]
mod conformance_config_reload;

#[path = "../conformance_profile.rs"]
mod conformance_profile;

#[path = "../conformance_profile_cli.rs"]
mod conformance_profile_cli;

#[path = "../conformance_profile_switch.rs"]
mod conformance_profile_switch;

#[path = "../integration_default_profile_exposes_task.rs"]
mod integration_default_profile_exposes_task;

#[path = "../integration_sub_task_decomposition.rs"]
mod integration_sub_task_decomposition;

#[path = "../profile_cli_integration.rs"]
mod profile_cli_integration;

#[path = "../profile_loading.rs"]
mod profile_loading;

#[path = "../profile_switch_e2e.rs"]
mod profile_switch_e2e;
