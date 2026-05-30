"""Story 10-4: Subagent Panel & Agent Inspector — TUI contract tests.

Deterministic (non-API) coverage for the subagent panel sidebar widget and its
chord dispatch. These mirror the Story 6-3 task-panel tests: they exercise the
``Ctrl+X, S`` chord, the empty-state render, panel toggle, and switching
between sidebar panels — all WITHOUT a live LLM (the registry is empty until a
subagent is spawned, which is exactly what the empty-state path renders).

The realism layer — a real LLM spawning a subagent via the ``task`` tool so the
panel shows a live row + the inspector renders — lives in
``test_story_10_x_subagent_llm_smoke.py`` (``@requires_api @slow``).

Chord reference (per Story 10-4): ``Ctrl+X, S`` → ``OpenPanel(PanelType::Agents)``.
Empty-state copy is pinned verbatim in epics.md / Story 10-4:
    "No agents running. Spawn one with rustain spawn --agent <name>
     or via plan delegation."
"""

from __future__ import annotations

import pytest

from harness import RustainTUI
from keys import CTRL_X


def _send_chord_ctrl_x_s(tui: RustainTUI) -> None:
    """Send Ctrl+X followed by 's' to trigger OpenPanel(Agents)."""
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("s")
    tui.wait(0.5)


# ── Scenario 1: Ctrl+X, S opens the Agents panel (empty state) ──────────────


@pytest.mark.story_10_4
def test_chord_opens_agents_panel(tui_monitor: RustainTUI):
    """Ctrl+X, S at the standard 130-col terminal opens the Agents sidebar."""
    tui = tui_monitor
    _send_chord_ctrl_x_s(tui)
    tui.assert_screen_contains("Agents", msg="Agents panel header should be visible")


# ── Scenario 2: Empty-state copy renders when no subagents are running ──────


@pytest.mark.story_10_4
def test_empty_state_copy(tui_monitor: RustainTUI):
    """With an empty registry the panel shows the pinned empty-state guidance.

    The sidebar is narrow, so the full sentence wraps/truncates; we assert the
    leading fragment that is guaranteed to land on a single line.
    """
    tui = tui_monitor
    _send_chord_ctrl_x_s(tui)
    screen = tui.get_screen_text()
    assert "No agents running" in screen, (
        "Expected the verbatim empty-state copy fragment 'No agents running' "
        f"(Story 10-4 / FR55). Screen:\n{screen}"
    )


# ── Scenario 3: Second Ctrl+X, S toggles the panel closed ───────────────────


@pytest.mark.story_10_4
def test_toggle_closes_agents_panel(tui_monitor: RustainTUI):
    """Pressing Ctrl+X, S a second time closes the Agents panel."""
    tui = tui_monitor
    _send_chord_ctrl_x_s(tui)
    tui.assert_screen_contains("Agents", msg="Panel should be open")

    _send_chord_ctrl_x_s(tui)
    tui.wait(1.0)
    tui.assert_screen_not_contains(
        "Agents", msg="Panel should close after second chord"
    )


# ── Scenario 4: Switching from History to Agents ────────────────────────────


@pytest.mark.story_10_4
def test_switch_from_history_to_agents(tui_monitor: RustainTUI):
    """Ctrl+X, S while the History panel is open switches to the Agents panel."""
    tui = tui_monitor
    tui.toggle_sidebar()
    tui.wait(0.5)
    tui.assert_screen_contains("History", msg="History panel should be open")

    _send_chord_ctrl_x_s(tui)
    tui.assert_screen_contains(
        "Agents", msg="Should have switched to the Agents panel"
    )
