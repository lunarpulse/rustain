"""Story 21.7 native Mission Control Room and History smoke at 80x24.

Leg A seeds a durable room journal, opens the Room frame with Ctrl+X r and
checks that Esc restores the Stage.

Leg E (slice 7-E, 80x24): two saved sessions; the launchpad key line names `^X h History`;
`Ctrl+X h` paints the `History` frame with both titles and the rail; `j` then `Enter` opens the
second (older) session and its transcript row is on the Stage with the frame gone.

Predicates are anchored on whole rows; waits are structural.
"""
import json
import os
import re
import tempfile
from pathlib import Path

import pytest

import harness
from harness import RustainTUI
from keys import CTRL_X


def _rows(screen):
    return [row.rstrip() for row in screen.split("\n")]


def _has_row(screen, pattern):
    compiled = re.compile(pattern)
    return any(compiled.fullmatch(row) for row in _rows(screen))


def _frame_top(rows, title="Mission Control · Room"):
    # A frame stacked over the hub paints its title behind the breadcrumb `Mission Control · <Tab> › `.
    pattern = re.compile(r"(?:│\s*|\s*)┌ ?(?:Mission Control · \w+ › )?" + re.escape(title) + r" ?─*┐\s*")
    for index, row in enumerate(rows):
        if pattern.fullmatch(row):
            return index
    return None

STATUS = "│ Normal │"


