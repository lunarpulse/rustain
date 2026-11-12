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

Offline first: RUSTAIN_TOUR_DESKCHECK=1 with RUSTAIN_TOUR_FRAMES=<dir> consumes only the frames
written by the ignored Rust test `story21_14_smoke_frames_export` (60x16, 80x24, 130x30, 130x30
with the monitor column); no binary or subprocess is started. Every predicate passes on the
intended frame and fails on torn controls. Predicates are anchored on whole rows; waits are
structural, never sleeps.
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

DESKCHECK = os.environ.get("RUSTAIN_TOUR_DESKCHECK") == "1"
SIZES = ((60, 16), (80, 24), (130, 30))
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


def tour_title(screen):
    rows = _rows(screen)
    top = _tour_top(rows)
    return None if top is None else re.search(r"┌(Tour[^─┐]*?) ?─*┐", rows[top]).group(1).strip()

def chrome_ok(screen):
    """The status row above the composer and, directly above it, the reserved strip row (the idle
    hint in every fixture): deleting or swapping either fails."""
    strip = harness.strip_row(screen)
    return "Normal" in harness.status_row(screen) and IDLE_HINT.search(strip) is not None



def card_ok(screen, title, rail):
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
    return body and rail_row and chrome_ok(screen)


LIVE_EMPTY = ("No agents active.",)
FRAME_EMPTY = ("No agents active.", "Nothing here yet.")  # the frame export has no roster read


def hub_ok(screen, empty=LIVE_EMPTY):
    rows = _rows(screen)
    top = _frame_top(rows, "Mission Control · Agents")
    return top is not None and _tour_top(rows) is None and chrome_ok(screen) \
        and any(text in row for row in rows for text in empty)


def help_ok(screen):
    rows = _rows(screen)
    return any(re.fullmatch(_LEFT + r"┌ ?Help — Rustain Keybindings ?─*┐" + _COL, row) for row in rows) \
        and _tour_top(rows) is None and chrome_ok(screen)


COMPLETE_NARROW = "Tour complete. Help is always one key away: Ctrl+X,…"  # a 60-column toast headline


def complete_ok(screen, text=COMPLETE, rows=1, monitor=False):
    """The Success toast on the toast rows the strip-aware harness derives (a bordered toast of
    100+ columns spans three rows; beside a monitor column the harness's row-end anchor cannot
    apply, so the headline is matched inside those rows only, never in Stage or monitor text)."""
    if monitor:
        toast = any(f"Success {text}" in row for row in harness.toast_rows(screen, rows))
    else:
        toast = harness.toast_present(screen, text, "Success", rows=rows)
    return _tour_top(_rows(screen)) is None and toast and chrome_ok(screen)


def idle_ok(screen):
    return _tour_top(_rows(screen)) is None and chrome_ok(screen)


