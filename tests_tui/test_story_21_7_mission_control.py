"""Story 21.7 native smoke: Mission Control (S-MC), one leg per slice. Generic lane (no marker
that CI deselects); the binary is resolved by the harness (RUSTAIN_TUI_BINARY, else the build).

Leg A (slice 7-A, 80x24): a seeded durable room journal; `Ctrl+X r` paints the frame whose title
row reads `Mission Control · Room`, the tab row, a seeded node row and the rail, with the status
row intact below; `Esc` leaves the Stage rows as they were.

Leg B (slice 7-B, 80x24): seeded A2A records and one stored artifact; `/team log` paints the frame
whose title row reads `Mission Control · Messages` with the seeded row; `Right` (to Room, then to
Artifacts) and `Enter` on the artifact row open the window titled `Artifact · <name>` with the
body row and the `[c] Copy` hint; `c` writes exactly the seeded source bytes to the clipboard file;
`Esc` closes the window, then the hub.

Offline first: RUSTAIN_MISSION_DESKCHECK=1 with RUSTAIN_MISSION_FRAMES=<dir> consumes only the
frames written by the ignored Rust test `story21_7_smoke_frames_export` (60x16, 80x24, 130x30);
no binary or subprocess is started. Every predicate passes on the intended frame and fails on
torn controls. Predicates are anchored on whole rows; waits are structural, never sleeps.
"""
import hashlib
import json
import os
import re
import tempfile
import time
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
    tab_row = re.fullmatch(r"\s*│ Messages  ▸Room   Artifacts\s*│\s*", rows[top + 1]) is not None
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

MESSAGES_TITLE = "Mission Control · Messages"
SEED_TS = "2023-11-14 22:13:20Z"
ARTIFACT_BODY = "proof artifact body line\n"


def _frame_bounds(rows, title):
    top = _frame_top(rows, title)
    if top is None:
        return None, None
    bottom = next((i for i in range(top + 1, len(rows)) if re.fullmatch(r"\s*└─*┘\s*", rows[i])), None)
    return top, bottom


