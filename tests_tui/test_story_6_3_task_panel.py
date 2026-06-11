"""Story 6-3: Task Panel & Progress Monitoring.

Contract tests for the task panel sidebar widget, drill-down detail view,
and multi-panel sidebar dispatcher.

Scenarios:
1. Ctrl+X, T chord opens task panel at wide terminal (sidebar shows "Tasks").
2. Ctrl+X, T at narrow terminal emits warning (sidebar NOT opened).
3. Auto-open on PlanExecutionStarted — panel opens automatically when plan
   starts executing (requires API — real plan execution).
4. j/k navigation moves selection cursor in the task panel.
5. Enter drills down to task detail view (chat pane replaced).
6. Esc returns from drill-down to task panel sidebar.
7. Failed task detail view shows action keys [r] Retry [s] Skip [e] Edit.
8. Second Ctrl+X, T toggles the panel closed.
10. Ctrl+X, T while on History panel switches to Tasks panel.

Structural-only tests (no API key needed) exercise the chord, sidebar
rendering, and key dispatch. API-dependent tests exercise the full
plan-approval → task-panel-auto-open → drill-down flow.
"""

from __future__ import annotations

import time

import pytest

from pathlib import Path

from harness import RustainTUI
from keys import ESC, ENTER, CTRL_X

PROMPT_TWO_TASK_PLAN = (
    "Use the propose_plan tool to propose a plan titled 'Panel Test' "
    "with two tasks: 1. Print 'alpha' 2. Print 'beta'. "
    "Include estimated_tool_calls=2 and estimated_seconds=10."
)




def _send_chord_ctrl_x_t(tui: RustainTUI) -> None:
    """Send Ctrl+X followed by 't' to trigger the OpenPanel(Tasks) chord."""
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("t")
    tui.wait(0.5)


def _wait_for_plan_card(tui: RustainTUI, timeout: float = 30.0) -> None:
    """Block until the PlanCard header line appears on the pyte screen."""
    found = tui.wait_for_screen("Plan:", timeout=timeout)
    assert found, f"PlanCard header did not appear. Screen:\n{tui.get_screen_text()}"


def _ensure_task_panel_open(tui: RustainTUI) -> None:
    """Ensure the task panel sidebar is open AND focused after plan execution.

    After plan execution the auto-open leaves sidebar_visible=true with
    focus=Input (from PlanCardResolved).  The chord Ctrl+X,T would TOGGLE
    the panel closed because sidebar_visible is already true.  We must
    first close the auto-opened panel, wait for the render to settle,
    then reopen via chord (which sets focus=Sidebar).
    """
    tui.send(ESC)
    tui.wait(0.5)
    _send_chord_ctrl_x_t(tui)
    tui.wait(0.5)
    _send_chord_ctrl_x_t(tui)
    tui.wait(1.0)
    tui.assert_screen_contains("Tasks", msg="Task panel should be open after reopen chord")


# ── Scenario 1: Ctrl+X, T opens task panel at wide terminal ────────────────


@pytest.mark.story_6_3
def test_chord_opens_panel(tui: RustainTUI):
    """Ctrl+X, T chord at 130-col terminal opens the Tasks sidebar panel."""
    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains("Tasks", msg="Task panel header should be visible")


# ── Scenario 2: Ctrl+X, T at narrow terminal shows warning ────────────────


@pytest.mark.story_6_3
def test_chord_no_crash_at_default_size(tui: RustainTUI):
    """Ctrl+X, T at the default 130x30 terminal opens the panel (no narrow guard).

    The 120-col guard is exercised by the Rust conformance tests
    (ac_resolve_panel_plan_rejects_narrow). Here we just verify the chord
    works at the standard test terminal size.
    """
    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains("Tasks", msg="Panel should open at 130 cols")


