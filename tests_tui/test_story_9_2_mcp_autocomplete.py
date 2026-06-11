"""Story 9-2 TUI E2E — ``@MCP/`` autocomplete dropdown.

Acceptance criteria (from ``docs/mcp.md`` and Story 9.2):
  * Typing ``@MCP/`` in the input box opens the MCP autocomplete popup.
  * The popup is titled ``MCP Tools @`` and lists ``[server] tool-name`` rows.
  * Filtering after the slash narrows the list case-insensitively.
  * Selecting a row inserts the canonical ``mcp__<server>__<tool>`` form.

These tests build on the ``tui_with_mcp`` fixture from conftest.py — the
stdio echo MCP fixture exposes ``echo``, ``add``, ``slow_op``, ``error_op``,
``file_writer``, ``large_result``, and ``image_content``.
"""
from __future__ import annotations

import time

import pytest

from keys import BACKSPACE, ENTER, ESC, TAB


pytestmark = [pytest.mark.story_9_2, pytest.mark.mcp]


# Allow the MCP child to handshake and populate the autocomplete cache
# before we look. The connection event posts a CapabilityRegistry update
# asynchronously, so a small grace period is needed even after "Ready".
MCP_HANDSHAKE_GRACE_S = 3.0


def _type_at_mcp(tui) -> None:
    """Type the ``@MCP/`` trigger one character at a time.

    Char-by-char with a delay because raw-mode TUIs drop burst input from
    ``sendline()`` — same pattern as ``RustainTUI.send_message``.
    """
    for ch in "@MCP/":
        tui.send(ch)
        time.sleep(0.05)


def _clear_input(tui) -> None:
    """Drain whatever is in the input box back to empty (visual)."""
    # Generous Backspace count — bounded by Input buffer max.
    for _ in range(64):
        tui.send(BACKSPACE)
    time.sleep(0.2)


def test_at_mcp_slash_opens_mcp_autocomplete_popup(tui_with_mcp):
    """``@MCP/`` opens the MCP autocomplete popup with the expected title.

    AC-1: popup activation + title ``MCP Tools @``.
    """
    tui = tui_with_mcp
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    _type_at_mcp(tui)

    assert tui.wait_for_screen("MCP Tools", timeout=4.0), (
        "Expected MCP autocomplete popup to open after typing '@MCP/'.\n"
        f"Screen:\n{tui.get_screen_text()}"
    )


def test_mcp_autocomplete_lists_echo_server_tools(tui_with_mcp):
    """The popup shows echo server's tools as ``[echo] <tool>`` rows.

    AC-2: per-server grouping with canonical ``[server] tool`` rendering.
    """
    tui = tui_with_mcp
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    _type_at_mcp(tui)
    tui.wait_for_screen("MCP Tools", timeout=4.0)
    screen = tui.get_screen_text()

    # Fixture exposes echo + add as the two simplest read-only tools. We
    # don't assert all seven (slow_op, error_op, etc.) because long popup
    # entries can overflow the popup viewport on a 30-row terminal; the
    # two canonical entries are a sufficient AC-2 check.
    assert "[echo] echo" in screen, (
        f"Expected '[echo] echo' row in popup. Screen:\n{screen}"
    )
    assert "[echo] add" in screen, (
        f"Expected '[echo] add' row in popup. Screen:\n{screen}"
    )


def test_mcp_autocomplete_filters_by_typed_substring(tui_with_mcp):
    """Typing after ``@MCP/`` narrows the list case-insensitively.

    AC-3: filter substring matches tool name; non-matching tools are hidden.
    """
    tui = tui_with_mcp
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    _type_at_mcp(tui)
    tui.wait_for_screen("MCP Tools", timeout=4.0)

    # Narrow to "add" — should remove "echo" from the list.
    for ch in "add":
        tui.send(ch)
        time.sleep(0.05)
    time.sleep(0.5)
    screen = tui.get_screen_text()

    assert "[echo] add" in screen, (
        f"'add' filter should keep the 'add' tool visible. Screen:\n{screen}"
    )
    # 'echo' tool name should be filtered out (its description may still
    # match — keep the assertion specific to the row prefix).
    assert "[echo] echo" not in screen, (
        f"'add' filter should hide the 'echo' tool row. Screen:\n{screen}"
    )


def test_mcp_autocomplete_selection_inserts_canonical_name(tui_with_mcp):
    """Tab on a selected row inserts the canonical ``mcp__<server>__<tool>``.

    AC-4: dropdown selection writes the prefixed wire name into the input.
    """
    tui = tui_with_mcp
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    _type_at_mcp(tui)
    tui.wait_for_screen("MCP Tools", timeout=4.0)

    # Narrow to a single match so Tab is deterministic.
    for ch in "add":
        tui.send(ch)
        time.sleep(0.05)
    time.sleep(0.3)
    tui.send(TAB)
    time.sleep(0.5)

    screen = tui.get_screen_text()
    assert "mcp__echo__add" in screen, (
        "Expected canonical 'mcp__echo__add' in the input box after Tab. "
        f"Screen:\n{screen}"
    )


def test_mcp_autocomplete_esc_dismisses_without_inserting(tui_with_mcp):
    """Esc closes the popup without modifying the input.

    AC-5: Esc cancels — the input loses the ``@MCP/...`` filter scaffolding.
    """
    tui = tui_with_mcp
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    _type_at_mcp(tui)
    tui.wait_for_screen("MCP Tools", timeout=4.0)

    tui.send(ESC)
    # The popup title should disappear; allow one render tick.
    closed = tui.wait_for_screen_not_contains("MCP Tools", timeout=3.0)
    assert closed, (
        "Expected the MCP autocomplete popup to close after Esc.\n"
        f"Screen:\n{tui.get_screen_text()}"
    )
    # The canonical wire form must NOT have been inserted.
    assert "mcp__echo__" not in tui.get_screen_text(), (
        "Esc must not insert the canonical name. "
        f"Screen:\n{tui.get_screen_text()}"
    )
