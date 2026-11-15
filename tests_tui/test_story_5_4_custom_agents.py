"""Story 5-4: Custom Agents.

TUI contract tests for non-API-driven agent interactions:
  - @Agents/ autocomplete popup shows discovered agents (AC2)
  - autocomplete filter narrows the list (AC2)
  - selecting an agent with Enter activates it + status bar shows Agent: <name> (AC3/AC7)
  - @Agents/default clears the active agent (AC4)
  - an unknown agent name never activates an agent (AC3)

All tests are non-API — they exercise the TUI lifecycle (startup, background
scan, autocomplete, status bar) without sending messages to an LLM. Agent
files are pre-populated in the temp workspace before the TUI starts.

Note on autocomplete interaction: when the autocomplete popup is active,
Enter/Tab accepts the *selected* suggestion (rewriting the buffer) but does
NOT submit the message. A second Enter is needed to actually submit. The
first suggestion is always the synthetic ``default`` entry, so navigating
with Down is required to select a real agent before accepting.
"""

from __future__ import annotations

import time

from harness import RustainTUI, composer_top, status_row
from keys import DOWN, ENTER

from fixtures.agents import write_custom_agent


CHAR_DELAY = 0.015


def type_slowly(t: RustainTUI, text: str, delay: float = CHAR_DELAY) -> None:
    for c in text:
        t.send(c)
        time.sleep(delay)


def _start_tui_with_workspace(workspace_path, isolated_home):
    """Start a RustainTUI pointing at the given workspace (no auto-build).

    ``isolated_home`` replaces ``$HOME`` so user-global agents in the developer's real home
    cannot add rows to the ``@Agents/`` popup.
    """
    tui = RustainTUI(fresh=True, build=False, workspace=workspace_path,
                     env_overrides={"HOME": str(isolated_home)})
    tui.start()
    return tui


def _popup(screen: str) -> str:
    """The rows above the composer box (the autocomplete popup and the chat), never the
    composer's own typed text."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[:top]) if top is not None else ""


def _composer_text(screen: str) -> str:
    """The composer box (its top border down to the screen's last row)."""
    lines = screen.split("\n")
    top = composer_top(lines)
    return "\n".join(lines[top:]) if top is not None else ""


def test_select_agent_with_enter_activates_and_shows_in_status_bar(build_binary, tmp_path):
    """AC2/AC3/AC4/AC7: one workspace, one launch; the whole ``@Agents/`` flow.

    1  ``@Agents/``           lists both agents.
    2  ``cod``                narrows to ``code-reviewer`` (``security-auditor`` leaves).
    3  the rest of the name, DOWN past the synthetic ``default``, Enter (accept), Enter (submit)
       activates the agent: the status row shows ``Agent: code-reviewer``.
    4  ``@Agents/`` + Enter, Enter on the first entry (``default``) clears the active agent.
    5  ``@Agents/nonexistent`` + Enter, Enter never activates an agent.

    The popup is read only above the composer box; the active agent only on the status row.
    """
    ws = tmp_path / "ws"
    home = tmp_path / "home"
    ws.mkdir()
    home.mkdir()
    write_custom_agent(ws, "code-reviewer", "Reviews code for bugs")
    write_custom_agent(ws, "security-auditor", "Audits security")

    tui = _start_tui_with_workspace(ws, home)
    try:
        assert "Agent:" not in status_row(tui.get_screen_text()), "no agent is active at spawn"

        # 1
        type_slowly(tui, "@Agents/")
        tui.wait_until(lambda s: "code-reviewer" in _popup(s) and "security-auditor" in _popup(s),
                       "both agents in the @Agents/ popup")
        # 2
        type_slowly(tui, "cod")
        tui.wait_until(lambda s: "code-reviewer" in _popup(s) and "security-auditor" not in _popup(s),
                       "the popup narrowed to code-reviewer")
        # 3
        type_slowly(tui, "e-reviewer")
        tui.wait_until(lambda s: "@Agents/code-reviewer" in _composer_text(s),
                       "the full agent name in the composer")
        tui.send_keys(DOWN, ENTER, ENTER)
        tui.wait_until(lambda s: "Agent: code-reviewer" in status_row(s),
                       "Agent: code-reviewer on the status row")
        # 4
        type_slowly(tui, "@Agents/")
        tui.wait_until(lambda s: "security-auditor" in _popup(s), "the unfiltered @Agents/ popup")
        tui.send_keys(ENTER, ENTER)
        tui.wait_until(lambda s: "Agent: code-reviewer" not in status_row(s),
                       "the active agent cleared by `default`")
        # 5
        type_slowly(tui, "@Agents/nonexistent")
        tui.wait_until(lambda s: "@Agents/nonexistent" in _composer_text(s),
                       "the unknown name in the composer")
        tui.send_keys(ENTER, ENTER)
        tui.wait_until(lambda s: "nonexistent" not in _composer_text(s),
                       "the unknown name submitted out of the composer")
        assert "Agent:" not in status_row(tui.get_screen_text()), "an unknown name never activates an agent"
    finally:
        tui.stop()
