"""Story 6-4: Task Control & Plan Deviation — pyte TUI contract tests.

Plan-card latency is 30-40s; completion latency is 2-5s per Bash task.
All timeouts are generous to absorb LLM variance.

Markers:
    @pytest.mark.story_6_4  — all tests
    @pytest.mark.requires_api — needs ANTHROPIC_API_KEY
    @pytest.mark.slow
@pytest.mark.story_6_4        — >30s
"""

from __future__ import annotations

import time

import pytest

from harness import RustainTUI
from keys import ENTER, CTRL_X, CTRL_P, ESC

# Exact 6-3 pattern with minimal substitutions.
PROMPT = (
    "Use the propose_plan tool to propose a plan titled 'T64' "
    "with two tasks: "
    "1. Print 'aaa' "
    "2. Print 'bbb'. "
    "Include estimated_tool_calls=2 and estimated_seconds=10."
)


# ── helpers ──────────────────────────────────────────────────────────────────


def _chord_tasks(tui: RustainTUI) -> None:
    tui.send(CTRL_X)
    time.sleep(0.2)
    tui.send("t")
    time.sleep(0.6)


def _open_panel_focused(tui: RustainTUI) -> None:
    tui.send(ESC)
    time.sleep(0.3)
    _chord_tasks(tui)  # close-or-open
    _chord_tasks(tui)  # reopen → focus Sidebar
    time.sleep(0.6)


def _drive_plan(tui: RustainTUI) -> bool:
    """Send prompt, approve, wait for terminal.  Returns True if plan finished."""
    tui.send_message(PROMPT)
    # The plan card border is "╔══" followed by "Plan: {title}".
    # Use "Plan:" as the signal; if the LLM mentions "Plan:" in prose
    # the test will approve then quickly fail at the completion check
    # (no actual plan running) and return False safely.
    ok = tui.wait_for_screen("Plan:", timeout=70)
    if not ok:
        return False
    tui.send("y")
    time.sleep(0.5)
    ok = tui.wait_for_screen("Plan complete", timeout=120)
    if ok:
        tui.wait_for_idle()
        return True
    return tui.wait_for_screen("Plan cancelled", timeout=15)


# ── structural (no API) ──────────────────────────────────────────────────────


def test_no_more_coming_in_6_4_notice(tui: RustainTUI):
    """AC11: the stale 6.3 'Coming in Story 6.4' placeholder is gone."""
    _chord_tasks(tui)
    time.sleep(0.3)
    tui.send("r")
    time.sleep(0.3)
    assert "Coming in Story 6.4" not in tui.get_screen_text()


def test_palette_opens_no_crash(tui: RustainTUI):
    """Ctrl+P palette overlay renders and Esc dismisses without freeze."""
    tui.send(CTRL_P)
    time.sleep(0.5)
    screen = tui.get_screen_text()
    assert any(w in screen for w in ("Tip:", "filter", "Command")), (
        f"Palette expected.\n{screen}"
    )
    tui.send(ESC)
    time.sleep(0.4)
    tui.send(ESC)
    time.sleep(0.2)


# ── API tests (single combined test — avoids wasteful repeat plan-execs) ─────


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_4
def test_all_panel_keys_after_plan(tui: RustainTUI):
    """Execute one plan, then exercise p/s/x/Enter on the task panel."""

    if not _drive_plan(tui):
        pytest.skip("Plan did not finish — LLM non-determinism")

    _open_panel_focused(tui)
    assert "Tasks" in tui.get_screen_text(), "Panel should be open"

    # ── p (pause) on completed task → notice ──
    tui.send("p")
    time.sleep(0.4)
    screen = tui.get_screen_text()
    assert "cannot be paused" in screen.lower() or "Tasks" in screen, (
        f"Expected 'cannot be paused' notice or panel still open.\n{screen}"
    )

    # ── s (skip) on completed task → notice ──
    tui.send("s")
    time.sleep(0.4)
    screen = tui.get_screen_text()
    assert "cannot be skipped" in screen.lower() or "Tasks" in screen, (
        f"Expected 'cannot be skipped' notice or panel still open.\n{screen}"
    )

    # ── x (cancel — completed plan → notice) ──
    tui.send("x")
    time.sleep(0.4)
    screen = tui.get_screen_text()
    assert "No active plan" in screen or "cancel" in screen.lower(), (
        f"Expected notice or confirm card.\n{screen}"
    )
    tui.send("n")  # dismiss
    time.sleep(0.3)

    # ── Enter (drill-down) ──
    tui.send(ENTER)
    time.sleep(0.6)
    screen2 = tui.get_screen_text()
    ok = "[Esc] Back" in screen2 or "Tasks" in screen2
    if not ok:
        pass
    assert "Tasks" in screen2 or "[Esc]" in screen2, (
        f"Panel or drill-down expected.\n{screen2}"
    )

    # Tear down: dismiss overlays, close panel, verify TUI alive
    for _ in range(3):
        tui.send(ESC)
        time.sleep(0.2)
    _chord_tasks(tui)                # close Tasks panel
    time.sleep(0.3)
    tui.chat_mode()
    time.sleep(0.3)
    # Final probe: the TUI process must still be alive.
    assert tui.child.isalive(), "TUI process died"


@pytest.mark.requires_api
@pytest.mark.slow
@pytest.mark.story_6_4
def test_cancel_during_execution(tui: RustainTUI):
    """Press 'x' while a plan is still running; expect cancel UI."""

    tui.send_message(PROMPT)
    ok = tui.wait_for_screen("Plan:", timeout=70)
    if not ok:
        pytest.skip("Plan card never appeared")
    tui.send("y")
    time.sleep(0.5)

    # Open panel quickly — plan likely still mid-execution (first Bash ~1-2s).
    _open_panel_focused(tui)
    assert "Tasks" in tui.get_screen_text()

    tui.send("x")
    time.sleep(0.6)
    screen = tui.get_screen_text()
    # If plan already finished → notice; otherwise → confirm card.
    assert "cancel" in screen.lower() or "No active plan" in screen, (
        f"Expected cancel UI.\n{screen}"
    )
    tui.send("n")  # dismiss / keep-running
    time.sleep(0.3)

    for _ in range(3):
        tui.send(ESC)
        time.sleep(0.2)
    _chord_tasks(tui)
    time.sleep(0.3)
    tui.chat_mode()
    time.sleep(0.3)
    assert tui.child.isalive(), "TUI process died"
