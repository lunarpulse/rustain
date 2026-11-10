"""21-17 native smoke: `/room role` and `/peer` rows leave at the next submission.

One leg, 80x24 (harness viewport patched as in 21-11), debug default-feature binary,
the harness's default isolated workspace. No model
turn is made. Every wait is structural (screen text), never a sleep.

1. `/room role list` -> the room-roles view is on screen;
2. `/peer list`      -> `Transport peers` visible and the room-roles view gone;
3. `/peer list`      -> `Transport peers` occurs once (no stacking);
4. `/ml`             -> a command that paints no class-V row: `Transport peers` gone.

With RUSTAIN_21_17_EVIDENCE=<dir> the screens after steps 1, 2 and 4 and a
result.json are written there.
"""
import json
import os
import re
from pathlib import Path

import harness
import pytest

from keys import ENTER


@pytest.fixture
def tui_80x24(monkeypatch, build_binary):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    instance = harness.RustainTUI(fresh=True, build=False)
    instance.start()
    try:
        yield instance
    finally:
        instance.stop()


ROOM = re.compile(r"[Rr]oom roles")
PEERS = "Transport peers"


def _submit(tui, command: str, popup_open: bool = False) -> None:
    """Type, wait until echoed, then Enter. A bare slash command leaves the
    autocomplete popup open: the first Enter accepts, the second submits."""
    tui.send(command)
    assert tui.wait_for_screen(command, timeout=10), tui.get_screen_text()
    tui.send_keys(ENTER, ENTER) if popup_open else tui.send_keys(ENTER)


def _wait(tui, predicate, what: str, timeout: float = 15.0) -> str:
    import time

    deadline = time.monotonic() + timeout
    screen = tui.get_screen_text()
    while time.monotonic() < deadline:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        # bounded poll of the pyte buffer (harness wait; the deadline above is the limit)
        tui.wait(0.1)
    raise AssertionError(f"{what}\nScreen:\n{screen}")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_room_and_peer_rows_leave_at_the_next_submission(tui_80x24):
    tui = tui_80x24
    evidence = os.environ.get("RUSTAIN_21_17_EVIDENCE")
    shots = {}

    _submit(tui, "/room role list")
    shots["after_room"] = _wait(tui, lambda s: ROOM.search(s) is not None, "room view missing")

    _submit(tui, "/peer list")
    shots["after_peer"] = _wait(
        tui,
        lambda s: PEERS in s and "oom roles" not in s,
        "peer view missing or room view still on screen",
    )

    _submit(tui, "/peer list")
    # The composer is cleared before dispatch: "/peer list" gone proves the second
    # command was consumed, so the one-row count is of ITS output, not the prior screen.
    shots["after_peer_again"] = _wait(
        tui,
        lambda s: s.count(PEERS) == 1 and "oom roles" not in s and "/peer list" not in s,
        "peer views stacked or the second /peer list not yet consumed",
    )

    _submit(tui, "/ml", popup_open=True)
    shots["after_ml"] = _wait(tui, lambda s: PEERS not in s, "peer view survived the next submission")

    if evidence:
        out = Path(evidence)
        out.mkdir(parents=True, exist_ok=True)
        for name in ("after_room", "after_peer", "after_ml"):
            (out / f"{name}.txt").write_text(shots[name], encoding="utf-8")
        (out / "result.json").write_text(
            json.dumps({"steps": ["room", "peer", "peer-again", "ml"], "passed": True}),
            encoding="utf-8",
        )
