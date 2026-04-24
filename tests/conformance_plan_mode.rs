//! Conformance tests for the Plan Mode workflow.
//!
//! Source of truth:
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-10-plan-mode-reminder-injection.md`
//! - `_bmad-output/planning-artifacts/architecture/adr/ADR-06-04-orthogonal-sandbox-policy.md`
//! - `_bmad-output/implementation-artifacts/6-0d-plan-mode-workflow.md`
//!
//! Rationale: Plan mode is the single safety mechanism users rely on to
//! explore-before-execute. A bug in the injector cadence, the tool gate, or
//! the mode handoff turns Plan mode into a false sense of security. These
//! tests enforce every checkpoint of the flow.
//!
//! Convention: `#[ignore]`-marked skeletons with empty bodies. See
//! `tests/CONFORMANCE_README.md`.

/// AC1: `plan_file_for(conv)` produces a session-stable slug and path.
#[test]
#[ignore = "pending story 6-0d AC1: plan slug generation via petname"]
fn ac1_plan_slug_generated_once_per_session() {}

/// AC1: Slug generation deterministic under a test seed (for snapshot tests).
#[test]
#[ignore = "pending story 6-0d AC1: slug determinism under test seed"]
fn ac1_slug_determinism_under_seed() {}

/// AC1: Slug survives process restart via `SessionMeta.plan_slug`.
#[test]
#[ignore = "pending story 6-0d AC1: plan slug survives session reload"]
fn ac1_slug_survives_session_reload() {}

/// AC2: Turn 0 returns the full reminder inside `<plan-mode>...</plan-mode>`.
#[test]
#[ignore = "pending story 6-0d AC2: turn 0 returns full reminder"]
fn ac2_injector_turn_0_full_reminder() {}

/// AC2: Turn 5 returns the sparse reminder inside `<plan-mode-reminder>...</plan-mode-reminder>`.
#[test]
#[ignore = "pending story 6-0d AC2: turn 5 returns sparse reminder"]
fn ac2_injector_turn_5_sparse_reminder() {}

/// AC2: Turn 3 returns `None` (cadence gap).
#[test]
#[ignore = "pending story 6-0d AC2: turn 3 returns no reminder"]
fn ac2_injector_turn_3_no_reminder() {}

/// AC2: Re-entry reminder fires when the plan file already exists on disk.
#[test]
#[ignore = "pending story 6-0d AC2: re-entry reminder when plan file exists"]
fn ac2_reentry_reminder_on_existing_plan_file() {}

/// AC3: `ExitPlanMode` is callable only in `PermissionMode::Plan`; refused
/// in Normal/AutoEdit/YOLO.
#[test]
#[ignore = "pending story 6-0d AC3: ExitPlanMode only callable in Plan mode"]
fn ac3_exit_plan_mode_mode_gated() {}

/// AC3: Executing `ExitPlanMode` emits `AppEvent::PlanApprovalRequested` with
/// the expected fields.
#[test]
#[ignore = "pending story 6-0d AC3: ExitPlanMode emits PlanApprovalRequested"]
fn ac3_exit_plan_mode_emits_event() {}

/// AC4: `[y]` Approve transitions to Normal + injects synthetic user message.
#[test]
#[ignore = "pending story 6-0d AC4: [y] Approve transitions to Normal"]
fn ac4_approve_normal_transitions_mode_and_injects_synthetic() {}

/// AC4: `[a]` Approve & AutoEdit transitions to AutoEdit.
#[test]
#[ignore = "pending story 6-0d AC4: [a] Approve & AutoEdit transitions to AutoEdit"]
fn ac4_approve_autoedit_transitions_mode() {}

/// AC4: `[n]` Reject routes feedback to model; mode stays Plan.
#[test]
#[ignore = "pending story 6-0d AC4: [n] Reject routes feedback to model"]
fn ac4_reject_routes_feedback_and_stays_in_plan() {}

/// AC4: `[e]` Revise opens `$EDITOR` and re-renders on save.
#[test]
#[ignore = "pending story 6-0d AC4: [e] Revise opens editor and re-renders"]
fn ac4_revise_opens_editor_and_rerenders() {}

/// AC5: `/plan on` slash command activates Plan mode + warms up PlanManager.
#[test]
#[ignore = "pending story 6-0d AC5: /plan on activates Plan mode"]
fn ac5_slash_plan_on_activates() {}

/// AC5: Shift+Tab cycles Plan → Normal → AutoEdit → YOLO → Plan (wrap).
#[test]
#[ignore = "pending story 6-0d AC5: Shift+Tab cycles modes"]
fn ac5_shift_tab_cycle_order() {}

/// AC5: `default_plan_mode = true` in config starts session in Plan mode.
#[test]
#[ignore = "pending story 6-0d AC5: default_plan_mode config starts in Plan"]
fn ac5_default_plan_mode_config_respected() {}

/// AC6: `ToolRisk::Safe` tools pass through in Plan mode.
#[test]
#[ignore = "pending story 6-0d AC6: Safe tools pass in Plan mode"]
fn ac6_safe_tools_pass() {}

/// AC6: `exit_plan_mode` passes in Plan mode.
#[test]
#[ignore = "pending story 6-0d AC6: exit_plan_mode passes in Plan mode"]
fn ac6_exit_plan_mode_passes() {}

/// AC6: Write to plan-file path is the lone write exception in Plan mode.
#[test]
#[ignore = "pending story 6-0d AC6: write to plan-file path passes (exception)"]
fn ac6_plan_file_write_exception() {}

/// AC6: Standard/Elevated tools refused in Plan mode with a clear error
/// referring to Plan mode and instructing revise-or-exit.
#[test]
#[ignore = "pending story 6-0d AC6: Standard/Elevated tools refused with clear error"]
fn ac6_other_tools_refused_with_plan_mode_error() {}

/// AC7: `SandboxPolicy::from_mode(Plan, workspace)` is `ReadOnly { network: false }`.
#[test]
#[ignore = "pending story 6-0d AC7: SandboxPolicy::from_mode(Plan) is ReadOnly"]
fn ac7_sandbox_policy_plan_is_readonly_no_network() {}

/// AC8: The `<plan-mode>...</plan-mode>` envelope is hidden from the chat
/// view but transmitted to the LLM.
#[test]
#[ignore = "pending story 6-0d AC8: plan-mode envelope hidden from chat view"]
fn ac8_reminder_envelope_not_displayed() {}

/// AC8: Status bar shows `⟳ plan-reminder t+N` chip when a reminder is injected.
#[test]
#[ignore = "pending story 6-0d AC8: status bar shows plan-reminder chip"]
fn ac8_status_bar_reminder_indicator() {}

/// AC9: Synthetic user message carries a `synthetic: true` flag in conversation
/// metadata (for wire-log distinction).
#[test]
#[ignore = "pending story 6-0d AC9: synthetic user message marked as synthetic"]
fn ac9_synthetic_message_metadata() {}

/// AC9: Approval schedules the next turn automatically (user doesn't need to
/// press Enter).
#[test]
#[ignore = "pending story 6-0d AC9: next turn scheduled automatically"]
fn ac9_approval_triggers_next_turn_automatically() {}
