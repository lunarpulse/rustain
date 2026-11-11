"""Story 6-3: Task Panel & Progress Monitoring (Story 21.7: Mission Control Tasks tab).

Contract tests for the plan's task list and the per-task detail view. Story 21.7
moved the old Tasks sidebar panel and the chat-pane drill-down into Mission
Control: ``Ctrl+X, t`` opens the hub on its Tasks tab (frame title
``Mission Control · Tasks``), ``Enter`` on a row opens the task window (frame
title ``Task <n> · <title>``) and ``Esc`` goes back one frame at a time. Nothing
opens by itself when a plan starts or fails (the auto-open is gone).

Scenarios:
1. Ctrl+X, t opens Mission Control on the Tasks tab (zero state when no plan).
2. The chord works at the default 130x30 terminal (no narrow guard, no crash).
3. Plan start does NOT auto-open a Tasks surface (requires API).
4. j/k navigation moves the selection in the Tasks tab (requires API).
5. Enter opens the task window ``Task <n> · <title>`` (requires API).
6. Esc returns from the task window to the Tasks tab (requires API).
7. Failed task window shows its action keys [r] Retry [s] Skip [e] Edit (requires API).
8. Esc closes the Tasks tab (replaces the old second-chord toggle).
10. Ctrl+X, t while the History sidebar is open opens the Tasks tab.
11. Task windows show distinct content per task (requires API).
12. Task windows show distinct headers / copy payloads per task (requires API).

Structural-only tests (no API key needed) exercise the chord and the tab
rendering. API-dependent tests exercise the full plan-approval -> Tasks tab ->
task window flow; they cannot run in the generic lane and need live desk-checking.
"""

from __future__ import annotations

import re
import time

import pytest

from pathlib import Path

from harness import RustainTUI
from keys import ESC, ENTER, CTRL_X

HUB_TASKS = "Mission Control \u00b7 Tasks"
# The task window frame title: ``Task <n> · <title>``.
TASK_WINDOW_TITLE = re.compile(r"Task \d+ \u00b7 \S")

PROMPT_TWO_TASK_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'Panel Test' "
    "with two tasks: 1. Print 'alpha' 2. Print 'beta'. "
    "Include estimated_tool_calls=2 and estimated_seconds=10."
)


def _send_chord_ctrl_x_t(tui: RustainTUI) -> None:
    """Send Ctrl+X followed by 't' to open Mission Control on its Tasks tab."""
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("t")
    tui.wait(0.5)


def _wait_for_plan_card(tui: RustainTUI, timeout: float = 30.0) -> None:
    """Block until the PlanCard header line appears on the pyte screen."""
    found = tui.wait_for_screen("Plan:", timeout=timeout)
    assert found, f"PlanCard header did not appear. Screen:\n{tui.get_screen_text()}"


def _open_tasks_tab(tui: RustainTUI) -> None:
    """Open the Tasks tab with the chord and assert the hub frame is on top.

    Nothing is open after a plan finishes (no auto-open), so a single chord
    opens the hub; there is no toggle to undo first.
    """
    _send_chord_ctrl_x_t(tui)
    tui.wait(1.0)
    tui.assert_screen_contains(HUB_TASKS, msg="Tasks tab should be open after the chord")


def _open_task_window(tui: RustainTUI) -> str:
    """Enter on the selected row; return the screen once the task window is up."""
    tui.send(ENTER)
    tui.wait(0.5)
    screen = tui.get_screen_text()
    if not TASK_WINDOW_TITLE.search(screen):
        pytest.fail(
            "Task window not opened — expected a 'Task <n> · <title>' frame after Enter. "
            f"Screen:\n{screen}"
        )
    return screen


def _plan_reached_terminal_state(tui: RustainTUI, timeout: float = 180.0) -> bool:
    """Relaxed completion detection — model verbosity may scroll the summary."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        screen = tui.get_screen_text()
        if "Plan complete:" in screen or ("Plan complete" in screen and "Ready" in screen):
            return True
        time.sleep(1.0)
    return False


# ── Scenario 1: Ctrl+X, t opens the Tasks tab ──────────────────────────────


@pytest.mark.story_6_3
def test_chord_opens_panel(tui_monitor: RustainTUI):
    """Ctrl+X, t opens Mission Control on its Tasks tab (zero state: no plan)."""
    tui = tui_monitor
    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains(HUB_TASKS, msg="Tasks tab frame title should be visible")
    tui.assert_screen_contains("No active plan", msg="Zero state expected with no plan")


# ── Scenario 2: the chord at the default terminal size ─────────────────────


def test_chord_no_crash_at_default_size(tui_monitor: RustainTUI):
    """Ctrl+X, t at the default 130x30 terminal opens the tab (no narrow guard).

    The row/rail behaviour at narrow widths is exercised by the Rust conformance
    tests; here we just verify the chord works at the standard test terminal size.
    """
    tui = tui_monitor
    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains(HUB_TASKS, msg="Tab should open at 130 cols")
    assert tui.child.isalive(), "TUI process died"


# ── Scenario 4: j/k navigation moves cursor in the Tasks tab ───────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_panel_navigation(tui: RustainTUI):
    """After plan execution, the Tasks tab lists the tasks and j/k moves selection."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()

    _open_tasks_tab(tui)

    tui.send("j")
    tui.wait(0.3)
    tui.send("k")
    tui.wait(0.3)
    tui.assert_screen_contains(HUB_TASKS, msg="Tab should still be on top after j/k")


