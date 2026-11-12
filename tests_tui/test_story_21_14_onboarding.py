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

Leg B (slice 14-B, 80x24): `-c onboarding.team=true` starts the session in Plan mode (the status
row says `PLAN`) and offers the tour. While a step is armed the top row of the chat pane is the
coach line (`Tour <k>/5 · <instruction> · /tour`); the target of the step carries the reverse
attribute (pyte `reverse`): step 4 the strip row's idle hint, step 5 the status row's `^X menu`.
`/tour` then reopens step 1 and `Esc` leaves the `Tour closed.` toast. The live process is spawned
with the raw PTY capture attached at spawn, so the first bytes are in `raw.pty`. Offline first:
the frames and `.rev` highlight masks of `story21_14_smoke_frames_export_b` at 60x16, 80x24,
130x30 and 130x30 with the monitor column, plus the strip-yield frame (a full composer at 60x16),
each predicate proven against torn controls.

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
FRAME_EMPTY = ("No agents active.", "Nothing here yet.")  # the frame export has no roster read


def hub_ok(screen, empty=LIVE_EMPTY, mode="Normal"):
    rows = _rows(screen)
    top = _frame_top(rows, "Mission Control · Agents")
    return top is not None and _tour_top(rows) is None and chrome_ok(screen, mode) \
        and any(text in row for row in rows for text in empty)


def help_ok(screen, mode="Normal"):
    rows = _rows(screen)
    return any(re.fullmatch(_LEFT + r"┌ ?Help — Rustain Keybindings ?─*┐" + _COL, row) for row in rows) \
        and _tour_top(rows) is None and chrome_ok(screen, mode)


COMPLETE_NARROW = "Tour complete. Help is always one key away: Ctrl+X,…"  # a 60-column toast headline


def complete_ok(screen, text=COMPLETE, rows=1, monitor=False, mode="Normal"):
    """The Success toast on the toast rows the strip-aware harness derives (a bordered toast of
    100+ columns spans three rows; beside a monitor column the harness's row-end anchor cannot
    apply, so the headline is matched inside those rows only, never in Stage or monitor text)."""
    if monitor:
        toast = any(f"Success {text}" in row for row in harness.toast_rows(screen, rows))
    else:
        toast = harness.toast_present(screen, text, "Success", rows=rows)
    return _tour_top(_rows(screen)) is None and toast and chrome_ok(screen, mode)


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


# ── Leg B: coach line, highlight, team defaults ───────────────────────────────────────────────
COACH = {
    1: "Tour 1/5 \u00b7 Send a message with Enter",
    4: "Tour 4/5 \u00b7 Open Mission Control with Ctrl+X, S",
    5: "Tour 5/5 \u00b7 Open help with Ctrl+X, ?",
}
TEAM_ARGS = ["-c", "onboarding.team=true"]


def _marked(mask):
    """{row: [columns]} of the highlighted cells of a `#`/`.` mask."""
    marks = {}
    for y, row in enumerate(mask.split("\n")):
        cols = [x for x, ch in enumerate(row) if ch == "#"]
        if cols:
            marks[y] = cols
    return marks


def _span(text, needle):
    start = text.rfind(needle)
    return None if start < 0 else list(range(start, start + len(needle)))


def expected_marks(screen, number):
    """Where the step's target is painted: the composer's top border (1), the strip's hint (4,
    the status row's `^X menu` when the strip yielded), the status row's `^X menu` (5)."""
    lines = screen.split("\n")
    top = harness.composer_top(lines)
    if top is None or top < 2:
        return None
    if number == 1:
        first = lines[top].index("\u250c")
        return {top: list(range(first, lines[top].index("\u2510", first) + 1))}
    strip = harness.strip_row(screen)
    if number == 4 and strip:
        hint = IDLE_HINT.match(strip)
        return {top - 2: list(range(hint.start(), hint.end()))} if hint else None
    span = _span(lines[top - 1], "^X menu")
    return {top - 1: span} if span else {}


def coach_frame_ok(screen, mask, number, mode="Normal", strip=True):
    """Row 0 of the chat pane is the coach line, exactly the target cells carry the highlight, and
    the status row (and the strip row unless it yielded) are in place."""
    if not _rows(screen)[0].startswith(COACH[number]):
        return False
    expected = expected_marks(screen, number)
    return expected is not None and _marked(mask) == expected and chrome_ok(screen, mode, strip)


