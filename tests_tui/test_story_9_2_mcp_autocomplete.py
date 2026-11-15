"""Story 9-2 TUI E2E — ``@MCP/`` autocomplete dropdown.

Acceptance criteria (from ``docs/mcp.md`` and Story 9.2), proven by one launch:
  * Typing ``@MCP/`` in the input box opens the MCP autocomplete popup.
  * The popup lists ``[server] tool-name`` rows.
  * Filtering after the slash narrows the list.
  * Selecting a row inserts the canonical ``mcp__<server>__<tool>`` form.
  * Esc closes the popup without inserting anything.

The ``tui_with_mcp`` fixture's stdio echo MCP server exposes ``echo``, ``add``, ``slow_op``,
``error_op``, ``file_writer``, ``large_result`` and ``image_content``. The popup is populated
from the connected server's tool list, so the survivor starts by waiting for the Adapters frame to
show the server connected (``mcp_support.wait_mcp_ready``).
"""
from __future__ import annotations

import time

import pytest

from keys import BACKSPACE, ESC, TAB
from mcp_support import close_adapters, wait_mcp_ready


pytestmark = [pytest.mark.story_9_2, pytest.mark.mcp]


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


def test_mcp_autocomplete_selection_inserts_canonical_name(tui_with_mcp):
    """Popup opens and lists the echo tools; typing narrows it; Tab inserts the canonical
    ``mcp__echo__add``; Esc closes a reopened popup without inserting."""
    tui = tui_with_mcp
    wait_mcp_ready(tui)
    close_adapters(tui)

    # 1. ``@MCP/`` opens the popup with the two canonical entries (the other tools can overflow
    # the popup viewport on a 30-row terminal).
    _type_at_mcp(tui)
    tui.wait_until(
        lambda s: "MCP Tools" in s and "[echo] echo" in s and "[echo] add" in s,
        "the MCP popup listing '[echo] echo' and '[echo] add'",
    )

    # 2. Typing narrows the list: a transition from the screen that showed both rows.
    for ch in "add":
        tui.send(ch)
        time.sleep(0.05)
    tui.wait_until(
        lambda s: "[echo] add" in s and "[echo] echo" not in s,
        "the 'add' filter keeping '[echo] add' and hiding '[echo] echo'",
    )

    # 3. Tab inserts the canonical wire name.
    tui.send(TAB)
    tui.wait_until(lambda s: "mcp__echo__add" in s, "the canonical 'mcp__echo__add' in the input box")

    # 4. Esc closes a reopened popup and inserts nothing.
    _clear_input(tui)
    tui.wait_until(
        lambda s: "mcp__echo__add" not in s and "@MCP" not in s,
        "the input box emptied",
    )
    _type_at_mcp(tui)
    tui.wait_until(lambda s: "MCP Tools" in s, "the MCP popup reopened")
    tui.send(ESC)
    tui.wait_until(
        lambda s: "MCP Tools" not in s and "mcp__echo__" not in s,
        "the popup closed without inserting the canonical name",
        timeout=5.0,
    )
