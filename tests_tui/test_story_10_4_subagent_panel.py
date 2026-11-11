"""Story 10-4 (migrated by 21-7 slice 7-C): the Agents tab of Mission Control, TUI contract tests.

Deterministic (non-API) coverage of the ``Ctrl+X, S`` chord and the empty-state render: since
21-7 the roster is the ``Mission Control · Agents`` tab (every width), not a sidebar panel, and
``Esc`` closes it. All WITHOUT a live LLM (the registry is empty until a subagent is spawned,
which is exactly what the empty-state path renders).

The realism layer — a real LLM spawning a subagent via the ``task`` tool so the tab shows a live
row and the agent window renders — lives in ``test_story_10_x_subagent_llm_smoke.py``
(``@requires_api @slow``).

Empty-state copy: "No agents active." / "Ask the assistant to delegate or use plan mode."
"""

from __future__ import annotations

import pytest

from harness import RustainTUI
from keys import CTRL_X

TITLE = "Mission Control · Agents"


def _send_chord_ctrl_x_s(tui: RustainTUI) -> None:
    """Send Ctrl+X followed by 's': the hub opens on the Agents tab."""
    tui.send(CTRL_X)
    tui.wait(0.2)
    tui.send("s")
    tui.wait(0.5)


@pytest.mark.story_10_4
def test_chord_opens_agents_tab(tui_monitor: RustainTUI):
    """Ctrl+X, S opens Mission Control on the Agents tab."""
    tui = tui_monitor
    _send_chord_ctrl_x_s(tui)
    tui.assert_screen_contains(TITLE, msg="the Agents tab title should be visible")


@pytest.mark.story_10_4
def test_empty_state_copy(tui_monitor: RustainTUI):
    """With an empty registry the tab shows the pinned empty-state guidance."""
    tui = tui_monitor
    _send_chord_ctrl_x_s(tui)
    screen = tui.get_screen_text()
    assert "No agents active." in screen, (
        "Expected the empty-state copy fragment 'No agents active.' "
        f"(Story 10-4 / FR55). Screen:\n{screen}"
    )


@pytest.mark.story_10_4
def test_esc_closes_agents_tab(tui_monitor: RustainTUI):
    """Esc closes the hub (the tab is a frame, not a toggled panel)."""
    tui = tui_monitor
    _send_chord_ctrl_x_s(tui)
    tui.assert_screen_contains(TITLE, msg="Tab should be open")
    tui.send("\x1b")
    tui.wait(1.0)
    tui.assert_screen_not_contains(TITLE, msg="Tab should close on Esc")


@pytest.mark.story_10_4
def test_chord_over_the_history_panel_opens_the_agents_tab(tui_monitor: RustainTUI):
    """Ctrl+X, S while the History sidebar is open opens the hub over it."""
    tui = tui_monitor
    tui.open_history()
    tui.wait(0.5)
    tui.assert_screen_contains("History", msg="History panel should be open")
    _send_chord_ctrl_x_s(tui)
    tui.assert_screen_contains(TITLE, msg="Should have opened the Agents tab")