def coach_torn_controls(screen, mask, number, strip=True):
    rows = screen.split("\n")
    masks = mask.split("\n")
    status = harness.status_row(screen)
    row_strip = harness.strip_row(screen)
    controls = {
        "coach line missing": "\n".join([""] + rows[1:]),
        "coach line on the wrong row": "\n".join([rows[1], rows[0]] + rows[2:]),
        "coach text torn": "\n".join([rows[0].replace("Tour", "Tou", 1)] + rows[1:]),
        "status row missing": "\n".join(r for r in rows if r != status),
        "toast over the coach line": "\n".join([f"{'':>30}Success Tour closed."] + rows[1:]),
    }
    if strip and row_strip:
        controls["strip row missing"] = "\n".join(r for r in rows if r.rstrip() != row_strip)
        swapped = [status if r.rstrip() == row_strip else row_strip if r == status else r for r in rows]
        controls["strip and status swapped"] = "\n".join(swapped)
    mask_controls = {
        "highlight missing": "\n".join("." * len(r) for r in masks),
        "highlight shifted by a cell": "\n".join("." + r[:-1] for r in masks),
        "highlight on the wrong row": "\n".join(masks[1:] + masks[:1]),
        "highlight everywhere": "\n".join("#" * len(r) for r in masks),
    }
    return controls, mask_controls


def _masks(width, height, suffix=""):
    root = Path(os.environ["RUSTAIN_TOUR_FRAMES"])
    return {path.name.split("-")[0]: path.read_text(encoding="utf-8")
            for path in root.glob(f"*-{width}x{height}{suffix}.rev")}


def _deskcheck_b():
    for width, height, suffix in [(w, h, "") for w, h in SIZES] + [(130, 30, "-monitor")]:
        frames, masks = _frames(width, height, suffix), _masks(width, height, suffix)
        label = f"{width}x{height}{suffix}"
        for number in (1, 4, 5):
            name = f"coach{number}"
            screen, mask = frames[name], masks[name]
            assert coach_frame_ok(screen, mask, number), f"{label} {name}:\n{screen}\n{mask}"
            controls, mask_controls = coach_torn_controls(screen, mask, number)
            for control, torn in controls.items():
                assert not coach_frame_ok(torn, mask, number), f"{label} {name}: `{control}` passed"
            for control, torn in mask_controls.items():
                if not expected_marks(screen, number) and control != "highlight everywhere":
                    continue  # the target is not painted at this width: no cells to tear
                assert not coach_frame_ok(screen, torn, number), f"{label} {name}: `{control}` passed"
            # The highlight is the step's own: another step's frame is not this step's.
            other = 5 if number != 5 else 1
            assert not coach_frame_ok(frames[f"coach{other}"], masks[f"coach{other}"], number)
        # The coach row is the chat pane's first row and never reaches the strip, the status row
        # or the composer (the target cells excepted).
        assert "Tour" not in "\n".join(_rows(frames["idle"])), f"{label}: no tour, no coach row"
    screen, mask = _frames(60, 16)["yield4"], _masks(60, 16)["yield4"]
    assert harness.strip_row(screen) == "", "the strip yielded its row"
    assert coach_frame_ok(screen, mask, 4, strip=False), f"yield:\n{screen}\n{mask}"
    assert not coach_frame_ok(screen, mask, 4), "the strip case does not hold on a yielded frame"
    lines = screen.split("\n")
    status = harness.status_row(screen)
    painted = "^X menu" in status  # at 60 columns the tail may not fit: then nothing is highlighted
    assert _marked(mask) == ({lines.index(status): _span(status, "^X menu")} if painted else {})
    controls, mask_controls = coach_torn_controls(screen, mask, 4, strip=False)
    for control, torn in controls.items():
        assert not coach_frame_ok(torn, mask, 4, strip=False), f"yield: `{control}` passed"
    for control, torn in mask_controls.items():
        if not painted and control != "highlight everywhere":
            continue  # an unpainted target has no cells to tear
        assert not coach_frame_ok(screen, torn, 4, strip=False), f"yield: `{control}` passed"


@pytest.mark.skipif(not DESKCHECK, reason="offline desk-check only")
def test_coach_predicates_accept_real_saved_frames_and_reject_torn_ones():
    _deskcheck_b()


def _reverse_row(tui, row):
    """Columns of screen row `row` whose cells are reverse-video and bold in the pyte screen."""
    tui.get_screen_text()  # drain the PTY into pyte first
    line = tui._screen.buffer[row]
    return [x for x in range(tui._screen.columns) if line[x].reverse and line[x].bold]


def _marked_rows(tui):
    tui.get_screen_text()
    out = {}
    for y in range(tui._screen.lines):
        cols = _reverse_row(tui, y)
        if cols:
            out[y] = cols
    return out


