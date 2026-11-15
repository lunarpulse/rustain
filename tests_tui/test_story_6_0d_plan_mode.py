"""Story 6-0d: Plan Mode Workflow.

Tests for the complete Plan mode workflow:
- Entry via slash command (/plan on|off|toggle) and Shift+Tab cycle
- Periodic reminder injection (invisible to user, visible to LLM)
- Plan file lifecycle (slug generation, session-stable path)
- exit_plan_mode tool → PlanApprovalCard widget
- Mode handoff with synthetic message on approval

Non-API tests (structural / no LLM calls):
  - Mode switching via slash command and Shift+Tab
  - Status bar PLAN indicator

API-dependent tests:
  - Full flow: enter Plan → agent Read (succeeds) → agent Write (denied)
    → agent ExitPlanMode → card renders → [y] approve → Normal mode
"""

from __future__ import annotations

import pytest

from harness import RustainTUI, mode_segment
from keys import SHIFT_TAB


# ── Structural / non-API tests ─────────────────────────────────────────────


def test_slash_command_plan_toggle_via_input(tui: RustainTUI):
    """Bug-fix E2E: ``/plan on|off|toggle`` typed in the input box moves the mode segment.

    Before the fix, /plan fell through to SubmitWithContext and was treated
    as a user-defined command, so the mode never changed. Every wait is a
    real transition of the status row's mode segment: not PLAN at spawn, then
    on → PLAN, off → Normal, toggle → PLAN, toggle → Normal.
    """
    assert mode_segment(tui.get_screen_text()) != "PLAN", "the TUI must not spawn in Plan mode"
    tui.send_message("/plan on")
    tui.wait_until(lambda s: mode_segment(s) == "PLAN", "the PLAN status segment after /plan on")
    tui.send_message("/plan off")
    tui.wait_until(lambda s: mode_segment(s) == "Normal", "Normal mode after /plan off")
    tui.send_message("/plan toggle")
    tui.wait_until(lambda s: mode_segment(s) == "PLAN", "Plan mode after /plan toggle from Normal")
    tui.send_message("/plan toggle")
    tui.wait_until(lambda s: mode_segment(s) == "Normal", "Normal mode after the second /plan toggle")


def test_shift_tab_in_input_cycles_modes(tui: RustainTUI):
    """AC5 + Story 21-2 AC9: Shift+Tab in input focus cycles Normal → AutoEdit → Plan → Normal;
    the command palette's ``mode:`` actions then move the same segment to Plan and back.

    Yolo is never reachable by cycling; only an explicit `/mode yolo` selects it.
    """
    tui.input_mode()
    segments = {"AutoEdit": "AUTOEDIT", "Plan": "PLAN", "Normal": "Normal"}
    for name, segment in segments.items():
        tui.send(SHIFT_TAB)
        tui.wait_until(lambda s, want=segment: mode_segment(s) == want, f"the {name} status segment")
    tui.set_permission_mode("plan")
    tui.wait_until(lambda s: mode_segment(s) == "PLAN", "the PLAN status segment after the palette action")
    tui.set_permission_mode("normal")
    tui.wait_until(lambda s: mode_segment(s) == "Normal", "Normal mode after the palette action")


# ── API-dependent TUI tests ─────────────────────────────────────────────────


@pytest.mark.requires_api
def test_plan_mode_read_succeeds_write_denied(tui_strict: RustainTUI):
    """AC6: Read passes in Plan mode; Write is denied with canonical message.

    The agent is asked to read a file, then write one.  In Plan mode:
    - Read should succeed (safe tool)
    - Write should be denied with the canonical Plan-mode error
    """
    tui_strict.set_permission_mode("plan")

    # Create a file to read
    (tui_strict.wp / "existing.txt").write_text("hello world")

    tui_strict.send_message("Read the file existing.txt")
    tui_strict.wait_for_idle()
    # Read should succeed — no permission prompt
    tui_strict.assert_screen_not_contains(
        "Plan mode is active",
        msg="Read should succeed in Plan mode",
    )

    tui_strict.send_message("Now write 'goodbye' to existing.txt")
    tui_strict.wait_for_screen(
        "Plan mode is active; you cannot modify state",
        timeout=15.0,
    )


@pytest.mark.requires_api
def test_exit_plan_mode_triggers_approval_card(tui_strict: RustainTUI):
    """AC3/AC4: Agent calls exit_plan_mode → PlanApprovalCard renders.

    In Plan mode, ask the agent to write a plan and then signal completion.
    We verify the card renders with double-border header and action keys.
    """
    tui_strict.set_permission_mode("plan")

    # Ask agent to write a plan and exit plan mode
    tui_strict.send_message(
        "Write a short plan to the plan file, then call exit_plan_mode."
    )

    # Wait for the approval card to appear
    tui_strict.wait_for_screen("Plan Approval", timeout=30.0)
    tui_strict.assert_screen_contains("[y] Approve")
    tui_strict.assert_screen_contains("[a] Approve & AutoEdit")
    tui_strict.assert_screen_contains("[n] Reject")
    tui_strict.assert_screen_contains("[e] Revise")


@pytest.mark.requires_api
def test_approve_plan_switches_to_normal_and_auto_fires(tui_strict: RustainTUI):
    """AC4/AC9: [y] approve → Normal mode + synthetic message + auto-trigger.

    After the approval card appears, press [y].  Verify:
    1. Mode switches to Normal
    2. A synthetic user message appears (⤷ marker)
    3. The agent executes the plan automatically
    """
    tui_strict.set_permission_mode("plan")

    tui_strict.send_message(
        "Write a short plan to the plan file, then call exit_plan_mode."
    )
    tui_strict.wait_for_screen("Plan Approval", timeout=30.0)

    # Approve
    tui_strict.send("y")
    tui_strict.wait_for_screen("Permission mode: Normal", timeout=5.0)

    # Synthetic message should appear
    tui_strict.assert_screen_contains("has been approved. Execute it.")

    # Agent should auto-execute
    tui_strict.wait_for_idle()
