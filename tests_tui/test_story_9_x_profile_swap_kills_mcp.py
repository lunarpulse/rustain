"""Story 9.x R5 — Profile swap actually kills the running MCP subprocess (TUI).

The Rust integration tests in `tests/integration_mcp_resilience.rs` prove that
`CompositeToolsetAdapter::prepare_detach()` kills a non-persistent MCP child.
This test drives the same code path through the TUI: open the profile-switcher
overlay (Ctrl+X, P), navigate to a profile without composite-MCP, confirm,
and verify via pgrep that the child died.

Risk closed:
  * R5 (TUI tier) — user-driven profile swap deterministically tears down
    non-persistent MCP processes; no zombies left behind.
"""
from __future__ import annotations

import subprocess
import time

import pytest

from keys import CTRL_X, ENTER, UP


pytestmark = [pytest.mark.story_9_4, pytest.mark.mcp]


MCP_FIXTURE_SIG = "fixtures/mcp_fixture.py"


def _pgrep_fixture() -> list[str]:
    out = subprocess.run(
        ["pgrep", "-f", MCP_FIXTURE_SIG], capture_output=True, text=True
    )
    return [p for p in out.stdout.strip().splitlines() if p]


def _open_profile_switcher(tui) -> None:
    tui.chat_mode()
    tui.send(CTRL_X)
    assert tui.wait_for_screen("Press a key", timeout=3.0), (
        f"Ctrl+X must open which-key. Screen:\n{tui.get_screen_text()}"
    )
    tui.send("p")
    # The switcher overlay renders a list of profiles; the active one is
    # marked. Wait until it's visible.
    assert tui.wait_for_screen("Profile", timeout=4.0), (
        f"Profile switcher overlay did not open. Screen:\n{tui.get_screen_text()}"
    )


def test_profile_swap_to_coding_kills_mcp_child(tui_with_mcp):
    """Pre-swap: echo MCP child running. Swap to `coding` profile (which uses
    the `builtin-full` tools adapter, no MCP). Post-swap: no fixture child
    remains.
    """
    tui = tui_with_mcp
    # Let the MCP handshake settle so prepare_detach actually has something
    # to disconnect.
    time.sleep(3.0)

    pre_pids = _pgrep_fixture()
    assert pre_pids, (
        "Pre-swap: at least one MCP fixture child must be running. "
        "Fixture wiring or handshake timing is broken."
    )

    _open_profile_switcher(tui)

    # The switcher lists profiles; the cursor starts on the active profile
    # (tests-mcp). Navigate up to a different profile. The embedded profiles
    # ship in alphabetical order: base, coding, personal-assistant. "coding"
    # is the default and the most stable swap target.
    #
    # We send Up arrows liberally — the switcher clamps at the top, so
    # extras are harmless and keep the test resilient to profile-list
    # additions.
    for _ in range(5):
        tui.send(UP)
        time.sleep(0.1)

    # Some switchers wrap the highlight to "coding" naturally; assert the
    # selected highlight matches before confirming. We just look for the
    # string "coding" in the visible region.
    assert tui.wait_for_screen("coding", timeout=3.0), (
        f"Profile 'coding' should appear in the switcher list. "
        f"Screen:\n{tui.get_screen_text()}"
    )

    # Enter opens the diff preview; press Enter again (or 'y') to confirm.
    tui.send(ENTER)
    time.sleep(0.5)
    # Confirm via 'y' which is the explicit accept binding for the preview.
    tui.send("y")

    # Profile swap is async: prepare_detach disconnects clients, then the
    # new composite is built. Give it generously.
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        if not _pgrep_fixture():
            break
        time.sleep(0.5)

    post_pids = _pgrep_fixture()
    assert not post_pids, (
        "Post-swap: no MCP fixture children should remain after swapping "
        f"to a non-MCP profile, but pgrep found pids={post_pids}.\n"
        f"Recent log tail:\n" + "\n".join(tui.log_lines()[-40:])
    )
