//! Area test binary `mcp` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 13 former `tests/*.rs` roots, compiled and linked once instead of 13 times.
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

#[path = "../conformance_acp_mcp.rs"]
mod conformance_acp_mcp;

#[path = "../conformance_mcp_lifecycle.rs"]
mod conformance_mcp_lifecycle;

#[path = "../conformance_mcp_profile_swap.rs"]
mod conformance_mcp_profile_swap;

#[path = "../conformance_mcp_status_panel.rs"]
mod conformance_mcp_status_panel;

#[path = "../conformance_mcp_tool_invocation.rs"]
mod conformance_mcp_tool_invocation;

#[path = "../integration_mcp_approval_persistence.rs"]
mod integration_mcp_approval_persistence;

#[path = "../integration_mcp_env_expansion.rs"]
mod integration_mcp_env_expansion;

#[path = "../integration_mcp_http.rs"]
mod integration_mcp_http;

#[path = "../integration_mcp_resilience.rs"]
mod integration_mcp_resilience;

#[path = "../integration_mcp_routing.rs"]
mod integration_mcp_routing;

#[path = "../integration_mcp_tasks.rs"]
mod integration_mcp_tasks;

#[path = "../unit_mcp_content_rendering.rs"]
mod unit_mcp_content_rendering;

#[path = "../unit_mcp_unsupported_transport.rs"]
mod unit_mcp_unsupported_transport;