def _coach_live_ok(tui, number):
    """Predicate on a live screen: the coach row, the status/strip rows and exactly the target's
    reverse cells (the pyte attributes are read from the same drained screen)."""
    screen = tui.get_screen_text()
    expected = expected_marks(screen, number)
    return _rows(screen)[0].startswith(COACH[number]) and expected is not None \
        and _marked_rows(tui) == expected and chrome_ok(screen, "PLAN")


def _submit_tour_command(tui):
    """`/tour` in the composer, then Enter; the autocomplete popup may take the first Enter."""
    import time
    tui.send("/tour")
    tui.wait_until(lambda s: "/tour" in s, "the typed command", timeout=10)
    tui.send(ENTER)
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        if card_ok(tui.get_screen_text(), "Tour \u00b7 1 of 5", STEP_RAIL, "PLAN"):
            return
    tui.send(ENTER)


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_b_coach_line_highlight_and_team_defaults_at_80x24(monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    with tempfile.TemporaryDirectory(prefix="21-14-b-") as root:
        root = Path(root)
        workspace = root / "workspace"
        workspace.mkdir()
        home = root / "home"
        home.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file"}
        evidence = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())) / "21-14-leg-b"
        evidence.mkdir(parents=True, exist_ok=True)
        real_spawn = harness.pexpect.spawn
        with (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            def spawn(*args, **kwargs):
                child = real_spawn(*args, **kwargs)
                child.logfile_read = raw  # attached at spawn: the first bytes are captured too
                return child

            monkeypatch.setattr(harness.pexpect, "spawn", spawn)
            with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env,
                            extra_args=TEAM_ARGS) as tui:
                offer = tui.wait_until(lambda s: card_ok(s, "Tour", OFFER_RAIL, "PLAN"),
                                       "the offer card in Plan mode", timeout=15)
                assert "PLAN" in harness.status_row(offer), offer
                tui.send(ENTER)
                tui.wait_until(lambda s: card_ok(s, "Tour \u00b7 1 of 5", STEP_RAIL, "PLAN"), "step 1 card", timeout=10)
                for number in (2, 3, 4):
                    tui.send("n")
                    tui.wait_until(lambda s, n=number: card_ok(s, f"Tour \u00b7 {n} of 5", STEP_RAIL, "PLAN"),
                                   f"step {number} card", timeout=10)
                tui.send(ENTER)
                tui.wait_until(lambda s: _coach_live_ok(tui, 4), "the coach line and the strip highlight (step 4)", timeout=10)
                tui.send(CTRL_X + "s")
                tui.wait_until(lambda s: hub_ok(s, mode="PLAN"), "the Agents tab", timeout=15)
                assert not _marked_rows(tui), "no highlight with a frame open"
                tui.send(ESC)
                tui.wait_until(lambda s: card_ok(s, "Tour \u00b7 5 of 5", STEP_RAIL, "PLAN"), "step 5 card", timeout=15)
                tui.send(ENTER)
                tui.wait_until(lambda s: _coach_live_ok(tui, 5), "the coach line and the menu highlight (step 5)", timeout=10)
                tui.send(CTRL_X + "?")
                tui.wait_until(lambda s: help_ok(s, "PLAN"), "the help frame", timeout=10)
                tui.send(ESC)
                tui.wait_until(lambda s: complete_ok(s, mode="PLAN"), "the completion toast", timeout=10)
                assert not _marked_rows(tui), "no tour, no highlight"
                _submit_tour_command(tui)
                tui.wait_until(lambda s: card_ok(s, "Tour \u00b7 1 of 5", STEP_RAIL, "PLAN"), "step 1 card again", timeout=10)
                tui.send(ESC)
                tui.wait_until(lambda s: harness.toast_present(
                    s, "Tour closed. Run /tour to take it again.", "Info") and _tour_top(_rows(s)) is None,
                    "the closed toast", timeout=10)
        text = (workspace / ".rustain" / "state.toml").read_text(encoding="utf-8")
        assert re.search(r"^tour_done = true$", text, re.M), text
        import hashlib
        import json
        binary = harness._resolve_binary()[0]
        (evidence / "run.json").write_text(json.dumps(
            {"viewport": [80, 24], "binary": str(binary),
             "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
             "state_toml": text}) + "\n", encoding="utf-8")


