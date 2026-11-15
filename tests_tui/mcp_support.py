"""Shared readiness wait of the MCP group (9_1 status panel, 9_2 autocomplete, 9_x profile swap).

The MCP child connects after the TUI is ready; the Adapters frame shows the echo server's
connection state as one row, ``└─ <glyph> echo stdio <pad> up <n>m · tools: <count>`` (the metric of
a connected server, ``mcp_server_state.rs``). ``wait_mcp_ready`` is the one wait for "the handshake
completed with the fixture's full tool list": it reads that row, never a log line and never a sleep.

Not ``harness.py`` / ``conftest.py``: those are receipt-bound.
"""
from __future__ import annotations

import re
import time

from fixtures.mcp_fixture import TOOLS
from keys import CTRL_X, ESC


# One screen row at a time (``echo_row_ready``): horizontal spaces only, so the server name, the
# transport and the metric cannot be stitched together from different rows, and a screen-wide
# match cannot run on to the panel's ``7 adapters loaded`` footer.
READY_ROW = re.compile(rf"└─ \S+ echo stdio *up \d+m · tools: {len(TOOLS)}\b")

_ADAPTERS_TITLE = re.compile(r"┌ ?Adapters ?─*┐")


def adapters_frame_open(screen: str) -> bool:
    """The Adapters frame's top border (its title) is painted."""
    return any(_ADAPTERS_TITLE.search(row) for row in screen.split("\n"))


def echo_row_ready(screen: str) -> bool:
    """A single screen row is the echo server's connected row with the fixture's full tool count."""
    return any(READY_ROW.search(row) for row in screen.split("\n"))


def open_adapters(tui, timeout: float = 10.0) -> str:
    """``Ctrl+X a``; return the screen once the Adapters frame is painted."""
    tui.send(CTRL_X + "a")
    return tui.wait_until(adapters_frame_open, "the Adapters frame", timeout)


def close_adapters(tui, timeout: float = 10.0) -> None:
    """``Esc`` (the frame is open: it goes back one frame); wait for the title to leave."""
    tui.send(ESC)
    tui.wait_until(lambda s: not adapters_frame_open(s), "the Adapters frame closed", timeout)


def wait_mcp_ready(tui, timeout: float = 20.0) -> str:
    """Open the Adapters frame until it shows the echo server connected with every fixture tool;
    return that screen (the frame stays open). The panel is a snapshot taken when it opens, so
    a row read too early is answered by closing and opening the frame again, bounded by
    ``timeout``; no sleep."""
    deadline = time.monotonic() + timeout
    while True:
        screen = open_adapters(tui)
        if echo_row_ready(screen):
            return screen
        assert time.monotonic() < deadline, (
            f"The echo MCP server never showed as connected with {len(TOOLS)} tools "
            f"within {timeout}s. Last Adapters frame:\n{screen}"
        )
        close_adapters(tui)
