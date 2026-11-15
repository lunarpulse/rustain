"""Story 21.14 native smoke: the onboarding tour. Generic lane (no marker that CI deselects): the
committed CI selector collects it like any generic test; only the developer's slice-evidence
run excluded it from its scrubbed whole-lane run (`--ignore`) because it was run by name, once per
live attempt (`one live execution` counts that evidence run, it is not a permanent exclusion); the
binary is resolved by the harness (RUSTAIN_TUI_BINARY, else the build). The live leg keeps the raw
PTY output of its first process under `RUSTAIN_CAPTURE_EVIDENCE_DIR/21-14-leg-a/raw.pty` with the
run identity (`run.json`: binary path and sha256, viewport).

Leg A (slice 14-A, 80x24): `-c onboarding.offer_tour=true` offers the tour on the first launch:
the frame whose title row reads `Tour` shows the offer rail (no `[n]`); `Enter` shows
`Tour · 1 of 5` with `[n] Skip step`; `n` three times shows `Tour · 4 of 5`; `Enter` arms the
step and the card goes; `Ctrl+X s` is the real action (`Mission Control · Agents` with its empty
roster row); `Esc` closes the hub and `Tour · 5 of 5` opens on the next due tick; `Enter` arms it;
`Ctrl+X ?` opens help; `Esc` leaves the `Tour complete.` toast (read with the strip-aware
`harness.toast_present`) and `<ws>/.rustain/state.toml` holds `tour_done = true` and
`session_count = 1`, owner-only. A second process on the same workspace shows the launchpad and
never a `Tour` title. The negative leg starts without the key: no `Tour` title, the strip row is
the idle hint. No message is sent to a provider (step 1 is skipped with `n`).

Predicates are anchored on whole rows; waits are structural, never sleeps.
"""
import os
import re
import stat
import tempfile
from pathlib import Path

import pytest

import harness
from harness import RustainTUI
from keys import CTRL_X, ENTER, ESC

OFFER_RAIL = "[Enter] Continue  [Esc] Skip tour"
STEP_RAIL = "[Enter] Continue  [n] Skip step  [Esc] Skip tour"
COMPLETE = "Tour complete. Help is always one key away: Ctrl+X, ?."
IDLE_HINT = re.compile(r"\^X s Mission Control")
_LEFT = r"(?:.*│\s*|\s*)"  # Stage text left of a centred card (>= 100 columns)
_COL = r"(?:\s{2,}\S.*)?"  # a monitor column painted right of the main column (>= 120 columns)
OFFER_ARGS = ["-c", "onboarding.offer_tour=true"]


def _rows(screen):
    return [row.rstrip() for row in screen.split("\n")]


def _frame_top(rows, title):
    """Index of the frame title row `┌<title> ──…┐` (the block paints the title right after the
    corner; a frame stacked over another one paints it with a space)."""
    pattern = re.compile(r"(?:│\s*|\s*)┌ ?" + re.escape(title) + r" ?─*┐" + _COL)
    for index, row in enumerate(rows):
        if pattern.fullmatch(row):
            return index
    return None


def _tour_top(rows):
    pattern = re.compile(_LEFT + r"┌Tour(?: · [1-5] of 5)? ?─*┐" + _COL)
    return next((i for i, row in enumerate(rows) if pattern.fullmatch(row)), None)


def chrome_ok(screen, mode="Normal", strip=True):
    """The status row above the composer and, directly above it, the reserved strip row (the idle
    hint in every fixture unless the strip yielded): deleting or swapping either fails."""
    row = harness.strip_row(screen)
    return mode in harness.status_row(screen) and ((IDLE_HINT.search(row) is not None) if strip else not row)



def card_ok(screen, title, rail, mode="Normal"):
    """The card: its title row, a body row, the rail as the last row inside the frame, the bottom
    border, the status row directly above the composer and the strip row (when painted) above
    that."""
    rows = _rows(screen)
    top = _frame_top(rows, title)
    if top is None or _tour_top(rows) != top:
        return False
    bottom = next((i for i in range(top + 1, len(rows)) if re.fullmatch(_LEFT + r"└─*┘" + _COL, rows[i])), None)
    if bottom is None or bottom < top + 3:
        return False
    body = any(re.fullmatch(r"\s*│\s*\S.*│\s*", row) for row in rows[top + 1:bottom - 1])
    rail_row = re.fullmatch(_LEFT + r"│" + re.escape(rail) + r"\s*│" + _COL, rows[bottom - 1]) is not None
    return body and rail_row and chrome_ok(screen, mode)


LIVE_EMPTY = ("No agents active.",)