# ── Leg C: the voice pass ────────────────────────────────────────────────────────────────────
# Leg C (slice 14-C, 80x24): `rustain --help`, `rustain daemon --help` and `rustain team --help`
# (non-PTY, bounded) exit 0 and carry no internal identifier and no first person; in the TUI,
# `/export <abs path>` (an absolute path is refused by the export handler without a provider) paints
# an Error toast whose row is the fact line, `Ctrl+X n` opens the notification log whose row shows
# the fact and not the next step, and `Enter` opens Details that show the fact AND the next step.
# Offline first: the frames of `story21_14_smoke_frames_export_c` (toast, log list, details at
# 60x16, 80x24, 130x30 and 130x30 with the monitor column) and the rendered help texts.
ID_RE = re.compile(r"\b(?:(?:Story|Stories|Epic)\s+\d|(?:FR|NFR|UX-DR|ADR|AD|RU|DF|KG|MW|LR|OD|TM|AC)-?\d)")
FP_RE = re.compile(r"(?:^|[^\w/])(?:I|I'm|I'll|I've|I'd)\b(?!/O)", re.M)
ERR_WHAT = "Could not export to an absolute path."
ERR_NEXT = "Run /export again with a name relative to .rustain/exports/."
HELP_COMMANDS = ((), ("daemon",), ("team",))


def help_clean(text):
    """A rendered help text: non-empty, no internal identifier, no first person."""
    return bool(text.strip()) and ID_RE.search(text) is None and FP_RE.search(text) is None


def _flat(screen):
    """Whitespace-normalised text of a frame, borders dropped (a wrapped sentence is found whole)."""
    return " ".join(re.sub(r"[│┌┐└┘─]", " ", screen).split())


def err_toast_ok(screen, monitor=False, rows=1, mode="Normal"):
    """The Error toast is on the strip-aware toast rows and shows only the fact (+ `…`)."""
    text = ERR_WHAT + "…"
    if monitor:
        toast = any(f"Error {text}" in row for row in harness.toast_rows(screen, rows))
    else:
        toast = harness.toast_present(screen, text, "Error", rows=rows)
    return toast and ERR_NEXT not in _flat(screen) and chrome_ok(screen, mode)


def err_log_ok(screen, full=True):
    """The S-LOG list: the newest row is the Error row carrying the export fact (whole, with its
    ellipsis, from 80 columns; a clipped prefix of at least 20 characters at 60), and no next step."""
    rows = harness.notification_log_rows(screen)
    if not rows or ERR_NEXT in harness.notification_log_text(screen):
        return False
    found = re.match(r"Error · \d\d:\d\d:\d\d · export · (.*)$", rows[0])
    if found is None:
        return False
    body = found.group(1).rstrip()
    fact = ERR_WHAT + "…"
    if body == fact:
        return True
    return not full and body.endswith("…") and len(body) > 20 and fact.startswith(body[:-1])


def err_details_ok(screen):
    """The S-LOG Details: the fact and the next step, wrapped or not."""
    text = harness.notification_log_text(screen)
    return ERR_WHAT in text and ERR_NEXT in text


def _deskcheck_c():
    for name in ("root", "daemon", "team"):
        text = (Path(os.environ["RUSTAIN_TOUR_FRAMES"]) / f"help-{name}.txt").read_text(encoding="utf-8")
        assert help_clean(text), f"help {name}:\n{text}"
        assert len(text.splitlines()) > 4, f"help {name} is not empty"
        for control in (text + "\n  (Story 9.4)", text + "\nI will draft it for you", text.replace("Usage", "Usage (FR12)", 1),
                        text + "\nSee ADR-09-02.", ""):
            assert not help_clean(control), f"help {name}: torn control passed:\n{control[-80:]!r}"
    assert help_clean("Say I/O errors out loud") and not help_clean("Say: I think so")
    for width, height, suffix in [(w, h, "") for w, h in SIZES] + [(130, 30, "-monitor")]:
        frames = _frames(width, height, suffix)
        label = f"{width}x{height}{suffix}"
        monitor, rows = suffix == "-monitor", (3 if width >= 100 else 1)
        toast, log, details = frames["errtoast"], frames["errlog"], frames["errdetails"]
        assert err_toast_ok(toast, monitor, rows), f"{label} toast:\n{toast}"
        full = width >= 80
        assert err_log_ok(log, full), f"{label} log:\n{log}"
        # Torn LOG-row facts: truncated to `Could not`, another export fact, a leaked next step.
        row = next(r for r in _rows(log) if "export · Could not" in r)
        body = row[row.index("export · ") + len("export · "):].rstrip(" │")
        torn = ["Could not…", "Could not export the conversation.…", "Could not export to an a"]
        if width >= 130:  # the row has room to carry a leaked next step inside the frame
            torn.append(body + " " + ERR_NEXT)
        for torn_body in torn:
            assert not err_log_ok(log.replace(body, torn_body, 1), full), f"{label}: torn log row `{torn_body}` passed"
        assert not err_log_ok("\n".join(r for r in _rows(log) if "export ·" not in r), full)
        assert err_details_ok(details), f"{label} details:\n{details}"
        # Torn controls: each predicate fails when its fact is gone, moved or merged.
        assert not err_toast_ok(toast.replace(ERR_WHAT, "Could not exp"), monitor, rows)
        assert not err_toast_ok(toast.replace("Error " + ERR_WHAT, "Error " + ERR_WHAT + " " + ERR_NEXT), monitor, rows)
        assert not err_toast_ok("\n".join(r for r in _rows(toast) if ERR_WHAT not in r), monitor, rows)
        moved = [r.replace("Error " + ERR_WHAT, "") if ERR_WHAT in r else r for r in _rows(toast)]
        moved[2] = (moved[2] + "  Error " + ERR_WHAT + "…").rstrip()
        assert not err_toast_ok("\n".join(moved), monitor, rows), f"{label}: a toast moved into the Stage passed"
        assert not err_toast_ok(toast, monitor, rows, mode="PLAN")
        assert not err_log_ok(toast) and not err_log_ok(details)
        assert not err_log_ok(log.replace("Error ·", "Info  ·"))
        assert not err_details_ok(log) and not err_details_ok(toast)
        assert not err_details_ok(details.replace("Run /export", "Run /expo"))
        assert not err_details_ok(details.replace("absolute path", "abso"))
        assert not err_details_ok("\n".join(r for r in _rows(details) if "Run" not in r))