def torn_controls(screen, title, rail):
    rows = _rows(screen)
    top = _frame_top(rows, title)
    assert top is not None
    status = harness.status_row(screen)
    controls = {
        "title missing": "\n".join(row for i, row in enumerate(rows) if i != top),
        "rail half painted": "\n".join(row.replace(rail, rail[: len(rail) // 2]) for row in rows),
        "status row missing": "\n".join(row for row in rows if row != status),
        "wrong title": "\n".join(row.replace(title, "Needs you (1)", 1) for row in rows),
    }
    strip = harness.strip_row(screen)
    if strip:
        swapped = [status if row == strip else strip if row == status else row for row in rows]
        controls["strip and status swapped"] = "\n".join(swapped)
    bottom = next(i for i in range(top + 1, len(rows)) if re.fullmatch(_LEFT + r"└─*┘" + _COL, rows[i]))
    controls["bottom border missing"] = "\n".join(row for i, row in enumerate(rows) if i != bottom)
    controls["strip deleted"] = "\n".join(row for row in rows if not strip or row != strip)
    return controls


def _frames(width, height, suffix=""):
    root = Path(os.environ["RUSTAIN_TOUR_FRAMES"])
    return {path.name.split("-")[0]: path.read_text(encoding="utf-8")
            for path in root.glob(f"*-{width}x{height}{suffix}.txt")}


def _deskcheck():
    for width, height, suffix in [(w, h, "") for w, h in SIZES] + [(130, 30, "-monitor")]:
        frames = _frames(width, height, suffix)
        label = f"{width}x{height}{suffix}"
        cards = {
            "offer": ("Tour", OFFER_RAIL),
            "step1": ("Tour · 1 of 5", STEP_RAIL),
            "step4": ("Tour · 4 of 5", STEP_RAIL),
            "step5": ("Tour · 5 of 5", STEP_RAIL),
        }
        for name, (title, rail) in cards.items():
            screen = frames[name]
            assert card_ok(screen, title, rail), f"{label} {name}:\n{screen}"
            assert tour_title(screen) == title
            for control, torn in torn_controls(screen, title, rail).items():
                assert not card_ok(torn, title, rail), f"{label} {name}: torn control `{control}` passed"
        assert "[n]" not in frames["offer"], f"{label}: n is not painted on the offer"
        assert hub_ok(frames["hub"], FRAME_EMPTY), f"{label} hub:\n{frames['hub']}"
        assert not hub_ok(frames["help"], FRAME_EMPTY) and not hub_ok(frames["idle"], FRAME_EMPTY)
        assert help_ok(frames["help"]), f"{label} help:\n{frames['help']}"
        text = COMPLETE_NARROW if width == 60 else COMPLETE
        kw = {"rows": 3 if width >= 100 else 1, "monitor": suffix == "-monitor"}
        assert complete_ok(frames["complete"], text, **kw), f"{label} complete:\n{frames['complete']}"
        assert idle_ok(frames["idle"]), f"{label} idle:\n{frames['idle']}"
        assert idle_ok(frames["armed4"]) and tour_title(frames["armed4"]) is None
        assert not complete_ok(frames["armed4"], text, **kw)
        # Torn controls of the other predicates.
        assert not hub_ok("\n".join(r for r in _rows(frames["hub"]) if "Nothing here yet." not in r), FRAME_EMPTY)
        assert not help_ok(frames["hub"]) and not idle_ok(frames["offer"])
        torn_toast = frames["complete"].replace("Tour complete.", "Tour comp")
        assert not complete_ok(torn_toast, text, **kw)
        assert not complete_ok("\n".join(r for r in _rows(frames["complete"]) if "Normal" not in r), text, **kw)
        # The reserved strip is required, and the toast is anchored to the toast rows.
        for name in ("idle", "hub", "help", "complete"):
            strip = harness.strip_row(frames[name])
            assert strip, f"{label} {name}: the strip row is painted"
            no_strip = "\n".join(r for r in _rows(frames[name]) if r != strip)
            assert not chrome_ok(no_strip), f"{label} {name}: strip deleted passed"
        moved = [r.replace(f"Success {text}", "") if "Success " in r else r for r in _rows(frames["complete"])]
        moved[2] = (moved[2] + f"  Success {text}").rstrip()
        assert not complete_ok("\n".join(moved), text, **kw), f"{label}: a toast moved into the Stage passed"


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Offline desk-check starts no binary; live runs use the harness resolution."""
    if not DESKCHECK:
        assert harness._resolve_binary()[0].is_file()


@pytest.mark.skipif(not DESKCHECK, reason="offline desk-check only")
def test_tour_predicates_accept_real_saved_frames_and_reject_torn_ones():
    _deskcheck()


def _settle_no_tour(tui, polls=30):
    """The launchpad is up and no tour title ever paints over `polls` screen reads (each read
    waits on the PTY, not on a timer)."""
    assert tui.wait_for_screen("Type a message to start.", timeout=15), tui.get_screen_text()
    for _ in range(polls):
        screen = tui.get_screen_text()
        assert _tour_top(_rows(screen)) is None, screen
    return tui.get_screen_text()


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
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
                        extra_args=OFFER_ARGS) as tui, \
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