def hub_ok(screen, empty=LIVE_EMPTY, mode="Normal"):
    rows = _rows(screen)
    top = _frame_top(rows, "Mission Control · Agents")
    return top is not None and _tour_top(rows) is None and chrome_ok(screen, mode) \
        and any(text in row for row in rows for text in empty)


def help_ok(screen, mode="Normal"):
    rows = _rows(screen)
    return any(re.fullmatch(_LEFT + r"┌ ?Help — Rustain Keybindings ?─*┐" + _COL, row) for row in rows) \
        and _tour_top(rows) is None and chrome_ok(screen, mode)


def complete_ok(screen, text=COMPLETE, rows=3, monitor=False, mode="Normal"):
    """The Success toast on the toast rows the strip-aware harness derives (a bordered toast of
    100+ columns spans three rows; beside a monitor column the harness's row-end anchor cannot
    apply, so the headline is matched inside those rows only, never in Stage or monitor text)."""
    if monitor:
        toast = any(f"Success {text}" in row for row in harness.toast_rows(screen, rows))
    else:
        toast = harness.toast_present(screen, text, "Success", rows=rows)
    return _tour_top(_rows(screen)) is None and toast and chrome_ok(screen, mode)


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Asserts the supplied/resolved binary exists; live runs use the harness resolution."""
    assert harness._resolve_binary()[0].is_file()


def _settle_no_tour(tui, polls=30):
    """The launchpad is up and no tour title ever paints over `polls` screen reads (each read
    waits on the PTY, not on a timer)."""
    assert tui.wait_for_screen("Type a message to start.", timeout=15), tui.get_screen_text()
    for _ in range(polls):
        screen = tui.get_screen_text()
        assert _tour_top(_rows(screen)) is None, screen
    return tui.get_screen_text()


def test_leg_a_tour_offer_steps_and_persistence_at_80x24(monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    with tempfile.TemporaryDirectory(prefix="21-14-a-") as root:
        root = Path(root)
        workspace = root / "workspace"
        workspace.mkdir()
        home = root / "home"
        home.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file"}
        evidence = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())) / "21-14-leg-a"
        evidence.mkdir(parents=True, exist_ok=True)
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env,
                        notification_ttl_secs=30, extra_args=OFFER_ARGS) as tui, \
                (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            offer = tui.wait_until(lambda s: card_ok(s, "Tour", OFFER_RAIL), "the offer card", timeout=15)
            assert "[n]" not in offer
            tui.send(ENTER)
            tui.wait_until(lambda s: card_ok(s, "Tour · 1 of 5", STEP_RAIL), "step 1 card", timeout=10)
            for number in (2, 3, 4):
                tui.send("n")
                tui.wait_until(lambda s, n=number: card_ok(s, f"Tour · {n} of 5", STEP_RAIL),
                               f"step {number} card", timeout=10)
            tui.send(ENTER)
            armed = tui.wait_until(lambda s: _tour_top(_rows(s)) is None, "the card closed (armed)", timeout=10)
            assert IDLE_HINT.search(harness.strip_row(armed)), armed
            tui.send(CTRL_X + "s")
            tui.wait_until(hub_ok, "the Agents tab with its empty roster row", timeout=15)
            tui.send(ESC)
            tui.wait_until(lambda s: card_ok(s, "Tour · 5 of 5", STEP_RAIL), "step 5 card after the hub", timeout=15)
            tui.send(ENTER)
            tui.wait_until(lambda s: _tour_top(_rows(s)) is None, "the card closed (armed)", timeout=10)
            tui.send(CTRL_X + "?")
            tui.wait_until(help_ok, "the help frame", timeout=10)
            tui.send(ESC)
            tui.wait_until(complete_ok, "the completion toast", timeout=10)
        state = workspace / ".rustain" / "state.toml"
        text = state.read_text(encoding="utf-8")
        assert re.search(r"^tour_done = true$", text, re.M), text
        assert re.search(r"^session_count = 1$", text, re.M), text
        assert stat.S_IMODE(state.stat().st_mode) == 0o600
        import hashlib
        import json
        binary = harness._resolve_binary()[0]
        (evidence / "run.json").write_text(json.dumps(
            {"viewport": [80, 24], "binary": str(binary),
             "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
             "state_toml": text}) + "\n", encoding="utf-8")
        # A second process on the same workspace never offers the tour again.
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env,
                        extra_args=OFFER_ARGS) as again:
            _settle_no_tour(again)
        # Negative leg: a default start has no offer and the strip row is the idle hint.
        with tempfile.TemporaryDirectory(prefix="21-14-a-neg-") as neg_root:
            with RustainTUI(fresh=True, build=False, workspace=Path(neg_root), env_overrides=env) as plain:
                screen = _settle_no_tour(plain)
                assert IDLE_HINT.search(harness.strip_row(screen)), screen
