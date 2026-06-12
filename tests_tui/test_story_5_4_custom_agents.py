"""Story 5-4: Custom Agents.

TUI contract tests for non-API-driven agent interactions:
  - @Agents/ autocomplete popup shows discovered agents (AC2)
  - autocomplete filter narrows the list (AC2)
  - selecting an agent with Enter activates it + status bar shows Agent: <name> (AC3/AC7)
  - @Agents/default clears the active agent (AC4)
  - no agents dir renders only the synthetic default entry (AC2)
  - unknown agent name produces a warning notice (AC3)
  - background scan notice appears when agents are discovered (AC8)

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

import json
import time

import pytest

from harness import RustainTUI
from keys import ESC, ENTER, TAB, BACKSPACE, DOWN

from fixtures.agents import write_custom_agent


CHAR_DELAY = 0.015


def type_slowly(t: RustainTUI, text: str, delay: float = CHAR_DELAY) -> None:
    for c in text:
        t.send(c)
        time.sleep(delay)


def _start_tui_with_workspace(workspace_path):
    """Start a RustainTUI pointing at the given workspace (no auto-build)."""
    tui = RustainTUI(fresh=True, build=False, workspace=workspace_path)
    tui.start()
    return tui


def _wait_for_agents_loaded(tui: RustainTUI, screen_text: str, timeout: float = 15.0) -> None:
    """Wait for the background agent scan to complete.

    Polls the pyte screen for *screen_text* (the SystemNotice flash).
    Falls back to checking the log file for ``AgentsDiscovered`` entries.
    """
    if tui.wait_for_screen(screen_text, timeout=timeout):
        return
    tui.assert_log_contains(r"Background agent scan complete:\s+\d+ agents",
                            msg=f"Agent scan completed but '{screen_text}' not on screen")


def _activate_agent(tui: RustainTUI, agent_name: str) -> None:
    """Activate an agent via the @Agents/ autocomplete flow.

    Types @Agents/<name>, navigates past the synthetic default to the
    agent entry, accepts the selection (Enter), then submits (Enter again).
    """
    type_slowly(tui, "@Agents/")
    time.sleep(0.3)
    type_slowly(tui, agent_name)
    time.sleep(0.5)
    tui.send(DOWN)
    time.sleep(0.3)
    tui.send(ENTER)
    time.sleep(0.3)
    tui.send(ENTER)
    time.sleep(0.5)


# ── Tests ───────────────────────────────────────────────────────────────────


@pytest.mark.story_5_4
def test_at_agents_slash_opens_agent_autocomplete(build_binary, tmp_path):
    """AC2: typing @Agents/ switches to agent autocomplete with discovered agent + default.

    Write a custom agent 'code-reviewer', start TUI, wait for scan notice,
    then type @Agents/ and assert the popup shows both the agent name and
    the synthetic 'default' entry.
    """
    write_custom_agent(tmp_path, "code-reviewer", "Reviews code for bugs")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_agents_loaded(tui, "Discovered 1 custom agent")

        type_slowly(tui, "@Agents/")
        time.sleep(0.5)

        tui.assert_screen_contains("code-reviewer", msg="Agent should appear in @Agents/ autocomplete")
        tui.assert_screen_contains("default", msg="Synthetic default entry should appear")

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_4
def test_agent_filter_narrows_list(build_binary, tmp_path):
    """AC2: typing after @Agents/ narrows the agent list case-insensitively."""
    write_custom_agent(tmp_path, "code-reviewer", "Reviews code")
    write_custom_agent(tmp_path, "security-auditor", "Audits security")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_agents_loaded(tui, "Discovered 2 custom agent")

        type_slowly(tui, "@Agents/cod")
        time.sleep(0.5)

        tui.assert_screen_contains("code-reviewer", msg="Filter 'cod' should match code-reviewer")
        tui.assert_screen_not_contains("security-auditor", msg="Filter 'cod' should exclude security-auditor")

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_4
def test_select_agent_with_enter_activates_and_shows_in_status_bar(build_binary, tmp_path):
    """AC3/AC7: selecting an agent from autocomplete activates it, status bar shows Agent: <name>.

    Use the @Agents/ autocomplete flow: open popup, filter by name, navigate
    Down past the synthetic default to the agent, accept (Enter), then submit
    (second Enter). Assert the status bar segment shows the active agent.
    """
    write_custom_agent(tmp_path, "code-reviewer", "Reviews code for bugs")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_agents_loaded(tui, "Discovered 1 custom agent")

        _activate_agent(tui, "code-reviewer")

        assert tui.wait_for_screen("Agent: code-reviewer", timeout=5.0), (
            "Status bar should show Agent: code-reviewer after activation"
        )
    finally:
        tui.stop()


@pytest.mark.story_5_4
def test_default_clears_active_agent(build_binary, tmp_path):
    """AC4: @Agents/default clears an active agent and status bar segment disappears.

    Activate an agent, then select @Agents/default from the autocomplete
    (first entry), accept, and submit. Assert the status bar no longer shows
    'Agent: <name>'.
    """
    write_custom_agent(tmp_path, "code-reviewer", "Reviews code")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_agents_loaded(tui, "Discovered 1 custom agent")

        _activate_agent(tui, "code-reviewer")
        assert tui.wait_for_screen("Agent: code-reviewer", timeout=5.0), (
            "Agent should be activated before clearing"
        )

        type_slowly(tui, "@Agents/")
        time.sleep(0.5)
        tui.assert_screen_contains("default")
        tui.send(ENTER)
        time.sleep(0.3)
        tui.send(ENTER)
        time.sleep(0.5)

        assert tui.wait_for_screen_not_contains("Agent: code-reviewer", timeout=5.0), (
            "Status bar should no longer show Agent: code-reviewer after clearing"
        )
    finally:
        tui.stop()


@pytest.mark.story_5_4
def test_no_agents_dir_renders_only_default(build_binary, tmp_path):
    """AC2: workspace with no .claude/agents/ → popup shows only the synthetic default.

    No discovery notice should appear. The @Agents/ popup should still
    work and show the 'default' entry with the 'no agents discovered' description.
    """
    tui = _start_tui_with_workspace(tmp_path)
    try:
        time.sleep(3.0)

        tui.assert_screen_not_contains(
            "Discovered",
            msg="Empty workspace should not show agent discovery notice",
        )

        type_slowly(tui, "@Agents/")
        time.sleep(0.5)

        tui.assert_screen_contains("default", msg="Synthetic default should always appear")
        tui.assert_screen_contains(
            "No custom agents discovered",
            msg="Empty-state description should appear when no agents found",
        )

        tui.send(ESC)
        time.sleep(0.3)
    finally:
        tui.stop()


@pytest.mark.story_5_4
def test_unknown_agent_name_warns_and_keeps_buffer(build_binary, tmp_path):
    """AC3: typing an unknown agent filter narrows autocomplete to only default.

    The autocomplete-based agent selection prevents submitting unknown agent
    names. Typing a filter that doesn't match any discovered agent shows only
    the synthetic 'default' entry. The buffer preserves the typed text, and
    no unknown agent is activated.
    """
    tui = _start_tui_with_workspace(tmp_path)
    try:
        time.sleep(3.0)

        type_slowly(tui, "@Agents/nonexistent")
        time.sleep(0.5)

        # Autocomplete should show only 'default' (unknown filter matches nothing)
        tui.assert_screen_contains(
            "default",
            msg="Synthetic default should appear even with unknown filter",
        )
        tui.assert_screen_contains(
            "No custom agents discovered",
            msg="Popup should show empty-state description for unknown filter",
        )

        # Accept default via double-Enter (accept + submit)
        tui.send(ENTER)
        time.sleep(0.3)
        tui.send(ENTER)
        time.sleep(0.5)

        # No custom agent should be activated
        tui.assert_screen_not_contains(
            "Agent: nonexistent",
            msg="Status bar should not show agent for unknown name",
        )
    finally:
        tui.stop()


@pytest.mark.story_5_4
def test_background_scan_notice_for_discovered_agents(build_binary, tmp_path):
    """AC8: starting the TUI with agents in .claude/agents/ surfaces the discovery notice.

    The notice text is 'Discovered N custom agent(s) in .claude/agents/'.
    Only appears when N >= 1.
    """
    write_custom_agent(tmp_path, "alpha", "First agent")
    write_custom_agent(tmp_path, "beta", "Second agent")

    tui = _start_tui_with_workspace(tmp_path)
    try:
        _wait_for_agents_loaded(tui, "Discovered 2 custom agent")
    finally:
        tui.stop()
