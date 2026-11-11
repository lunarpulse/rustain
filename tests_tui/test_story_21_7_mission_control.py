"""Story 21.7 native smoke: Mission Control (S-MC), one leg per slice. Generic lane (no marker
that CI deselects); the binary is resolved by the harness (RUSTAIN_TUI_BINARY, else the build).

Leg A (slice 7-A, 80x24): a seeded durable room journal; `Ctrl+X r` paints the frame whose title
row reads `Mission Control · Room`, the tab row, a seeded node row and the rail, with the status
row intact below; `Esc` leaves the Stage rows as they were.

Offline first: RUSTAIN_MISSION_DESKCHECK=1 with RUSTAIN_MISSION_FRAMES=<dir> consumes only the
frames written by the ignored Rust test `story21_7_smoke_frames_export` (60x16, 80x24, 130x30);
no binary or subprocess is started. Every predicate passes on the intended frame and fails on
torn controls. Predicates are anchored on whole rows; waits are structural, never sleeps.
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

DESKCHECK = os.environ.get("RUSTAIN_MISSION_DESKCHECK") == "1"
SIZES = ((60, 16), (80, 24), (130, 30))


def _rows(screen):
    return [row.rstrip() for row in screen.split("\n")]


def _has_row(screen, pattern):
    compiled = re.compile(pattern)
    return any(compiled.fullmatch(row) for row in _rows(screen))


def _frame_top(rows):
    for index, row in enumerate(rows):
        if re.fullmatch(r"(?:│\s*|\s*)┌Mission Control · Room─*┐\s*", row):
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
    tab_row = re.fullmatch(r"\s*│▸Room\s*│\s*", rows[top + 1]) is not None
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


def torn_controls(screen):
    rows = _rows(screen)
    top = _frame_top(rows)
    assert top is not None
    controls = {}
    controls["cut mid-row"] = "\n".join(rows[:top + 2] + [rows[top + 2][: max(len(rows[top + 2]) // 2, 1)]] + rows[top + 3:])
    controls["status row missing"] = "\n".join(row for row in rows if STATUS not in row)
    controls["tab row missing"] = "\n".join(rows[:top + 1] + rows[top + 2:])
    controls["rail half painted"] = "\n".join(row.replace("[Esc] Back", "[Esc") for row in rows)
    controls["title swapped for status"] = "\n".join(
        row.replace("Mission Control · Room", "Needs you (1)") for row in rows)
    return controls


def _deskcheck():
    frames = Path(os.environ["RUSTAIN_MISSION_FRAMES"])
    for width, height in SIZES:
        name = f"{width}x{height}"
        open_frame = (frames / f"room-open-{name}.txt").read_text(encoding="utf-8")
        before = (frames / f"stage-before-{name}.txt").read_text(encoding="utf-8")
        after = (frames / f"stage-after-{name}.txt").read_text(encoding="utf-8")
        assert room_frame_ok(open_frame), f"{name}: the intended frame must pass\n{open_frame}"
        assert not room_frame_ok(before), f"{name}: the frame is absent before the open"
        assert stage_rows(before) == stage_rows(after), f"{name}: Esc restores the Stage rows"
        assert not room_frame_ok(after), f"{name}: the frame is gone after Esc"
        for label, torn in torn_controls(open_frame).items():
            assert not room_frame_ok(torn), f"{name}: torn control `{label}` must fail:\n{torn}"
    print("desk-check ok", [f"{w}x{h}" for w, h in SIZES])


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
    """Offline desk-check starts no binary; live runs use the harness resolution."""
    if not DESKCHECK:
        assert harness._resolve_binary()[0].is_file()


@pytest.mark.skipif(not DESKCHECK, reason="offline desk-check only")
def test_mission_predicates_accept_real_saved_frames_and_reject_torn_ones():
    _deskcheck()


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
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
