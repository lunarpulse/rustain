//! Area test binary `daemon` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 4 former `tests/*.rs` roots, compiled and linked once instead of 4 times.
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

#[path = "../conformance_daemon_cli.rs"]
mod conformance_daemon_cli;

#[path = "../crash_recovery.rs"]
mod crash_recovery;

#[path = "../daemon_crash_recovery.rs"]
mod daemon_crash_recovery;

#[path = "../daemon_lifecycle.rs"]
mod daemon_lifecycle;
