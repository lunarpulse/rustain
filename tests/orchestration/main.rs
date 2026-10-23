//! Area test binary `orchestration` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 46 former `tests/*.rs` roots, compiled and linked once instead of 46 times.
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

#[path = "../conformance_17_1a_envelope_signed_header.rs"]
mod conformance_17_1a_envelope_signed_header;

#[path = "../conformance_17_1a_rap_task_state.rs"]
mod conformance_17_1a_rap_task_state;

#[path = "../conformance_17_2a_domain_purity.rs"]
mod conformance_17_2a_domain_purity;

#[path = "../conformance_17_2a_successor.rs"]
mod conformance_17_2a_successor;

#[path = "../conformance_agent_message_bus.rs"]
mod conformance_agent_message_bus;

#[path = "../conformance_agent_message_bus_receipt_honesty.rs"]
mod conformance_agent_message_bus_receipt_honesty;

#[path = "../conformance_agent_message_bus_wiring.rs"]
mod conformance_agent_message_bus_wiring;

#[path = "../conformance_authority_provider.rs"]
mod conformance_authority_provider;

#[path = "../conformance_capability_registry.rs"]
mod conformance_capability_registry;

#[path = "../conformance_cow_mergeback.rs"]
mod conformance_cow_mergeback;

#[path = "../conformance_isolation.rs"]
mod conformance_isolation;

#[path = "../conformance_no_std_sync_in_subagent_async.rs"]
mod conformance_no_std_sync_in_subagent_async;

#[path = "../conformance_node_tree.rs"]
mod conformance_node_tree;

#[path = "../conformance_plan_card.rs"]
mod conformance_plan_card;

#[path = "../conformance_plan_mode.rs"]
mod conformance_plan_mode;

#[path = "../conformance_plan_runtime.rs"]
mod conformance_plan_runtime;

#[path = "../conformance_sandbox.rs"]
mod conformance_sandbox;

#[path = "../conformance_sandbox_inheritance.rs"]
mod conformance_sandbox_inheritance;

#[path = "../conformance_subagent_approval_routing.rs"]
mod conformance_subagent_approval_routing;

#[path = "../conformance_subagent_auto_approve.rs"]
mod conformance_subagent_auto_approve;

#[path = "../conformance_subagent_ownership_propagation.rs"]
mod conformance_subagent_ownership_propagation;

#[path = "../conformance_subagent_panel_read_only_tab.rs"]
mod conformance_subagent_panel_read_only_tab;

#[path = "../conformance_subagent_provider_protocol.rs"]
mod conformance_subagent_provider_protocol;

#[path = "../conformance_subagent_recursion_guard.rs"]
mod conformance_subagent_recursion_guard;

#[path = "../conformance_subagent_registry_no_app_state.rs"]
mod conformance_subagent_registry_no_app_state;

#[path = "../conformance_subagent_task_through_tool_scheduler.rs"]
mod conformance_subagent_task_through_tool_scheduler;

#[path = "../conformance_task_control.rs"]
mod conformance_task_control;

#[path = "../conformance_task_panel.rs"]
mod conformance_task_panel;

#[path = "../conformance_toolcall_fsm.rs"]
mod conformance_toolcall_fsm;

#[path = "../custom_agent_invocation.rs"]
mod custom_agent_invocation;

#[path = "../integration_delegation.rs"]
mod integration_delegation;

#[path = "../integration_delegation_fallback.rs"]
mod integration_delegation_fallback;

#[path = "../integration_legacy_agent_prompt_only.rs"]
mod integration_legacy_agent_prompt_only;

#[path = "../integration_plan_runtime_delegation_happy_path.rs"]
mod integration_plan_runtime_delegation_happy_path;

#[path = "../integration_plan_runtime_delegation_summary.rs"]
mod integration_plan_runtime_delegation_summary;

#[path = "../integration_plan_runtime_parallel_delegation.rs"]
mod integration_plan_runtime_parallel_delegation;

#[path = "../integration_subagent_cascade_kill.rs"]
mod integration_subagent_cascade_kill;

#[path = "../integration_task_tool_budget.rs"]
mod integration_task_tool_budget;

#[path = "../integration_task_tool_cancel.rs"]
mod integration_task_tool_cancel;

#[path = "../integration_task_tool_concurrent.rs"]
mod integration_task_tool_concurrent;

#[path = "../integration_task_tool_dispatch.rs"]
mod integration_task_tool_dispatch;

#[path = "../trybuild_execution_sandbox.rs"]
mod trybuild_execution_sandbox;

#[path = "../trybuild_orchestration.rs"]
mod trybuild_orchestration;

#[path = "../turn_scheduler_migration.rs"]
mod turn_scheduler_migration;

#[path = "../view_state_anchor_fsm.rs"]
mod view_state_anchor_fsm;

#[path = "../wasm_execution_sandbox.rs"]
mod wasm_execution_sandbox;
