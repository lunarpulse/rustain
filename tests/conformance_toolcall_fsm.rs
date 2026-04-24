//! Conformance tests for `ToolCall` 7-variant FSM and `ToolScheduler` pipeline.
//!
//! Source of truth:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-02-toolcall-enum-fsm.md`
//! - `_bmad-output/implementation-artifacts/6-0b-toolscheduler-toolcall-fsm.md`
//!
//! Rationale: The scheduler FSM is the single coordination point for tool
//! execution. Invalid transitions or missed emissions break every downstream
//! consumer (TUI tool block, task panel, wire log, metrics). These tests
//! enumerate legal transitions and forbid illegal ones.
//!
//! Convention: `#[ignore]`-marked skeletons with empty bodies. See
//! `tests/CONFORMANCE_README.md`.

/// AC1: Enum has exactly 7 variants, each with required fields; serde round-trip.
/// When implemented: construct each variant, serialize via `serde_json`,
/// deserialize, assert the variant survives exactly (`tag = "status"`,
/// snake_case).
#[test]
#[ignore = "pending story 6-0b AC1: ToolCall enum shape + serde round-trip"]
fn ac1_enum_shape_and_serde_round_trip() {}

/// AC2: `ToolScheduler::new` + `subscribe` + `schedule` signatures and wiring.
/// When implemented: instantiate scheduler with fake ports; assert `subscribe`
/// returns a fresh receiver at the current tail.
#[test]
#[ignore = "pending story 6-0b AC2: ToolScheduler API surface"]
fn ac2_scheduler_api_surface() {}

/// AC3: Legal transition — happy path (Validating → Scheduled → Executing → Success).
/// When implemented: drive a Read tool call through the scheduler with a fake
/// security port returning `Allow`; assert the observed transitions match.
#[test]
#[ignore = "pending story 6-0b AC3: legal transition — happy path"]
fn ac3_transition_happy_path() {}

/// AC3: Legal transition — with approval.
/// When implemented: policy returns `Ask`; approval resolves `Once`; assert
/// Validating → Scheduled → AwaitingApproval → Executing → Success.
#[test]
#[ignore = "pending story 6-0b AC3: legal transition — with approval"]
fn ac3_transition_with_approval() {}

/// AC3: Legal transition — invalid input fails fast.
/// When implemented: validate_input returns Err; assert Validating → Error
/// without traversing Scheduled.
#[test]
#[ignore = "pending story 6-0b AC3: legal transition — invalid input"]
fn ac3_transition_invalid_input_fails_fast() {}

/// AC3: Legal transition — policy denial.
/// When implemented: policy returns `Deny`; assert Validating → Scheduled → Error.
#[test]
#[ignore = "pending story 6-0b AC3: legal transition — policy denial"]
fn ac3_transition_policy_denial() {}

/// AC3: Legal transition — user rejection with feedback.
/// When implemented: approval resolves `Reject { feedback: Some("nope") }`;
/// assert Error.error == "nope".
#[test]
#[ignore = "pending story 6-0b AC3: legal transition — user rejection with feedback"]
fn ac3_transition_user_rejection_with_feedback() {}

/// AC3: Cancellation mid-execute → Cancelled with "cancelled-during-execute".
#[test]
#[ignore = "pending story 6-0b AC3: cancellation mid-execute"]
fn ac3_transition_cancel_during_execute() {}

/// AC3: Cancellation mid-approval → Cancelled with "cancelled-during-approval",
/// and `ApprovalRuntime::cancel_by_source` was invoked.
#[test]
#[ignore = "pending story 6-0b AC3: cancellation mid-approval"]
fn ac3_transition_cancel_during_approval() {}

/// AC4: Parallel batch when all `parallel_safe`.
/// When implemented: 3 read-only tools with 100ms sleep each; assert total
/// wall-clock time < 200ms (parallel), not 300ms (sequential).
#[test]
#[ignore = "pending story 6-0b AC4: parallel batch when all parallel_safe"]
fn ac4_parallel_batch_all_safe() {}

/// AC4: Sequential fallback when any `parallel_safe == false`.
/// When implemented: batch [Read, Bash, Glob]; assert Read does not start
/// while Bash is executing (sequential ordering).
#[test]
#[ignore = "pending story 6-0b AC4: sequential fallback when any parallel_safe=false"]
fn ac4_sequential_when_any_unsafe() {}

/// AC4: Built-in `parallel_safe` flags.
/// When implemented: assert Read/Glob/Grep/WebFetch are true; Bash/Write/Edit/ExitPlanMode are false.
#[test]
#[ignore = "pending story 6-0b AC4: built-in parallel_safe table"]
fn ac4_builtin_parallel_safe_flags() {}

/// AC6: Policy `Allow` → Executing.
#[test]
#[ignore = "pending story 6-0b AC6: policy Allow → Executing"]
fn ac6_policy_allow_proceeds() {}

/// AC6: Policy `Deny` → Error.
#[test]
#[ignore = "pending story 6-0b AC6: policy Deny → Error"]
fn ac6_policy_deny_emits_error() {}

/// AC6: Policy `Ask` → AwaitingApproval (calls ApprovalRuntime::request).
#[test]
#[ignore = "pending story 6-0b AC6: policy Ask → AwaitingApproval"]
fn ac6_policy_ask_routes_to_approval_runtime() {}

/// AC7: `turn.rs` delegates to scheduler — no residual tool-execution logic.
/// When implemented: grep `src/infrastructure/runtime/turn.rs` for direct
/// tool-execution / permission-check patterns; assert none remain.
#[test]
#[ignore = "pending story 6-0b AC7: turn.rs migration cleanliness"]
fn ac7_turn_rs_delegates_to_scheduler() {}
