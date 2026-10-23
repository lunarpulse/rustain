//! Area test binary `cli` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 15 former `tests/*.rs` roots, compiled and linked once instead of 15 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[path = "../cli_ask.rs"]
mod cli_ask;

#[path = "../cli_auth.rs"]
mod cli_auth;

#[path = "../cli_catalog.rs"]
mod cli_catalog;

#[path = "../cli_completions.rs"]
mod cli_completions;

#[path = "../cli_flags.rs"]
mod cli_flags;

#[path = "../conformance_auth_cli.rs"]
mod conformance_auth_cli;

#[path = "../conformance_catalog_cli.rs"]
mod conformance_catalog_cli;

#[path = "../conformance_release_workflow.rs"]
mod conformance_release_workflow;

#[path = "../conformance_session_cli.rs"]
mod conformance_session_cli;

#[path = "../conformance_startup_multi_provider.rs"]
mod conformance_startup_multi_provider;

#[path = "../doctor_health.rs"]
mod doctor_health;

#[path = "../doctor_offline.rs"]
mod doctor_offline;

#[path = "../model_catalog_e2e.rs"]
mod model_catalog_e2e;

#[path = "../session_persistence.rs"]
mod session_persistence;

#[path = "../startup_panic.rs"]
mod startup_panic;
