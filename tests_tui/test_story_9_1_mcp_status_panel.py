"""Story 9-1 R7 — Adapter Status panel shows MCP server health (TUI E2E).

The ``tui_with_mcp`` fixture starts the TUI in Monitor density (set in the
test-only profile TOML). Monitor density is required because the default
Focus density forces sidebars hidden in `layout::compute_layout` even when
`state.sidebar_visible == true`. Tests target the **MCP wiring** here: when
the panel is visible, does it expose the running echo MCP server with its
transport and tool count?

Risk closed:
  * R7 — Adapter Status panel reflects live MCP connection state at the TUI
"""
from __future__ import annotations

import time

import pytest

from keys import CTRL_X


pytestmark = [pytest.mark.story_9_1, pytest.mark.mcp]


def _open_adapter_status(tui) -> None:
    """Open the Adapter Status panel via the Ctrl+X, A chord. Assumes the
    fixture has put the TUI in Monitor density so the sidebar renders.
    """
    tui.chat_mode()
    tui.send(CTRL_X)
    assert tui.wait_for_screen("Press a key", timeout=3.0), (
        f"Ctrl+X must open which-key. Screen:\n{tui.get_screen_text()}"
    )
    tui.send("a")
    time.sleep(0.5)


def test_adapter_status_panel_shows_echo_server(tui_with_mcp):
    """Open the Adapter Status panel and assert the echo MCP server row is
    visible with its name and transport. The fixture pre-configures Monitor
    density so the sidebar renders without additional chord-switching.
    """
    tui = tui_with_mcp
    # Wait for MCP handshake before opening the panel — the row's metric
    # comes from `composite.mcp_health_rows()` which reads the cached
    # tools after the initialize/tools-list round-trip.
    time.sleep(2.5)

    _open_adapter_status(tui)

    panel_opened = tui.wait_for_screen("Adapters", timeout=5.0)
    screen = tui.get_screen_text()
    assert panel_opened, (
        f"'Adapters' title not visible — panel did not render. Screen:\n{screen}"
    )

    # The MCP sub-row format is rendered by `adapter_status_panel::render`
    # as "└─ {symbol} {server_name} {transport}".
    assert "echo" in screen, (
        f"Expected MCP server 'echo' in the Adapter Status panel. Screen:\n{screen}"
    )
    assert "stdio" in screen, (
        f"Expected stdio transport indicator for echo. Screen:\n{screen}"
    )


def test_adapter_status_panel_health_row_after_handshake(tui_with_mcp):
    """Health-row metric must reflect a real tool count once the handshake
    has populated cached_tools. The fixture exposes seven tools — assert
    any non-zero count digit appears near the echo row.
    """
    tui = tui_with_mcp
    time.sleep(3.0)

    _open_adapter_status(tui)
    assert tui.wait_for_screen("Adapters", timeout=5.0)

    screen = tui.get_screen_text()
    import re
    has_count = bool(re.search(r"echo\b.*\b[1-9]\d?\b", screen, re.DOTALL))
    assert has_count, (
        f"Adapter row for 'echo' should include a non-zero tool count after "
        f"handshake. Screen:\n{screen}"
    )