def room_frame_ok(screen):
    """Title row, tab row directly below it, a body row with the room header, the rail row and the
    status row still intact below the frame."""
    rows = _rows(screen)
    top = _frame_top(rows)
    if top is None or top + 3 >= len(rows):
        return False
    tab_row = re.fullmatch(r"\s*│ Agents   Tasks   Messages  ▸Room   Artifacts\s*│\s*", rows[top + 1]) is not None
    body = any(re.fullmatch(r"\s*│Durable Room · as of .*│\s*", row) for row in rows[top + 2:])
    bottom = next((i for i in range(top + 1, len(rows)) if re.fullmatch(r"\s*└─*┘\s*", rows[i])), None)
    if bottom is None or bottom < top + 3:
        return False
    rail = re.fullmatch(r"\s*│.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return tab_row and body and rail and status


def stage_rows(screen):
    """The Stage rows: everything above the status row that precedes the composer."""
    rows = _rows(screen)
    composer = next((i for i, row in enumerate(rows) if "┌ Message" in row or "┌Message" in row), len(rows))
    return rows[:max(composer - 1, 0)]


def _frame_bounds(rows, title):
    top = _frame_top(rows, title)
    if top is None:
        return None, None
    bottom = next((i for i in range(top + 1, len(rows)) if re.fullmatch(r"\s*└─*┘\s*", rows[i])), None)
    return top, bottom


HISTORY_TITLE = "History"
HISTORY_A = "Alpha history session"
HISTORY_B = "Bravo history session"
HISTORY_ROW = "Parser B wins: it has fewer cases."


def launchpad_key_ok(screen):
    """The launchpad key line names the generated History chord (registry text, any width)."""
    return any(re.fullmatch(r"\s*\^X \? Help .*\^X h History\s*", row) for row in _rows(screen))


def history_frame_ok(screen):
    """`History` title row, both saved titles with their `msgs` facts inside the frame, the rail
    with `Open` and the way out last, and the status row intact below."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, HISTORY_TITLE)
    if top is None or bottom is None or bottom < top + 3:
        return False
    inside = rows[top + 1:bottom - 1]
    both = all(any(re.fullmatch(r".*│.*" + re.escape(t) + r" · .* · \d+ msgs.*│\s*", row) for row in inside)
               for t in (HISTORY_A, HISTORY_B))
    rail = re.fullmatch(r"\s*│.*\[Enter\] Open.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return both and rail and status


def transcript_ok(screen):
    return HISTORY_ROW in screen and _frame_top(_rows(screen), HISTORY_TITLE) is None


def _seed_journal(journal: Path) -> None:
    host = {"host_id": "old", "workspace_id": "fixture-workspace"}
    events = [
        {"event": "node_registered", "node": "proof-node", "origin": "Interactive", "host": host},
        {"event": "node_state_changed", "node": "proof-node", "from": "created", "to": "running"},
    ]
    with journal.open("w", encoding="utf-8") as stream:
        for seq, event in enumerate(events, start=1):
            entry = {"schema_version": 1, "seq": seq, "recorded_at_ms": 1_700_000_000_000,
                     "record": {"kind": "room", "payload": event}}
            stream.write(json.dumps(entry, separators=(",", ":")) + "\n")
        stream.flush()
        os.fsync(stream.fileno())


@pytest.fixture(scope="session", autouse=True)
def build_binary():
    """Assert the supplied or resolved binary exists."""
    assert harness._resolve_binary()[0].is_file()


def test_leg_a_room_tab_at_80x24(monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    with tempfile.TemporaryDirectory(prefix="21-7-a-") as root:
        root = Path(root)
        workspace = root / "workspace"
        config = workspace / ".rustain"
        config.mkdir(parents=True)
        home = root / "home"
        home.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file"}
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as writer:
            assert writer.wait_for_screen("Ready", timeout=15), writer.get_screen_text()
        journals = sorted((config / "rooms").glob("*.jsonl"))
        assert len(journals) == 1, journals
        _seed_journal(journals[0])
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-a"
        evidence.mkdir(parents=True, exist_ok=True)
        binary = str(harness._resolve_binary()[0])
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui, \
                (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            before = tui.get_screen_text()
            tui.send(CTRL_X + "r")
            screen = tui.wait_until(
                lambda s: room_frame_ok(s) and _has_row(s, r".*│.*proof-node.*│.*"),
                "the Room frame with the seeded node row", timeout=15)
            assert not any("Needs you (" in row for row in _rows(screen))
            tui.send("\x1b")
            after = tui.wait_until(lambda s: _frame_top(_rows(s)) is None, "the frame closed", timeout=10)
            (evidence / "stage-before.txt").write_text(before, encoding="utf-8")
            (evidence / "room-open.txt").write_text(screen, encoding="utf-8")
            (evidence / "stage-after.txt").write_text(after, encoding="utf-8")
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "result": "PASS-PENDING-STAGE-COMPARE"}) + "\n")
            assert stage_rows(after) == stage_rows(before), f"Esc must restore the Stage:\n{before}\n{after}"


def _history_session(session_id: str, title: str, updated: int, with_turns: bool) -> dict:
    base = json.loads((Path(__file__).parent / "fixtures" / "sessions" / "story21_9_stage_polish.json").read_text())
    session = {**base, "id": session_id, "title": title, "updatedAt": updated, "createdAt": updated}
    if not with_turns:
        session = {**session, "messages": [], "turns": []}
    return session


def test_leg_e_history_window_at_80x24(monkeypatch):
    """Leg E (7-E): two saved sessions; the launchpad key line names `^X h History`; `Ctrl+X h`
    -> the `History` frame with both titles; `j`, `Enter` -> the older session's transcript row
    on the Stage and the frame gone. Hermetic."""
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    with tempfile.TemporaryDirectory(prefix="21-7-e-") as root:
        root = Path(root)
        workspace = root / "workspace"
        sessions = workspace / ".claude" / "sessions"
        sessions.mkdir(parents=True)
        for sid, title, updated, turns in (("story21-7-alpha", HISTORY_A, 1700000, True),
                                           ("story21-7-bravo", HISTORY_B, 1700100, False)):
            (sessions / f"{sid}.meta.json").write_text(json.dumps(_history_session(sid, title, updated, turns)))
        (workspace / ".env").write_text("")
        home = root / "home"
        home.mkdir()
        data = root / "data"
        data.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file",
               "RUSTAIN_DATA_DIR": str(data)}
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-e"
        evidence.mkdir(parents=True, exist_ok=True)
        binary = str(harness._resolve_binary()[0])
        with RustainTUI(fresh=False, build=False, workspace=workspace, env_overrides=env) as tui, \
                (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            launch = tui.wait_until(launchpad_key_ok, "the launchpad key line names History", timeout=15)
            assert not history_frame_ok(launch)
            tui.send(CTRL_X + "h")
            history = tui.wait_until(history_frame_ok, "the History frame with both sessions", timeout=15)
            tui.send("j")
            tui.wait_until(lambda s: any(re.search(r"▸ .*" + HISTORY_A, row) for row in _rows(s)),
                           "the older session selected", timeout=10)
            tui.send("\r")
            after = tui.wait_until(transcript_ok, "the opened session's transcript row", timeout=15)
            for name, frame in (("launchpad", launch), ("history-open", history), ("transcript", after)):
                (evidence / f"{name}.txt").write_text(frame, encoding="utf-8")
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "result": "PASS"}) + "\n")