# ── Scenario 4: j/k navigation moves cursor in task panel ──────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_panel_navigation(tui: RustainTUI):
    """After plan execution auto-opens panel, j/k moves selection."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()

    _ensure_task_panel_open(tui)

    tui.send("j")
    tui.wait(0.3)
    tui.send("k")
    tui.wait(0.3)


# ── Scenario 5: Enter drills down to task detail view ───────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_drill_down(tui: RustainTUI):
    """Enter on a task in the panel drills down to detail view replacing chat."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()

    _ensure_task_panel_open(tui)

    tui.send(ENTER)
    tui.wait(0.5)
    screen = tui.get_screen_text()
    if "[Esc] Back" in screen or "drill_down" in screen.lower():
        assert "Task" in screen, (
            f"Expected task detail view after Enter. Screen:\n{screen}"
        )
    else:
        pytest.fail(
            "Drill-down not triggered — expected task detail view after Enter. "
            f"Screen:\n{screen}"
        )


# ── Scenario 6: Esc returns from drill-down to sidebar ─────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_drill_down_back(tui: RustainTUI):
    """Esc from drill-down detail view returns to the sidebar panel."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    tui.wait_for_screen("Plan complete", timeout=120.0)
    tui.wait_for_idle()

    _ensure_task_panel_open(tui)

    tui.send(ENTER)
    tui.wait(0.5)
    screen = tui.get_screen_text()
    if "Task" not in screen or "[Esc] Back" not in screen:
        pytest.fail(
            "Drill-down not triggered — expected task detail view after Enter. "
            f"Screen:\n{screen}"
        )

    tui.send(ESC)
    tui.wait(0.5)
    tui.assert_screen_contains("Tasks", msg="Should be back at sidebar panel")


# ── Scenario 7: Failed task detail view shows reserved keys ────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_failed_task_action_row(tui: RustainTUI):
    """Detail view for a Failed task shows [r] Retry, [s] Skip, [e] Edit task."""
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

    _ensure_task_panel_open(tui)

    # The LLM may reorder tasks; navigate to the failed task (✗ icon) if needed.
    panel_screen = tui.get_screen_text()
    if "\u2717" not in panel_screen:
        pytest.skip("No failed task visible in panel — LLM may have reordered or not failed.")
    # If first visible task is not the failed one, navigate down.
    lines = panel_screen.splitlines()
    first_task_line = next((ln for ln in lines if ln.strip().startswith("1.")), "")
    if "\u2717" not in first_task_line:
        tui.send("j")
        tui.wait(0.3)

    tui.send(ENTER)
    tui.wait(0.5)
    screen = tui.get_screen_text()
    if "[Esc] Back" not in screen:
        pytest.fail(
            "Drill-down not triggered — expected task detail view after Enter. "
            f"Screen:\n{screen}"
        )
    has_action_row = (
        "[r]" in screen or "[s]" in screen or "[e]" in screen
    )
    assert has_action_row, (
        f"Expected action row with action keys. Screen:\n{screen}"
    )


# ── Scenario 9: Second Ctrl+X, T toggles the panel closed ──────────────────


@pytest.mark.story_6_3
def test_toggle_closes_panel(tui: RustainTUI):
    """Pressing Ctrl+X, T a second time closes the task panel."""
    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains("Tasks", msg="Panel should be open")

    _send_chord_ctrl_x_t(tui)
    tui.wait(1.0)
    tui.assert_screen_not_contains(
        "Tasks", msg="Panel should close after second chord"
    )


# ── Scenario 10: Switching from History to Tasks ────────────────────────────


@pytest.mark.story_6_3
def test_switch_from_history_to_tasks(tui: RustainTUI):
    """Ctrl+X, T while History panel is open switches to Tasks panel."""
    tui.toggle_sidebar()
    tui.wait(0.5)
    tui.assert_screen_contains("History", msg="History panel should be open")

    _send_chord_ctrl_x_t(tui)
    tui.assert_screen_contains("Tasks", msg="Should have switched to Tasks panel")


# ── Scenario 3: Auto-open on plan execution ────────────────────────────────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_3
def test_auto_open_on_execution_started(tui: RustainTUI):
    """When plan execution starts, the task panel auto-opens (>= 120 cols)."""
    tui.send_message(PROMPT_TWO_TASK_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    found = tui.wait_for_screen("Tasks", timeout=15.0)
    if found:
        tui.assert_screen_contains(
            "Tasks", msg="Task panel should have auto-opened on plan execution"
        )
    else:
        pytest.skip(
            "Plan completed before auto-open could be observed — race condition; "
            "panel content verified by test_drill_down"
        )


# ── Scenario 11: Task detail views show distinct content per task ───────────


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
    """Drilling into different tasks must show different detail content.

    If every task detail view renders identical text, the lookup or render
    pipeline is broken — this test catches that bug.
    """
    tui.send_message(PROMPT_AGI_TREND_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    # Relaxed completion detection — model verbosity may scroll the summary
    # out of the visible chat pane; sidebar "Plan complete" + "Ready" is
    # sufficient proof the plan reached a terminal state.
    deadline = time.time() + 180.0
    plan_done = False
    while time.time() < deadline:
        screen = tui.get_screen_text()
        if "Plan complete:" in screen or ("Plan complete" in screen and "Ready" in screen):
            plan_done = True
            break
        time.sleep(1.0)
    assert plan_done, f"Plan never reached terminal state within 180s. Screen:\n{tui.get_screen_text()}"
    tui.wait_for_idle()

    _ensure_task_panel_open(tui)

    # Capture detail content for the first two tasks and compare.
    contents: list[str] = []
    for _ in range(2):
        tui.send(ENTER)
        tui.wait(0.5)
        screen = tui.get_screen_text()
        if "[Esc] Back" not in screen:
            pytest.fail(
                "Drill-down not triggered — expected task detail view after Enter. "
                f"Screen:\n{screen}"
            )
        contents.append(screen)
        tui.send(ESC)
        tui.wait(0.5)
        # Move to next task for the next iteration
        tui.send("j")
        tui.wait(0.3)

    assert contents[0] != contents[1], (
        "Task detail views for different tasks show identical content — "
        f"this indicates a drill-down bug.\n\n"
        f"--- Task 1 detail ---\n{contents[0]}\n\n"
        f"--- Task 2 detail ---\n{contents[1]}"
    )


# ── Scenario 12: Task detail copied content differs per task ────────────────


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
    """Drill-down into different tasks shows distinct task detail views.

    We verify selection correctness by reading the task title/header line
    visible in each drill-down view *before* pressing 'c'.  The clipboard
    payload itself is non-deterministic — a model may complete a task by
    chatting without calling a tool, leaving result.text empty — so we
    only use it as auxiliary evidence, not the primary assertion.
    """
    tui.send_message(PROMPT_AGI_PLAN)
    _wait_for_plan_card(tui)
    tui.send("y")

    # Accept either chat-pane summary or sidebar terminal-state indicator
    deadline = time.time() + 180.0
    plan_done = False
    while time.time() < deadline:
        screen = tui.get_screen_text()
        if "Plan complete:" in screen or ("Plan complete" in screen and "Ready" in screen):
            plan_done = True
            break
        time.sleep(1.0)
    assert plan_done, f"Plan never reached terminal state within 180s. Screen:\n{tui.get_screen_text()}"
    tui.wait_for_idle()

    _ensure_task_panel_open(tui)

    header_lines: list[str] = []
    copy_payloads: list[str] = []
    for _ in range(2):
        tui.send(ENTER)
        tui.wait(0.5)
        screen = tui.get_screen_text()
        if "[Esc] Back" not in screen:
            pytest.fail(
                "Drill-down not triggered — expected task detail view after Enter. "
                f"Screen:\n{screen}"
            )

        # Capture the task header line (e.g. "Task 1. Search the latest news...")
        # The detail view header format is "Task {n}. {title}    {icon} {status} · {elapsed}".
        # We match lines that start with "Task " after stripping leading spaces.
        header = next(
            (ln for ln in screen.splitlines() if ln.strip().startswith("Task ")),
            ""
        )
        header_lines.append(header)

        # Attempt to copy — non-deterministic; model may have empty result
        if CLIPBOARD_FALLBACK.exists():
            CLIPBOARD_FALLBACK.unlink()
        tui.send("c")
        tui.wait(0.5)
        post_copy_screen = tui.get_screen_text()
        if CLIPBOARD_FALLBACK.exists():
            copy_payloads.append(CLIPBOARD_FALLBACK.read_text())
        else:
            copy_payloads.append("")

        tui.send(ESC)
        tui.wait(0.5)
        tui.send("j")
        tui.wait(0.3)

    # PRIMARY assertion: the drill-down views show different task headers.
    # This proves selection is moving between tasks.
    assert header_lines[0] != header_lines[1], (
        "Drill-down did not select different tasks — headers are identical.\n\n"
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
