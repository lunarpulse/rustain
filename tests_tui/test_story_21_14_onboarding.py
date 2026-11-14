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
with the raw PTY capture attached at spawn, so the first bytes are in `raw.pty`.

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


def complete_ok(screen, text=COMPLETE, rows=1, monitor=False, mode="Normal"):
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
