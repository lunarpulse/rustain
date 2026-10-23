//! Area test binary `acp` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 4 former `tests/*.rs` roots, compiled and linked once instead of 4 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[path = "../acp_approval_source.rs"]
mod acp_approval_source;

#[path = "../acp_self_session.rs"]
mod acp_self_session;

#[path = "../acp_subprocess_no_listener.rs"]
mod acp_subprocess_no_listener;

#[path = "../conformance_acp_zed.rs"]
mod conformance_acp_zed;
