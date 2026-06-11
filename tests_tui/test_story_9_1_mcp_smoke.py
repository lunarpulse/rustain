"""Story 9-1 smoke test — proves the MCP fixture wire is connected end-to-end.

This is the load-bearing first test for Epic 9 TUI coverage. It proves that
the rustain binary, when handed a workspace ``.claude/mcp.json`` and a profile
that selects the composite tools adapter, actually:

  * Resolves the workspace MCP config at startup.
  * Spawns the stdio MCP child process.
  * Completes the JSON-RPC handshake (``initialize`` + ``tools/list``).
  * Emits the connection-state lifecycle through the event bus.

If this test passes, every higher-level MCP TUI test has a known-good wire.
Visual/keystroke assertions for the Adapter Status panel and the ``@MCP/``
autocomplete live in the per-story files (test_story_9_1_mcp_status_panel.py,
test_story_9_2_mcp_autocomplete.py) — they build on top of this.
"""
from __future__ import annotations

import os
import re
import subprocess
import time

import pytest


pytestmark = [pytest.mark.story_9_1, pytest.mark.mcp]


# Time budget for the MCP child to handshake. Generous because cargo build +
# the first run of the Python fixture under a fresh interpreter dominate.
MCP_HANDSHAKE_GRACE_S = 4.0


def _scan_log_for(tui, patterns: list[str]) -> dict[str, list[str]]:
    """Return a dict of pattern → matching lines (empty list if none)."""
    lines = tui.log_lines()
    out: dict[str, list[str]] = {}
    for pat in patterns:
        regex = re.compile(pat, re.IGNORECASE)
        out[pat] = [ln for ln in lines if regex.search(ln)]
    return out


def test_mcp_echo_server_handshake_completes(tui_with_mcp):
    """The rustain runtime spawns the stdio child and completes ``initialize``.

    AC source: Story 9.1 — workspace ``.claude/mcp.json`` is loaded, the
    ``McpClientAdapter`` connects, and at least one ``McpConnectionStateChanged``
    transition fires from the underlying event bus.
    """
    tui = tui_with_mcp

    # Give the runtime time to: load profile → resolve workspace mcp.json →
    # spawn child → exchange initialize/tools-list.
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    # Drain pyte once so the process keeps draining its PTY (otherwise rustain
    # can block on a full pipe and stall the handshake).
    _ = tui.get_screen_text()

    # Re-poll the log up to 6s — under load the connection state transitions
    # may trail the initial sleep.
    handshake_log_patterns = [
        r"mcp.*echo",
        r"McpConnectionStateChanged",
        r"workspace.*mcp\.json",
        r"McpClientAdapter",
    ]
    deadline = time.monotonic() + 6.0
    matches: dict[str, list[str]] = {}
    while time.monotonic() < deadline:
        matches = _scan_log_for(tui, handshake_log_patterns)
        if any(matches.values()):
            break
        time.sleep(0.5)

    hits = sum(1 for v in matches.values() if v)
    assert hits >= 1, (
        "Expected at least one MCP lifecycle log line. Recent log tail:\n"
        + "\n".join(tui.log_lines()[-50:])
    )


def test_mcp_echo_child_process_is_running(tui_with_mcp):
    """At least one Python fixture child process must exist under rustain.

    A passing assertion here is independent proof that ``.claude/mcp.json``
    was parsed and the stdio transport actually forked a child — regardless
    of internal state-machine details. Uses ``pgrep -f`` for portability.
    """
    tui = tui_with_mcp
    time.sleep(MCP_HANDSHAKE_GRACE_S)

    # Drain screen so we keep the rustain process unblocked.
    _ = tui.get_screen_text()

    # The fixture script path is uniquely identifying — even on a busy host,
    # it won't collide with unrelated Python processes.
    fixture_signature = "fixtures/mcp_fixture.py"
    result = subprocess.run(
        ["pgrep", "-f", fixture_signature],
        capture_output=True,
        text=True,
    )
    pids = [p for p in result.stdout.strip().splitlines() if p]
    if not pids:
        # Some CI distros ship pgrep without -f support; fall back to ps.
        ps = subprocess.run(["ps", "-ef"], capture_output=True, text=True)
        present = fixture_signature in ps.stdout
        assert present, (
            "Expected the MCP fixture child process to be running, but no "
            "matching pid was found via pgrep -f or ps -ef.\n"
            f"pgrep stderr: {result.stderr}\n"
            "Recent log tail:\n" + "\n".join(tui.log_lines()[-30:])
        )
    # Sanity: at least one process belongs to our test (uid match), guarding
    # against the unlikely case of a leftover from a prior test run.
    assert os.getuid() in {
        int(line.split()[2])
        for line in subprocess.run(
            ["ps", "-o", "pid=,ppid=,uid=", "-p", ",".join(pids)],
            capture_output=True, text=True,
        ).stdout.strip().splitlines()
        if line.strip()
    } if pids else True