@pytest.mark.skipif(not DESKCHECK, reason="offline desk-check only")
def test_voice_predicates_accept_real_saved_frames_and_reject_torn_ones():
    _deskcheck_c()


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_c_help_is_plain_and_an_error_states_its_next_step_at_80x24(monkeypatch):
    import hashlib
    import json
    import subprocess
    import time

    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    binary = harness._resolve_binary()[0]
    with tempfile.TemporaryDirectory(prefix="21-14-c-") as root:
        root = Path(root)
        workspace = root / "workspace"
        workspace.mkdir()
        home = root / "home"
        home.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file"}
        evidence = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())) / "21-14-leg-c"
        evidence.mkdir(parents=True, exist_ok=True)
        helps = {}
        clean_env = {**os.environ, "HOME": str(home), "RUSTAIN_DATA_DIR": str(root / "data"),
                     "RUSTAIN_CONFIG_DIR": str(root / "config")}
        for command in HELP_COMMANDS:
            done = subprocess.run([str(binary), *command, "--help"], capture_output=True, text=True,
                                  timeout=60, cwd=str(workspace), env=clean_env, stdin=subprocess.DEVNULL)
            label = " ".join(("rustain", *command, "--help"))
            assert done.returncode == 0, f"{label}: exit {done.returncode}\n{done.stderr}"
            assert help_clean(done.stdout), f"{label}:\n{done.stdout}"
            helps[label] = done.returncode
            (evidence / ("help-" + (command[0] if command else "root") + ".txt")).write_text(done.stdout, encoding="utf-8")
        target = workspace / "missing-dir" / "out.md"
        real_spawn = harness.pexpect.spawn
        with (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            def spawn(*args, **kwargs):
                child = real_spawn(*args, **kwargs)
                child.logfile_read = raw  # attached at spawn: the first bytes are captured too
                return child

            monkeypatch.setattr(harness.pexpect, "spawn", spawn)
            with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui:
                assert tui.wait_for_screen("Type a message to start.", timeout=15), tui.get_screen_text()
                tui.send(f"/export {target}")
                tui.wait_until(lambda s: "out.md" in s, "the typed command", timeout=10)
                tui.send(ENTER)
                deadline = time.monotonic() + 4
                while time.monotonic() < deadline and not err_toast_ok(tui.get_screen_text()):
                    pass
                if not err_toast_ok(tui.get_screen_text()):
                    tui.send(ENTER)  # the autocomplete popup took the first Enter
                toast = tui.wait_until(err_toast_ok, "the Error toast (the fact line only)", timeout=15)
                tui.send(CTRL_X + "n")
                log = tui.wait_until(err_log_ok, "the notification log row", timeout=10)
                assert ERR_NEXT not in harness.notification_log_text(log)
                tui.send(ENTER)
                details = tui.wait_until(err_details_ok, "the Details with the next step", timeout=10)
        (evidence / "run.json").write_text(json.dumps(
            {"viewport": [80, 24], "binary": str(binary),
             "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
             "help_exit_codes": helps, "toast_row": harness.toast_rows(toast)[0].strip(),
             "details": harness.notification_log_text(details)}) + "\n", encoding="utf-8")
