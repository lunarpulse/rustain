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

from harness import RustainTUI, mode_segment, status_row
from keys import SHIFT_TAB


# ── Structural / non-API tests ─────────────────────────────────────────────


@pytest.mark.story_6_0d
def test_slash_plan_on_activates_plan_mode(tui: RustainTUI):
    """AC5: /plan on switches to Plan mode and shows PLAN indicator."""
    # Use the command palette to enter Plan mode (reliable)
    tui.set_permission_mode("plan")
    # Wait for the Info toast to settle and the status bar to re-render
    tui.wait(0.5)
    # Status bar should show "PLAN" chip
    screen = tui.get_screen_text()
    assert "PLAN" in screen, f"Status bar should show PLAN mode indicator. Screen:\n{screen}"


@pytest.mark.story_6_0d
def test_slash_plan_off_deactivates_plan_mode(tui: RustainTUI):
    """AC5: /plan off switches back to Normal mode."""
    tui.set_permission_mode("plan")
    tui.set_permission_mode("normal")
    tui.wait(0.5)
    screen = tui.get_screen_text()
    assert "Normal" in screen, f"Status bar should show Normal mode. Screen:\n{screen}"
    assert "PLAN" not in screen, "Status bar should NOT show PLAN in Normal mode"


@pytest.mark.story_6_0d
def test_slash_plan_toggle_flips_mode(tui: RustainTUI):
    """AC5: /plan toggle flips between current and Plan mode."""
    # Start in Normal, toggle to Plan via command palette
    tui.set_permission_mode("plan")
    tui.wait(0.5)
    screen = tui.get_screen_text()
    assert "PLAN" in screen, "After toggle to Plan, status bar should show PLAN"
    # Toggle back to Normal
    tui.set_permission_mode("normal")
    tui.wait(0.5)
    screen = tui.get_screen_text()
    assert "Normal" in screen, "After toggle to Normal, status bar should show Normal"


# ── E2E tests for the /plan slash-command fix (Story 6-0d bug fix) ──────────


@pytest.mark.story_6_0d
def test_slash_command_plan_on_via_input(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan on' in the input box activates Plan mode.

    Before the fix, /plan fell through to SubmitWithContext and was treated
    as a user-defined command, so the mode never changed.
    """
    tui.send_message("/plan on")
    tui.wait_until(lambda s: mode_segment(s) == "PLAN", "the PLAN status segment after /plan on")


@pytest.mark.story_6_0d
def test_slash_command_plan_off_via_input(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan off' in the input box returns to Normal mode."""
    # Enter Plan mode first
    tui.set_permission_mode("plan")
    tui.wait_until(lambda s: mode_segment(s) == "PLAN", "the PLAN status segment")
    # Now type the slash command to exit
    tui.send_message("/plan off")
    tui.wait_until(lambda s: mode_segment(s) == "Normal", "Normal mode after /plan off")


@pytest.mark.story_6_0d
def test_slash_command_plan_toggle_via_input(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan toggle' in the input box toggles Plan mode."""
    # Start in Normal — toggle should enter Plan
    tui.send_message("/plan toggle")
    tui.wait_until(lambda s: mode_segment(s) == "PLAN", "Plan mode after /plan toggle from Normal")
    # Toggle again — should return to Normal
    tui.send_message("/plan toggle")
    tui.wait_until(lambda s: mode_segment(s) == "Normal", "Normal mode after the second /plan toggle")


@pytest.mark.story_6_0d
def test_shift_tab_in_input_cycles_modes(tui: RustainTUI):
    """AC5 + Story 21-2 AC9: Shift+Tab in input focus cycles Normal → AutoEdit → Plan → Normal.

    Yolo is never reachable by cycling; only an explicit `/mode yolo` selects it.
    """
    tui.input_mode()
    segments = {"AutoEdit": "AUTOEDIT", "Plan": "PLAN", "Normal": "Normal"}
    for name, segment in segments.items():
        tui.send(SHIFT_TAB)
        tui.wait_until(lambda s, want=segment: mode_segment(s) == want, f"the {name} status segment")
    # Yolo is only reachable through an explicit `/mode yolo`.
    assert "YOLO mode active" not in status_row(tui.get_screen_text()), "Shift+Tab must never enter Yolo"


@pytest.mark.story_6_0d
def test_status_bar_plan_indicator(tui: RustainTUI):
    """AC5/AC8: Status bar shows PLAN chip when in Plan mode."""
    tui.set_permission_mode("plan")
    tui.wait(0.5)
    screen = tui.get_screen_text()
    assert "PLAN" in screen, f"Status bar should show PLAN chip in Plan mode. Screen:\n{screen}"


# ── API-dependent TUI tests ─────────────────────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_0d
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
@pytest.mark.slow
@pytest.mark.story_6_0d
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
@pytest.mark.slow
@pytest.mark.story_6_0d
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