def messages_frame_ok(screen):
    """Title row, the Messages tab row directly below it, the seeded A2A row inside the frame, the
    rail row (with the way out) last inside it and the status row intact below."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, MESSAGES_TITLE)
    if top is None or bottom is None or bottom < top + 4:
        return False
    tab_row = re.fullmatch(r"\s*│▸Messages\b.*│\s*", rows[top + 1]) is not None
    seeded = any(re.fullmatch(r".*│ *" + SEED_TS + r" .*\S *│\s*", row) for row in rows[top + 2:bottom - 1])
    rail = re.fullmatch(r"\s*│.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return tab_row and seeded and rail and status


def artifact_title(prefix):
    return f"Artifact · evidence {prefix}"


def artifact_frame_ok(screen, prefix="aaaaaa"):
    """Title row `Artifact · <name>`, the body row inside the frame, the rail row with the copy
    hint and the way out, and the status row intact below."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, artifact_title(prefix))
    if top is None or bottom is None or bottom < top + 3:
        return False
    body = any(re.fullmatch(r".*│ *" + re.escape(ARTIFACT_BODY.strip()) + r" *│\s*", row)
               for row in rows[top + 1:bottom - 1])
    rail = re.fullmatch(r"\s*│.*\[c\] Copy.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return body and rail and status


def _torn_frame(screen, title, marker, swap_to, extra):
    """Torn controls of a hub/window frame: `marker` is a substring of the row whose loss or cut
    must fail the predicate; `extra` is a {label: (old, new)} text replacement over every row."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, title)
    assert top is not None and bottom is not None, screen
    index = next(i for i in range(top + 1, bottom) if marker in rows[i])
    controls = {}
    controls["cut mid-row"] = "\n".join(rows[:index] + [rows[index][: max(len(rows[index]) // 2, 1)]] + rows[index + 1:])
    controls["marker row missing"] = "\n".join(rows[:index] + rows[index + 1:])
    controls["status row missing"] = "\n".join(row for row in rows if STATUS not in row)
    controls["rail half painted"] = "\n".join(row.replace("[Esc] Back", "[Esc") for row in rows)
    controls["title swapped for status"] = "\n".join(row.replace(title, swap_to) for row in rows)
    for label, (old, new) in extra.items():
        controls[label] = "\n".join(row.replace(old, new) for row in rows)
    return controls


def torn_messages(screen):
    controls = _torn_frame(screen, MESSAGES_TITLE, SEED_TS, "Needs you (1)",
                           {"tab row unmarked": ("▸Messages", " Messages")})
    rows = _rows(screen)
    top, _ = _frame_bounds(rows, MESSAGES_TITLE)
    controls["tab row missing"] = "\n".join(rows[:top + 1] + rows[top + 2:])
    return controls


def torn_artifact(screen, prefix="aaaaaa"):
    return _torn_frame(screen, artifact_title(prefix), ARTIFACT_BODY.strip(), "Needs you (1)",
                       {"copy hint missing": ("[c] Copy", "")})

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
    for width, height in SIZES:
        name = f"{width}x{height}"
        messages = (frames / f"messages-open-{name}.txt").read_text(encoding="utf-8")
        artifact = (frames / f"artifact-open-{name}.txt").read_text(encoding="utf-8")
        before = (frames / f"stage-before-{name}.txt").read_text(encoding="utf-8")
        room = (frames / f"room-open-{name}.txt").read_text(encoding="utf-8")
        assert messages_frame_ok(messages), f"{name}: the Messages frame must pass\n{messages}"
        assert artifact_frame_ok(artifact), f"{name}: the artifact window must pass\n{artifact}"
        for label, frame in (("the Stage", before), ("the Room frame", room)):
            assert not messages_frame_ok(frame), f"{name}: Messages is absent from {label}"
            assert not artifact_frame_ok(frame), f"{name}: the window is absent from {label}"
        assert not messages_frame_ok(artifact) and not artifact_frame_ok(messages), name
        assert not artifact_frame_ok(artifact, prefix="bbbbbb"), f"{name}: another artifact's title fails"
        for label, torn in torn_messages(messages).items():
            assert not messages_frame_ok(torn), f"{name}: torn Messages control `{label}` must fail:\n{torn}"
        for label, torn in torn_artifact(artifact).items():
            assert not artifact_frame_ok(torn), f"{name}: torn window control `{label}` must fail:\n{torn}"
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


def _seed_leg_b(journal: Path, workspace: Path) -> str:
    """Two A2A rejections, one durable evidence artifact and its stored body (the id of a non-patch
    artifact is the body's SHA-256). Returns the artifact id."""
    body = ARTIFACT_BODY.encode("utf-8")
    artifact_id = hashlib.sha256(body).hexdigest()
    host = {"host_id": "old", "workspace_id": "fixture-workspace"}
    artifact = {
        "id": artifact_id, "kind": "evidence", "producer": "proof-node", "content_hash": artifact_id,
        "authority": [0] * 32, "provenance": ["user_originated"], "depends_on": [], "review": None,
        "host": host,
    }
    store = workspace / ".rustain" / "artifacts" / artifact_id
    store.mkdir(parents=True)
    (store / "body").write_bytes(body)
    (store / "meta.json").write_text(json.dumps(artifact, separators=(",", ":")), encoding="utf-8")
    peer = "12204bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0"
    events = [
        {"event": "node_registered", "node": "proof-node", "origin": "Interactive", "host": host},
        {"event": "artifact_created", "artifact": artifact},
        *[{"event": "remote_envelope_rejected", "peer": peer,
           "reason": {"reason": "policy", "detail": f"smoke {index:02d}"},
           "direction": "inbound", "task": f"smoke-{index:02d}"} for index in range(2)],
    ]
    with journal.open("w", encoding="utf-8") as stream:
        for seq, event in enumerate(events, start=1):
            entry = {"schema_version": 1, "seq": seq,
                     "recorded_at_ms": 1_700_000_000_000 + (seq - 3) * 1000 if seq > 2 else 1_700_000_000_000,
                     "record": {"kind": "room", "payload": event}}
            stream.write(json.dumps(entry, separators=(",", ":")) + "\n")
        stream.flush()
        os.fsync(stream.fileno())
    return artifact_id


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_b_messages_and_artifact_window_at_80x24(monkeypatch):
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    with tempfile.TemporaryDirectory(prefix="21-7-b-") as root:
        root = Path(root)
        workspace = root / "workspace"
        config = workspace / ".rustain"
        config.mkdir(parents=True)
        home = root / "home"
        home.mkdir()
        data = root / "data"
        data.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file",
               "RUSTAIN_DATA_DIR": str(data)}
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as writer:
            assert writer.wait_for_screen("Ready", timeout=15), writer.get_screen_text()
        journals = sorted((config / "rooms").glob("*.jsonl"))
        assert len(journals) == 1, journals
        artifact_id = _seed_leg_b(journals[0], workspace)
        prefix = artifact_id[:6]
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-b"
        evidence.mkdir(parents=True, exist_ok=True)
        binary = str(harness._resolve_binary()[0])
        clipboard = data / "clipboard.txt"
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui, \
                (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            before = tui.get_screen_text()
            assert not messages_frame_ok(before) and not artifact_frame_ok(before, prefix)

            tui.send("/team log")
            tui.wait_until(lambda s: "/team log" in s, "`/team log` typed", timeout=10)
            tui.send("\r")
            messages = tui.wait_until(messages_frame_ok, "the Messages frame with the seeded A2A row", timeout=15)
            assert not any("Needs you (" in row for row in _rows(messages))

            tui.send("\x1b[C")
            room = tui.wait_until(lambda s: _frame_top(_rows(s)) is not None, "the Room tab", timeout=10)
            tui.send("\x1b[C")
            artifacts = tui.wait_until(
                lambda s: _frame_top(_rows(s), "Mission Control · Artifacts") is not None
                and _has_row(s, r".*│.*evidence " + prefix + r".*│.*"),
                "the Artifacts tab with the seeded artifact row", timeout=15)
            tui.send("\r")
            window = tui.wait_until(lambda s: artifact_frame_ok(s, prefix),
                                    "the artifact window with its body row", timeout=15)
            assert not messages_frame_ok(window)

            tui.send("c")
            deadline = time.monotonic() + 10
            while not clipboard.exists() and time.monotonic() < deadline:
                tui.get_screen_text()
            assert clipboard.exists(), f"`c` must write the clipboard file:\n{tui.get_screen_text()}"
            assert clipboard.read_bytes() == ARTIFACT_BODY.encode("utf-8")

            tui.send("\x1b")
            tui.wait_until(lambda s: not artifact_frame_ok(s, prefix)
                           and _frame_top(_rows(s), "Mission Control · Artifacts") is not None,
                           "the window closed over the Artifacts tab", timeout=10)
            tui.send("\x1b")
            after = tui.wait_until(
                lambda s: _frame_top(_rows(s), "Mission Control · Artifacts") is None
                and _frame_top(_rows(s), MESSAGES_TITLE) is None, "the hub closed", timeout=10)
            for name, frame in (("stage-before", before), ("messages-open", messages), ("room-open", room),
                                ("artifacts-open", artifacts), ("artifact-window", window),
                                ("stage-after", after)):
                (evidence / f"{name}.txt").write_text(frame, encoding="utf-8")
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "artifact": artifact_id, "result": "PENDING"}) + "\n")
            # The Stage is what the Esc restored. Only toast rows (they float over the Stage and
            # differ by what this run raised: the start-up notices, the copy confirmation) and the
            # status row's `log: N` reminder segment are ignored; every other row must be equal.
            def core(screen):
                rows = []
                for row in stage_rows(screen):
                    if re.match(r"\s*(Info|Success|Warning|Error) ", row):
                        row = ""
                    rows.append(re.sub(r"(log: \d+|Log !\d+)\s*(\^X menu)", r"\2", row).rstrip())
                return rows
            assert core(after) == core(before), f"Esc must restore the Stage:\n{before}\n{after}"
            assert any("log: 2" in row for row in _rows(before)), before
            assert not any("log: 2" in row for row in _rows(after)), after
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "artifact": artifact_id, "result": "PASS"}) + "\n")
