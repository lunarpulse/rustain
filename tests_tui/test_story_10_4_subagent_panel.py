"""Story 10-4 (migrated by 21-7 slice 7-C): the Agents tab of Mission Control, TUI contract tests.

Deterministic (non-API) coverage of the ``Ctrl+X, S`` chord: since
21-7 the roster is the ``Mission Control · Agents`` tab (every width), not a sidebar panel, and
``Esc`` closes it. All WITHOUT a live LLM (the registry is empty until a subagent is spawned).

The realism layer — a real LLM spawning a subagent via the ``task`` tool so the tab shows a live
row and the agent window renders — lives in ``test_story_10_x_subagent_llm_smoke.py``
(``@requires_api @slow``).
"""

from __future__ import annotations

import pytest

from harness import RustainTUI
from keys import CTRL_X, ESC

TITLE = "Mission Control · Agents"


def _leader_menu_open(screen: str) -> bool:
    """The Ctrl+X leader menu: a box whose top border names the chord."""
    return any(line.strip().startswith("┌") and "Ctrl+X" in line for line in screen.split("\n"))


def _send_chord_ctrl_x_s(tui: RustainTUI) -> None:
    """Ctrl+X, wait for the leader menu, then 's': the hub opens on the Agents tab
    (the call returns once the frame title is on screen)."""
    tui.send(CTRL_X)
    tui.wait_until(_leader_menu_open, "the Ctrl+X leader menu")
    tui.send("s")
    assert tui.wait_for_screen(TITLE, timeout=5.0), tui.get_screen_text()


@pytest.mark.story_10_4
def test_chord_opens_agents_tab(tui_monitor: RustainTUI):
    """Ctrl+X, S opens Mission Control on the Agents tab; Esc closes the hub (a frame, not a
    toggled panel); the chord opens it again over the History panel."""
    tui = tui_monitor
    assert TITLE not in tui.get_screen_text(), "the Agents tab must be closed at spawn"
    _send_chord_ctrl_x_s(tui)

    tui.send(ESC)
    assert tui.wait_for_screen_not_contains(TITLE, timeout=5.0), tui.get_screen_text()

    tui.open_history()
    _send_chord_ctrl_x_s(tui)