# ── Scenario 5: Enter opens the task window ─────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_drill_down(tui: RustainTUI):
    """Enter on a task in the Tasks tab opens the ``Task <n> · <title>`` window."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()

    _open_tasks_tab(tui)

    _open_task_window(tui)


# ── Scenario 6: Esc returns from the task window to the tab ────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_drill_down_back(tui: RustainTUI):
    """Esc from the task window returns to the Tasks tab."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()

    _open_tasks_tab(tui)
    _open_task_window(tui)

    tui.send(ESC)
    tui.wait(0.5)
    tui.assert_screen_contains(HUB_TASKS, msg="Should be back at the Tasks tab")


# ── Scenario 7: Failed task window shows its action keys ───────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_failed_task_action_row(tui: RustainTUI):
    """Window for a Failed task shows [r] Retry, [s] Skip, [e] Edit."""
    prompt = (
        "Use the propose_plan tool to propose a plan titled 'Fail Panel Test' "
        "with two tasks: 1. Read the file /nonexistent/path/fail.txt "
        "2. Echo 'after failure'. "
        "Include estimated_tool_calls=2 and estimated_seconds=10."
    )
    tui.send_message(prompt)
    _wait_for_plan_card(tui)
    tui.send("y")

    found = tui.wait_for_screen("Plan complete", timeout=180.0)
    if not found:
        found = tui.wait_for_screen("Plan cancelled", timeout=10.0)
    if not found:
        pytest.skip("Plan neither completed nor cancelled in time — LLM variability")

    tui.wait_for_idle()

    _open_tasks_tab(tui)

    # The LLM may reorder tasks; navigate to the failed task (✗ icon) if needed.
    panel_screen = tui.get_screen_text()
    if "\u2717" not in panel_screen:
        pytest.skip("No failed task visible in tab — LLM may have reordered or not failed.")
    # If the first task row is not the failed one, move down to the next row.
    lines = panel_screen.splitlines()
    first_task_line = next((ln for ln in lines if "1. " in ln and ("\u2717" in ln or "\u2713" in ln)), "")
    if "\u2717" not in first_task_line:
        tui.send("j")
        tui.wait(0.3)

    screen = _open_task_window(tui)
    has_action_row = (
        "[r]" in screen or "[s]" in screen or "[e]" in screen
    )
    assert has_action_row, (
        f"Expected action row with action keys. Screen:\n{screen}"
    )


# ── Scenario 8 (was 9): Esc closes the Tasks tab ───────────────────────────


@pytest.mark.story_6_3
def test_toggle_closes_panel(tui_monitor: RustainTUI):
    """Esc closes the Tasks tab (the second-chord toggle of the sidebar is gone)."""
    tui = tui_monitor
    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains(HUB_TASKS, msg="Tab should be open")

    tui.send(ESC)
    tui.wait(1.0)
    tui.assert_screen_not_contains(
        "Mission Control", msg="Tab should close on Esc"
    )


# ── Scenario 10: History sidebar open, then Tasks ───────────────────────────


@pytest.mark.story_6_3
def test_switch_from_history_to_tasks(tui_monitor: RustainTUI):
    """Ctrl+X, t while the History panel is open opens the Tasks tab."""
    tui = tui_monitor
    tui.open_history()
    tui.wait(0.5)
    tui.assert_screen_contains("History", msg="History panel should be open")

    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains(HUB_TASKS, msg="Should have opened the Tasks tab")


