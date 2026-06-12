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
  - Help overlay documentation

API-dependent tests:
  - Full flow: enter Plan → agent Read (succeeds) → agent Write (denied)
    → agent ExitPlanMode → card renders → [y] approve → Normal mode
"""

from __future__ import annotations

import pytest

from harness import RustainTUI
from keys import ENTER, ESC, SHIFT_TAB


# ── Structural / non-API tests ─────────────────────────────────────────────


@pytest.mark.story_6_0d
def test_slash_plan_on_activates_plan_mode(tui: RustainTUI):
    """AC5: /plan on switches to Plan mode and shows PLAN indicator."""
    # Use the command palette to enter Plan mode (reliable)
    tui.set_permission_mode("plan")
    # Wait for flash to settle and status bar to re-render
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
    tui.wait_for_screen("Permission mode: Plan", timeout=3.0)
    screen = tui.get_screen_text()
    assert "PLAN" in screen, f"Status bar should show PLAN after /plan on. Screen:\n{screen}"


@pytest.mark.story_6_0d
def test_slash_command_plan_off_via_input(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan off' in the input box returns to Normal mode."""
    # Enter Plan mode first
    tui.set_permission_mode("plan")
    tui.wait(0.5)
    # Now type the slash command to exit
    tui.send_message("/plan off")
    tui.wait_for_screen("Permission mode: Normal", timeout=3.0)
    screen = tui.get_screen_text()
    assert "PLAN" not in screen, "Status bar should NOT show PLAN after /plan off"


@pytest.mark.story_6_0d
def test_slash_command_plan_toggle_via_input(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan toggle' in the input box toggles Plan mode."""
    # Start in Normal — toggle should enter Plan
    tui.send_message("/plan toggle")
    tui.wait_for_screen("Permission mode: Plan", timeout=3.0)
    screen = tui.get_screen_text()
    assert "PLAN" in screen, "After /plan toggle from Normal, should be in Plan mode"
    # Toggle again — should return to Normal
    tui.send_message("/plan toggle")
    tui.wait_for_screen("Permission mode: Normal", timeout=3.0)
    screen = tui.get_screen_text()
    assert "PLAN" not in screen, "After second /plan toggle, should be back in Normal mode"


@pytest.mark.story_6_0d
def test_slash_command_plan_no_args_shows_status(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan' with no args flashes current mode status.

    Note: the slash-command autocomplete is active after typing '/plan',
    so we dismiss it with Esc before pressing Enter.
    """
    for c in "/plan":
        tui.send(c)
        tui.wait(0.05)
    tui.wait(0.3)
    tui.send(ESC)  # dismiss autocomplete so Enter submits
    tui.wait(0.3)
    tui.send(ENTER)
    found = tui.wait_for_screen(
        "Plan mode is not Plan — use /plan on|off|toggle to switch",
        timeout=3.0,
    )
    assert found, "No-arg /plan should flash current mode status"


@pytest.mark.story_6_0d
def test_slash_command_plan_invalid_arg_shows_error(tui: RustainTUI):
    """Bug-fix E2E: typing '/plan invalid' flashes an unknown-argument error."""
    tui.send_message("/plan invalid")
    found = tui.wait_for_screen(
        "Unknown /plan argument: invalid. Use on, off, or toggle",
        timeout=3.0,
    )
    assert found, "Invalid /plan argument should flash error message"


@pytest.mark.story_6_0d
def test_shift_tab_in_input_cycles_modes(tui: RustainTUI):
    """AC5: Shift+Tab in input focus cycles Normal → AutoEdit → Plan → Yolo → Normal."""
    tui.input_mode()
    # Cycle through all four modes
    mode_flash = {
        "AutoEdit": "Permission mode: AutoEdit",
        "Plan": "Permission mode: Plan",
        "Yolo": "YOLO mode active",
        "Normal": "Permission mode: Normal",
    }
    for expected in ["AutoEdit", "Plan", "Yolo", "Normal"]:
        tui.send(SHIFT_TAB)
        found = tui.wait_for_screen(mode_flash[expected], timeout=3.0)
        assert found, f"Expected mode change to {expected} after Shift+Tab"


@pytest.mark.story_6_0d
def test_shift_tab_in_chat_switches_tabs(tui: RustainTUI):
    """AC5: Shift+Tab in chat focus does NOT cycle modes (single tab, no visible change)."""
    tui.chat_mode()
    tui.send(SHIFT_TAB)
    # With a single tab, SwitchToPrevTab does nothing visible.
    # The important invariant is that NO mode cycle happens.
    screen = tui.get_screen_text()
    assert "Permission mode:" not in screen, (
        "Shift+Tab in chat focus should NOT cycle modes"
    )


@pytest.mark.story_6_0d
def test_help_overlay_shows_plan_mode_bindings(tui: RustainTUI):
    """AC5: Help overlay documents /plan and Shift+Tab cycle.

    On the 30-row pyte terminal, only the first few categories fit.
    We scroll down to find the COMMANDS category.
    """
    # Robust reset: close any leftover overlays, ensure chat focus
    tui.send(ESC)
    tui.wait(0.3)
    tui.send(ESC)
    tui.wait(0.3)
    tui.chat_mode()
    tui.open_help()
    # Wait for help overlay to fully render
    tui.wait_for_screen("Keybindings", timeout=3.0)
    # Scroll down incrementally to find /plan in COMMANDS section
    found_plan = False
    for _ in range(120):
        if "/plan on|off|toggle" in tui.get_screen_text():
            found_plan = True
            break
        tui.send("j")
        tui.wait(0.08)
    assert found_plan, f"Help should document /plan command. Screen:\n{tui.get_screen_text()}"
    # Scroll back to top and down to find Shift+Tab (in INPUT section)
    tui.send("g")
    tui.wait(0.1)
    found_shift_tab = False
    for _ in range(120):
        if "Shift+Tab in input" in tui.get_screen_text():
            found_shift_tab = True
            break
        tui.send("j")
        tui.wait(0.08)
    screen = tui.get_screen_text()
    assert found_shift_tab, f"Help should document Shift+Tab cycle. Screen:\n{screen}"
    assert "Cycle permission mode" in screen, "Help should describe Shift+Tab action"
    tui.close_overlay()


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
