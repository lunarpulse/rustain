//! Conformance tests for `ApprovalRuntime` pub/sub.
//!
//! Source of truth:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-01-approval-runtime-pubsub.md`
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-05-explicit-approval-source.md`
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-07-first-match-wins-permission-rules.md`
//! - `_bmad-output/implementation-artifacts/6-0c-approvalruntime-pubsub.md`
//!
//! Rationale: The ApprovalRuntime coordinates approvals from foreground turns,
//! foreground subagents, and background agents. Every downstream (TUI, wire
//! log, scheduler, subagent runner) depends on its contract holding. These
//! tests enforce pub/sub semantics, fast-path auto-approve, cancel-by-source,
//! rule evaluation, and persistence round-trip.
//!
//! Convention: `#[ignore]`-marked skeletons with empty bodies. See
//! `tests/CONFORMANCE_README.md`.

/// AC1: `RequestId` is a nanoid(12) string; `ApprovalSource` has 3 variants
/// with the expected fields.
#[test]
#[ignore = "pending story 6-0c AC1: shared primitives"]
fn ac1_request_id_and_approval_source_shapes() {}

/// AC2: `ApprovalRuntime::new` exposes `request`, `resolve`, `cancel_by_source`,
/// `subscribe`.
#[test]
#[ignore = "pending story 6-0c AC2: runtime construction + API"]
fn ac2_runtime_api_surface() {}

/// AC3: Fast-path `always_tools` auto-approves without emitting `Requested`.
/// When implemented: seed `session_set.always_tools` with "Bash"; call
/// `request(...)` for "Bash"; assert receiver resolves immediately with `Once`
/// and no event is emitted on `events`.
#[test]
#[ignore = "pending story 6-0c AC3: fast-path for always_tools"]
fn ac3_fast_path_always_tool_auto_approves() {}

/// AC3: Fast-path `always_servers` auto-approves.
#[test]
#[ignore = "pending story 6-0c AC3: fast-path for always_servers"]
fn ac3_fast_path_always_server_auto_approves() {}

/// AC3: Fast-path `always_paths` glob-match auto-approves (first-match-wins).
#[test]
#[ignore = "pending story 6-0c AC3: fast-path for always_paths glob-match"]
fn ac3_fast_path_path_glob_auto_approves() {}

/// AC3: Slow path emits `Requested` event and inserts into `pending`.
#[test]
#[ignore = "pending story 6-0c AC3: slow-path emits Requested event"]
fn ac3_slow_path_emits_requested_event() {}

/// AC4: `AlwaysTool` outcome updates `session_set.always_tools` so the next
/// request for the same tool hits the fast-path.
#[test]
#[ignore = "pending story 6-0c AC4: AlwaysTool updates session set"]
fn ac4_always_tool_updates_set() {}

/// AC4: `AlwaysAndSave { scope: Tool }` persists to `~/.rustain/config.toml`.
#[test]
#[ignore = "pending story 6-0c AC4: AlwaysAndSave persists to user config"]
fn ac4_always_and_save_tool_persists_to_user_config() {}

/// AC4: `AlwaysAndSave { scope: PathPrefix }` persists to `{workspace}/.rustain/permissions.toml`.
#[test]
#[ignore = "pending story 6-0c AC4: AlwaysAndSave path persists to workspace config"]
fn ac4_always_and_save_path_persists_to_workspace_config() {}

/// AC4: `Reject { feedback: Some(text) }` routes `text` to the model as
/// `ExitPlanMode` tool-result content (integration test via scheduler).
#[test]
#[ignore = "pending story 6-0c AC4: Reject routes feedback to model"]
fn ac4_reject_feedback_reaches_model_via_tool_result() {}

/// AC5: `cancel_by_source` drains matching pending only; others unaffected.
/// When implemented: queue 3 pending (2 subagent, 1 foreground-turn); call
/// `cancel_by_source(&subagent_source, SourceAborted)`; assert the 2 subagent
/// entries are removed and receive `Cancel`; the foreground entry remains.
#[test]
#[ignore = "pending story 6-0c AC5: cancel_by_source drains matching only"]
fn ac5_cancel_by_source_drains_matching_only() {}

/// AC6: Rules sorted by `priority` desc, file-order tie-break.
#[test]
#[ignore = "pending story 6-0c AC6: rules sorted by priority desc"]
fn ac6_rule_priority_ordering() {}

/// AC6: First-match-wins — specific deny before catch-all allow fires deny.
#[test]
#[ignore = "pending story 6-0c AC6: first-match-wins"]
fn ac6_first_match_wins() {}

/// AC6: `rustain doctor` warns when no catch-all rule exists in the rule file.
#[test]
#[ignore = "pending story 6-0c AC6: absence of catch-all warns in doctor"]
fn ac6_no_catchall_rule_warns() {}

/// AC7: `turn.rs` has no direct oneshot approval construction after migration.
#[test]
#[ignore = "pending story 6-0c AC7: turn.rs migration cleanliness"]
fn ac7_turn_rs_no_direct_oneshot_approval() {}

/// AC8: TUI permission widget renders `[subagent: <type>]` prefix for
/// `ForegroundSubagent`-sourced approvals.
#[test]
#[ignore = "pending story 6-0c AC8: subagent prompt prefix rendering"]
fn ac8_subagent_approval_header_prefix() {}

/// AC9: Broadcast lag does not panic — subscriber receives `Err(Lagged(n))`
/// and logs a warn; runtime continues.
#[test]
#[ignore = "pending story 6-0c AC9: broadcast lag does not panic"]
fn ac9_slow_subscriber_lag_logged_not_panicked() {}

/// AC10: `AlwaysAndSave` round-trips across process restart — new runtime
/// instantiated with same config files starts with populated `session_set`.
#[test]
#[ignore = "pending story 6-0c AC10: persistence across restart"]
fn ac10_persistence_round_trip() {}

/// Concurrency regression: 100 parallel requests from 3 sources; random
/// resolves; assert `pending` map empty at end; no deadlocks.
#[test]
#[ignore = "pending story 6-0c concurrency regression"]
fn concurrency_100_requests_no_leaks() {}
