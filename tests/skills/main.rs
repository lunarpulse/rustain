//! Area test binary `skills` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 14 former `tests/*.rs` roots, compiled and linked once instead of 14 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[path = "../autocomplete_skills.rs"]
mod autocomplete_skills;

#[path = "../command_interpolation.rs"]
mod command_interpolation;

#[path = "../command_normalize.rs"]
mod command_normalize;

#[path = "../command_registry.rs"]
mod command_registry;

#[path = "../conformance_skill_exposure.rs"]
mod conformance_skill_exposure;

#[path = "../conformance_skill_registry_sharing.rs"]
mod conformance_skill_registry_sharing;

#[path = "../conformance_tool_exposure.rs"]
mod conformance_tool_exposure;

#[path = "../conformance_tool_progress.rs"]
mod conformance_tool_progress;

#[path = "../custom_slash_command_invocation.rs"]
mod custom_slash_command_invocation;

#[path = "../integration_skill_eval_harness.rs"]
mod integration_skill_eval_harness;

#[path = "../permission_chain_skills.rs"]
mod permission_chain_skills;

#[path = "../skill_activation.rs"]
mod skill_activation;

#[path = "../skill_discovery.rs"]
mod skill_discovery;

#[path = "../skill_restriction_notice.rs"]
mod skill_restriction_notice;