# ── Scenario 3: No auto-open on plan execution ─────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_auto_open_on_execution_started(tui: RustainTUI):
    """Story 21.7: plan execution start opens NO Tasks surface (auto-open deleted)."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    # Watch the first seconds of execution: the chat stays on top throughout.
    deadline = time.time() + 8.0
    while time.time() < deadline:
        tui.assert_screen_not_contains(
            "Mission Control", msg="Plan start must not open Mission Control"
        )
        time.sleep(0.5)

    # The tab is one chord away and lists the plan's tasks.
    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()
    _open_tasks_tab(tui)


# ── Scenario 11: Task windows show distinct content per task ────────────────


PROMPT_AGI_TREND_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'AGI Trend Search' "
    "with three tasks: 1. Search the latest news about AGI developments in 2025 "
    "2. Summarize key milestones from major AI labs "
    "3. List potential risks and opportunities. "
    "Include estimated_tool_calls=3 and estimated_seconds=15."
)


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_task_detail_content_varies_per_task(tui: RustainTUI):
    """Opening different tasks must show different window content.

    If every task window renders identical text, the lookup or render
    pipeline is broken — this test catches that bug.
    """
    tui.send_message(PROMPT_AGI_TREND_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    plan_done = _plan_reached_terminal_state(tui)
    assert plan_done, f"Plan never reached terminal state within 180s. Screen:\n{tui.get_screen_text()}"
    tui.wait_for_idle()

    _open_tasks_tab(tui)

    # Capture window content for the first two tasks and compare.
    contents: list[str] = []
    for _ in range(2):
        contents.append(_open_task_window(tui))
        tui.send(ESC)
        tui.wait(0.5)
        # Move to next task for the next iteration
        tui.send("j")
        tui.wait(0.3)

    assert contents[0] != contents[1], (
        "Task windows for different tasks show identical content — "
        f"this indicates a task-window bug.\n\n"
        f"--- Task 1 window ---\n{contents[0]}\n\n"
        f"--- Task 2 window ---\n{contents[1]}"
    )


# ── Scenario 12: Task window copied content differs per task ────────────────


PROMPT_AGI_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'AGI Trend Search' "
    "with two tasks: 1. Search the latest news about AGI developments in 2025 "
    "2. Summarize key milestones from major AI labs. "
    "Include estimated_tool_calls=2 and estimated_seconds=10."
)

CLIPBOARD_FALLBACK = Path.home() / ".rustain" / "clipboard.txt"


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_task_detail_copy_result_differs_per_task(tui: RustainTUI):
    """Task windows for different tasks show distinct headers (and copy payloads).

    We verify selection correctness by reading the ``Task <n> · <title>`` frame
    title of each window *before* pressing 'c'.  The clipboard payload itself is
    non-deterministic — a model may complete a task by chatting without calling
    a tool, leaving result.text empty — so we only use it as auxiliary evidence,
    not the primary assertion.
    """
    tui.send_message(PROMPT_AGI_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    plan_done = _plan_reached_terminal_state(tui)
    assert plan_done, f"Plan never reached terminal state within 180s. Screen:\n{tui.get_screen_text()}"
    tui.wait_for_idle()

    _open_tasks_tab(tui)

    header_lines: list[str] = []
    copy_payloads: list[str] = []
    screen = ""
    for _ in range(2):
        screen = _open_task_window(tui)

        # Capture the window's frame title line (e.g. "Task 1 · Search the latest news...").
        header = next(
            (ln.strip() for ln in screen.splitlines() if TASK_WINDOW_TITLE.search(ln)),
            ""
        )
        header_lines.append(header)

        # Attempt to copy — non-deterministic; model may have empty result
        if CLIPBOARD_FALLBACK.exists():
            CLIPBOARD_FALLBACK.unlink()
        tui.send("c")
        tui.wait(0.5)
        if CLIPBOARD_FALLBACK.exists():
            copy_payloads.append(CLIPBOARD_FALLBACK.read_text())
        else:
            copy_payloads.append("")

        tui.send(ESC)
        tui.wait(0.5)
        tui.send("j")
        tui.wait(0.3)

    # PRIMARY assertion: the windows show different task headers.
    # This proves selection is moving between tasks.
    assert header_lines[0] != header_lines[1], (
        "Task windows did not select different tasks — headers are identical.\n\n"
        f"--- Task 1 header ---\n{header_lines[0]}\n\n"
        f"--- Task 2 header ---\n{header_lines[1]}\n\n"
        f"--- Full screen dump (last iteration) ---\n{screen}"
    )

    # SECONDARY (best-effort): if both tasks produced copyable content, it
    # should differ.  We do NOT fail when the clipboard is empty because that
    # is legitimate LLM behavior (no tool call → no stored result).
    if copy_payloads[0] and copy_payloads[1]:
        assert copy_payloads[0] != copy_payloads[1], (
            "Copy payload identical for different tasks.\n\n"
            f"--- Task 1 payload ---\n{copy_payloads[0]}\n\n"
            f"--- Task 2 payload ---\n{copy_payloads[1]}"
        )
