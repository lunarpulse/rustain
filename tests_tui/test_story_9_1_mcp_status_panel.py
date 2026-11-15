"""Story 9-1 R7 — Adapter Status panel shows MCP server health (TUI E2E).

One launch of the ``tui_with_mcp`` fixture (workspace ``.claude/mcp.json`` -> the echo fixture,
profile ``tests-mcp`` with the composite tools adapter). The survivor proves the whole MCP wire
through the one place the user sees it: once the handshake completed, the Adapters frame holds
one row ``└─ <glyph> echo stdio … up <n>m · tools: <count>``, ``count`` being the fixture's full
tool list (``mcp_support.READY_ROW``, read row by row). A server that never loaded, a failed
handshake (``error`` / ``not connected`` rows) and an empty tool list each leave that row out.

Risk closed:
  * R7 — Adapter Status panel reflects live MCP connection state at the TUI
"""
from __future__ import annotations

import pytest

from mcp_support import adapters_frame_open, echo_row_ready, wait_mcp_ready


pytestmark = [pytest.mark.story_9_1, pytest.mark.mcp]


def test_adapter_status_panel_shows_echo_server(tui_with_mcp):
    """The Adapters frame shows the echo server as a connected stdio server with every tool."""
    screen = wait_mcp_ready(tui_with_mcp)
    assert adapters_frame_open(screen), f"The Adapters frame is not painted. Screen:\n{screen}"
    assert echo_row_ready(screen), (
        f"Expected one connected echo row with the fixture's tool count. Screen:\n{screen}"
    )
