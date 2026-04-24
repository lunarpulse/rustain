//! Conformance tests for the CancellationToken tree + subprocess `kill_on_drop`.
//!
//! Source of truth:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-03-cancellation-token-tree.md`
//! - `_bmad-output/implementation-artifacts/6-0a-cancellation-and-event-bus.md`
//!
//! Rationale: Cancellation correctness is cross-cutting — a single tool that
//! ignores the cancel token or forgets `kill_on_drop(true)` leaves zombie
//! subprocesses and breaks Ctrl-C. These tests enforce the invariant across
//! every cancellable tool.
//!
//! Convention: each skeleton is `#[ignore]`-marked with a pointer to the
//! story + AC. Bodies are intentionally empty — running them under
//! `cargo test -- --include-ignored` reports "passed" (a no-op pass). When
//! the implementing story lands, the developer removes `#[ignore]` and fills
//! in real assertions. See `tests/CONFORMANCE_README.md`.

/// Story 6-0a AC1: `session_cancel` → `turn_cancel` → `call_cancel` hierarchy.
/// When implemented: cancel `turn_cancel` → `call_cancel` cancelled; cancel
/// `call_cancel` does NOT cancel parent.
#[test]
#[ignore = "pending story 6-0a AC1: CancellationToken tree"]
fn ac1_token_hierarchy_cascade() {}

/// Story 6-0a AC2: subprocess kill on cancel.
/// When implemented: spawn Bash with long-running `sleep 60`, cancel token,
/// assert `Err(ToolError::Cancelled)` within 200 ms and no leftover PID via
/// `/proc/<pid>` (Linux).
#[test]
#[ignore = "pending story 6-0a AC2: subprocess kill on cancel"]
fn ac2_bash_cancel_kills_subprocess() {}

/// Story 6-0a AC2: streaming-tool cooperative cancel.
/// When implemented: slow chunked read; cancel mid-read; tool observes cancel
/// between chunks and returns `Err(Cancelled)` within one chunk boundary.
#[test]
#[ignore = "pending story 6-0a AC2: streaming tool cooperative cancel"]
fn ac2_streaming_tool_cooperative_cancel() {}

/// Story 6-0a AC3: pending approval cancelled cleanly via select-arm.
/// When implemented: ApprovalRuntime has a pending approval with
/// `source == ForegroundTurn`; cancel the parent turn; assert the pending map
/// is empty and the receiver returned `Cancel`.
#[test]
#[ignore = "pending story 6-0a AC3: approval cancel-via-select"]
fn ac3_pending_approval_cleaned_on_turn_cancel() {}

/// Story 6-0a AC4: SIGTERM/SIGINT/SIGHUP cascade.
/// When implemented: simulate signal via a signal-hook test helper; assert
/// `session_cancel.is_cancelled()` within the graceful-shutdown budget (5 s).
#[test]
#[ignore = "pending story 6-0a AC4: signal cascade"]
fn ac4_signal_triggers_session_cancel() {}

/// Story 6-0a AC4: tab close cancels its turn only.
/// When implemented: close a tab, assert that tab's `turn_cancel` fires,
/// other tabs' `turn_cancel` remain un-cancelled.
#[test]
#[ignore = "pending story 6-0a AC4: tab close cancels turn"]
fn ac4_tab_close_cancels_turn_only() {}

/// Story 6-0a AC7: per-tool subprocess-leak sweep.
/// When implemented: drive every subprocess-spawning built-in through the
/// cancel harness; assert OS-level absence of leftover processes.
#[test]
#[ignore = "pending story 6-0a AC7: no-leftover-process sweep"]
fn ac7_no_leftover_subprocess_all_tools() {}
