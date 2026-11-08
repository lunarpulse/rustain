"""Story 21.6 K15: the decision layer and the Needs-you queue, native smoke.

One file, one leg per slice, 80x24, DEBUG binary (`RUSTAIN_TUI_BINARY`), isolated
HOME/config/data dirs, no sleeps: every wait is a bounded structural predicate.

* Leg A (slice 6-A, generic lane): the six interim kinds are L3 frames. The proven
  21-1 S2 scenario (recovery after `kill -9`, budget arrival while a draft is typed,
  `Ctrl+X q`, `Esc` defers, the chip) runs on the real binary; then the frames it
  captured are checked as the sheet they are: title `Decision: <kind>`, the
  breadcrumb row, the body, the rail, every border, nothing bottom-anchored.

* Leg B checks the read-only mirror while a writer's permission is pending.
  Its live snapshot count excludes approvals already issued to that writer, so
  the snapshot-seeded waiting badge is covered by the attached mirror unit test
  rather than asserted in this journey.

* Leg C (slice 6-C, generic lane): a scene AskUserQuestion is the `Decision: Question`
  frame. The answer is typed into the frame's own field (`Answer: blue▋`; the composer
  stays empty), `Esc` `Esc` defers it (chip `Needs you (1) ^X q`, the turn keeps
  streaming), `Ctrl+X q` + `Enter` reopens it with the field intact and `Enter` sends it.

The frame predicate is desk-checked offline first (a torn frame that paints the
title but only half the rail must fail it): those tests need no binary. The
native leg skips when no binary is built (`RUSTAIN_TUI_BINARY` absent).
"""
from contextlib import ExitStack
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time
import traceback
import re

import harness
import pytest
from fixtures.scene_provider import SceneStub
from test_story_21_3_keymap_truth import _PermissionTUI, _permission_daemon

import test_story_21_1_notification_lifecycle as s21_1

WIDTH, HEIGHT = 80, 24
REGION = HEIGHT - 4  # the modal region: everything above the composer and status row
RECOVERY_RAIL = ("[Enter/Esc] Continue", "[n] New session")
BUDGET_RAIL = ("[y] Continue", "[s] Switch model", "[p] Pause until tomorrow", "[Esc] Later")


def decision_frame(screen, title, rail, width=WIDTH, height=HEIGHT, crumb_title=None):
    """The decision frame as the exact L3 sheet (full width below 100 columns),
    every border painted: returns its content rows, or None while it is torn,
    partial, bottom-anchored or missing a rail pair. Never accepts half a frame."""
    rows = screen.split("\n")
    if len(rows) != height or any(len(row) != width for row in rows):
        return None
    region = height - 4
    top, bottom = rows[0], rows[region - 1]
    if not (top.startswith(f"┌ Decision: {title} ") and top.endswith("┐")):
        return None
    if not (bottom.startswith("└") and bottom.endswith("┘") and set(bottom[1:-1]) == {"─"}):
        return None
    interior = rows[1:region - 1]
    if any(not (row.startswith("│") and row.endswith("│")) for row in interior):
        return None
    content = [row[1:-1].strip() for row in interior if row[1:-1].strip()]
    if not content or not content[0].startswith(f"Needs you › {crumb_title or title} |"):
        return None
    # The rail sits under the body; a Ctrl+X menu may paint over the frame's bottom rows
    # (the 21-1 scenario captures the frame with it open), so search the whole body.
    body = " ".join(content[1:])
    if not all(item in body for item in rail):
        return None
    return tuple(content)


def _sheet(title, body, rail_row, width=WIDTH, height=HEIGHT, crumb=None, tear=None):
    """An offline 80x24 screen: the decision sheet over a composer and status row."""
    region = height - 4
    crumb = crumb or f"Needs you › {title} | 1 pending"
    inner = width - 2
    top = ("┌ Decision: " + title + " ").ljust(width - 1, "─") + "┐"
    rows = [top]
    content = [crumb, body]
    for index in range(1, region - 1):
        text = content[index - 1] if index <= len(content) else ""
        if index == region - 2:
            text = rail_row
        rows.append("│" + text.ljust(inner) + "│")
    rows.append("└" + "─" * inner + "┘")
    rows += ["┌" + "─" * inner + "┐", "│" + " " * inner + "│", "└" + "─" * inner + "┘",
             "Ready".ljust(width)]
    if tear == "no-bottom":
        rows[region - 1] = " " * width
    return "\n".join(rows)


def test_frame_predicate_accepts_the_whole_sheet_and_rejects_torn_frames():
    rail = "[Enter/Esc] Continue  [n] New session"
    whole = _sheet("Session recovery", "Recovered the previous session.", rail)
    found = decision_frame(whole, "Session recovery", RECOVERY_RAIL)
    assert found is not None and found[0] == "Needs you › Session recovery | 1 pending"
    assert found[-1] == rail
    # A torn frame: the title and the first rail pair are painted, the rest of the rail is not.
    half = _sheet("Session recovery", "Recovered the previous session.", "[Enter/Esc] Continue")
    assert decision_frame(half, "Session recovery", RECOVERY_RAIL) is None
    # A frame without its closing border is never accepted.
    assert decision_frame(_sheet("Session recovery", "x", rail, tear="no-bottom"),
                          "Session recovery", RECOVERY_RAIL) is None
    # The old bottom-anchored pane title and a missing breadcrumb are rejected.
    assert decision_frame(whole.replace("Decision: ", "System · "), "Session recovery", RECOVERY_RAIL) is None
    assert decision_frame(_sheet("Session recovery", "x", rail, crumb="no crumb here"),
                          "Session recovery", RECOVERY_RAIL) is None
    # A different decision is never mistaken for this one.
    assert decision_frame(whole, "Daily budget", RECOVERY_RAIL) is None


def test_frame_predicate_holds_for_the_budget_rail_and_a_wrapped_rail_row():
    rail = "[y] Continue  [s] Switch model  [p] Pause until tomorrow  [Esc] Later"
    assert decision_frame(_sheet("Daily budget", "Daily budget ($0.00) reached.", rail),
                          "Daily budget", BUDGET_RAIL) is not None
    # At narrow widths the rail wraps by whole pairs onto two rows: the last two content
    # rows together still hold every pair.
    screen = _sheet("Daily budget", "Daily budget ($0.00) reached.", "[p] Pause until tomorrow  [Esc] Later")
    rows = screen.split("\n")
    rows[REGION - 3] = "│" + "[y] Continue  [s] Switch model".ljust(WIDTH - 2) + "│"
    assert decision_frame("\n".join(rows), "Daily budget", BUDGET_RAIL) is not None


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(400)
def test_leg_a_interim_kinds_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    if not binary.is_file():
        pytest.skip("no native binary: build `cargo build --bin rustain` and set RUSTAIN_TUI_BINARY")
    assert "debug" in binary.parts and "release" not in binary.parts, binary
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-6-a-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size,
                "viewport": [WIDTH, HEIGHT], "leg": "A",
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r216a-", dir="/tmp") as root:
            s21_1._exercise_s2(binary, evidence, Path(root), monkeypatch)
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        # The frames the scenario captured are the L3 sheet with the generated rails.
        recovery = (evidence / "21-recovery-pane.screen.txt").read_text()[:-1]
        assert decision_frame(recovery, "Session recovery", RECOVERY_RAIL), recovery
        budget = (evidence / "32-budget-pane.screen.txt").read_text()[:-1]
        assert decision_frame(budget, "Daily budget", BUDGET_RAIL), budget
        for screen in (recovery, budget):
            assert "System ·" not in screen.split("\n")[0], "no bottom-anchored pane title survives"
            assert "more waiting" not in screen
        assert s21_1._cue(budget) == 1 and s21_1._cue(recovery) == 1
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_6_A_EVIDENCE={evidence}", flush=True)


PERMISSION_RAIL = ("[y] Allow  [s] Session  [a] Always",
                   "[n] Deny  [f] Deny + feedback  [r] Review")
PERMISSION_SCENE = Path(__file__).parent / "fixtures/scenes/decision_queue_permission.json"
FIRST_COMMAND = "echo permission-first-output; touch ready; read x < gate"


def permission_frame(screen, command=FIRST_COMMAND):
    """A complete Permission sheet, with the exact body delimiter and rail rows."""
    rows = decision_frame(screen, "Permission", PERMISSION_RAIL, crumb_title="Permission Bash")
    if not rows or rows[0] != "Needs you › Permission Bash | 1 pending":
        return None
    if not all(rail in rows for rail in PERMISSION_RAIL):
        return None
    if f"│ command: {command}" not in rows:
        return None
    return rows


def test_frame_predicate_permission_exact_sheet_and_torn_controls():
    # The command is deliberately short enough to occupy exactly one body row.
    command = "ls"
    whole = _sheet("Permission", "│ command: ls", PERMISSION_RAIL[1],
                   crumb="Needs you › Permission Bash | 1 pending")
    rows = whole.split("\n")
    rows[REGION - 3] = "│" + PERMISSION_RAIL[0].ljust(WIDTH - 2) + "│"
    whole = "\n".join(rows)
    assert permission_frame(whole, command)
    assert not permission_frame(whole.replace(PERMISSION_RAIL[1], " " * len(PERMISSION_RAIL[1])), command)
    assert not permission_frame(whole.replace("│ command: ls", "command: ls  "), command)
    assert not permission_frame(whole.replace("│ command: ls", "┃ command: ls"), command)
    assert not permission_frame(whole.replace("┌ Decision: Permission", "┃ Decision: Permission"), command)
    # Unrelated transcript and toast rows outside the sheet do not affect it.
    rows = whole.split("\n")
    rows[-1] = "▶ draft  Decision waiting — Ctrl+X q".ljust(WIDTH)
    assert permission_frame("\n".join(rows), command)


def _b_wait(tui, predicate, label, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        screen = tui.get_screen_text()
        if predicate(screen):
            return screen
        assert time.monotonic() < deadline, f"{label}:\n{screen}"


def _b_stable(tui, predicate, label, duration=0.6):
    """Poll actual frames until the entire screen has stayed unchanged long enough."""
    previous, since = None, None
    deadline = time.monotonic() + 60
    while True:
        screen = tui.get_screen_text()
        now = time.monotonic()
        assert now < deadline, f"{label}:\n{screen}"
        assert predicate(screen), f"{label}:\n{screen}"
        if screen != previous:
            previous, since = screen, now
        elif now - since >= duration:
            return screen


def _b_output_row(screen):
    # A completed tool may be collapsed: its checkmarked command is still visible
    # even when the output line itself is not.
    return any("✓ Bash " + FIRST_COMMAND in row for row in screen.splitlines())


def _b_chip_only(screen):
    """One waiting decision on the status chip and neither a decision frame nor the list."""
    return s21_1._cue(screen) == 1 and "Decision:" not in screen and "┌ Needs you " not in screen


def test_b_chip_only_needs_the_chip_and_no_frame():
    status = " (daemon) [F] │ attached │ Normal │ ↑10 ↓10 │ Streaming...    Needs you (1) ^X q"
    rows = ["\u00b7\u00b7\u00b7"] + [""] * 19 + [status, "┌ Message ─┐", "│ │", "└──┘"]
    assert _b_chip_only("\n".join(rows))
    # torn / wrong frames: a frame painted, the list painted, a missing or doubled chip
    assert not _b_chip_only("\n".join(["┌ Decision: Permission ┐"] + rows))
    assert not _b_chip_only("\n".join(["┌ Needs you ──┐"] + rows))
    assert not _b_chip_only("\n".join(rows[:-2] + ["no chip row", ""]))
    assert not _b_chip_only("\n".join(rows).replace("(1)", "(2)"))


def test_b_output_row_requires_the_completed_first_tool():
    assert _b_output_row("│ ┄ ✓ Bash echo permission-first-output; touch ready; read x < gate · 1.5s")
    assert not _b_output_row("│ ┄ ● Bash echo permission-first-output; touch ready; read x < gate")
    assert not _b_output_row("│ ┄ ✓ Bash printf 'permission-second-output\\n'")


def _b_capture(evidence, name, screen):
    (evidence / f"{name}.screen.txt").write_text(screen + "\n")


def _b_send(tui, text):
    os.write(tui.child.child_fd, text.encode("utf-8"))


def _b_launch(stack, ws, env, evidence, name, attached):
    cls = _PermissionTUI if attached else s21_1._NativeTUI
    tui = cls(build=False, fresh=not attached, workspace=ws, allowed_tools=[], env_overrides=env)
    # _NativeTUI records each intermediate frame, including the one before a keypress.
    tui.screens = stack.enter_context((evidence / f"{name}.frames.jsonl").open("w"))
    tui.start()
    stack.callback(tui.stop)
    tui.child.logfile_read = stack.enter_context((evidence / f"{name}.terminal.txt").open("w"))
    return tui


def _b_world(stack, root, evidence, name, scene=PERMISSION_SCENE):
    home, ws = root / name / "home", root / name / "ws"
    home.mkdir(parents=True)
    (ws / ".rustain").mkdir(parents=True)
    (ws / ".env").write_text("")
    (ws / ".rustain/config.toml").write_text("[permissions]\nalways_tools = []\n")
    os.mkfifo(ws / "gate")
    gate = os.open(ws / "gate", os.O_RDWR | os.O_NONBLOCK)
    stack.callback(os.close, gate)
    stub = stack.enter_context(SceneStub(scene, evidence / f"{name}-requests.jsonl"))
    env = {**os.environ, **stub.env("operator", home),
           "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
           "RUSTAIN_CONFIG_DIR": str(ws / ".rustain"),
           "RUSTAIN_DATA_DIR": str(root / name / "data"),
           "RUSTAIN_LOG_PATH": str(evidence / f"{name}-runtime.log")}
    return ws, env, gate, stub


def _b_first_permission(tui, evidence, name):
    _b_send(tui, "\x1b[200~run permission scene\x1b[201~\r")
    screen = _b_wait(tui, permission_frame, f"{name}: complete permission sheet")
    _b_capture(evidence, f"{name}-permission", screen)
    _b_send(tui, "y")
    # A changed frame or a disappearing frame is a failure, not a retry.
    screen = _b_stable(tui, permission_frame, f"{name}: first y was ignored")
    _b_capture(evidence, f"{name}-first-y-ignored", screen)
    return screen


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(400)
def test_leg_b_permission_both_hosts_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    if not binary.is_file():
        pytest.skip("no native binary: build `cargo build --bin rustain` and set RUSTAIN_TUI_BINARY")
    assert "debug" in binary.parts and "release" not in binary.parts, binary
    monkeypatch.setattr(harness, "TERM_COLS", WIDTH)
    monkeypatch.setattr(harness, "TERM_ROWS", HEIGHT)
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-6-b-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size,
                "viewport": [WIDTH, HEIGHT], "leg": "B",
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r216b-", dir="/tmp") as directory, ExitStack() as stack:
            root = Path(directory)
            ws, env, gate, stub = _b_world(stack, root, evidence, "local")
            local = _b_launch(stack, ws, env, evidence, "local", False)
            _b_first_permission(local, evidence, "local")
            _b_send(local, "y")
            _b_wait(local, lambda s: (ws / "ready").exists(), "first Bash is running")
            _b_send(local, "draft kept")
            _b_wait(local, lambda s: s21_1._composer(s) == "draft kept", "draft typed before second request")
            os.write(gate, b"go\n")
            screen = _b_wait(local, lambda s: _b_output_row(s)
                             and any("Bash printf 'permission-second-output\\n'" in row
                                     for row in s.splitlines())
                             and s21_1._cue(s) == 1 and "Decision:" not in s
                             and "┌ Needs you " not in s
                             and s21_1._composer(s) == "draft kept",
                             "completed first tool, queued second permission chip and intact draft")
            _b_capture(evidence, "local-queued", screen)
            _b_send(local, "\x18q")
            screen = _b_wait(local, lambda s: "┌ Needs you " in s
                             and any("Permission · Bash" in row for row in s.splitlines())
                             and "[Enter] Open" in s, "Needs-you list")
            _b_capture(evidence, "local-list", screen)
            _b_stable(local, lambda s: "┌ Needs you " in s
                      and any("▸ Permission · Bash" in row for row in s.splitlines())
                      and "[Enter] Open  [Esc] Back  [Up/Down] Select" in s,
                      "Needs-you list beyond its inert window")
            _b_send(local, "\r")
            second = "printf 'permission-second-output\\n'"
            screen = _b_wait(local, lambda s: permission_frame(s, second), "second permission sheet")
            _b_stable(local, lambda s: permission_frame(s, second), "second permission inert window")
            _b_capture(evidence, "local-second-permission", screen)
            _b_send(local, "f")
            _b_wait(local, lambda s: "Feedback:" in s and "[Enter] Send" in s
                    and "[Esc] Back" in s, "feedback field")
            _b_send(local, "use rg")
            screen = _b_wait(local, lambda s: "Feedback: use rg" in s, "feedback typed")
            _b_capture(evidence, "local-feedback", screen)
            _b_send(local, "\r")
            screen = _b_wait(local, lambda s: any('Tool denied. User feedback: \"use rg\"' in row
                                                 for row in s.splitlines()), "denial transcript")
            _b_capture(evidence, "local-denied", screen)
            assert s21_1._composer(screen) == "draft kept"
            assert not any(row.get("desync") for row in stub.rows()), stub.rows()

            ws, env, gate, stub = _b_world(stack, root, evidence, "attached")
            _permission_daemon(stack, ws, env, root)
            writer = _b_launch(stack, ws, env, evidence, "writer", True)
            _b_first_permission(writer, evidence, "writer")
            mirror = _b_launch(stack, ws, env, evidence, "mirror", True)
            screen = _b_wait(mirror, lambda s: "read-only" in harness.status_row(s),
                             "read-only mirror attached while writer permission is pending")
            assert "Decision:" not in screen and all(rail not in screen for rail in PERMISSION_RAIL), screen
            assert "[r] Review" not in screen and "[y] Allow" not in screen, screen
            _b_capture(evidence, "mirror-pending", screen)
            _b_send(mirror, "y")
            screen = _b_stable(mirror, lambda s: "read-only" in harness.status_row(s)
                               and "Decision:" not in s
                               and all(rail not in s for rail in PERMISSION_RAIL)
                               and "[r] Review" not in s and "[y] Allow" not in s
                               # A read-only composer types by design (19.16g): `y` lands there, never decides.
                               and s21_1._composer(s) == "y",
                               "mirror cannot answer or send the pending permission")
            _b_capture(evidence, "mirror-after-y", screen)
            _b_stable(writer, permission_frame, "mirror key leaves writer permission unchanged")
            _b_send(writer, "y")
            _b_wait(writer, lambda s: (ws / "ready").exists(), "attached Bash is running")
            os.write(gate, b"go\n")
            # The first tool ran (the model's turn-2 request carries its output) and the
            # second request waits as a chip only: the attached transcript paints no tool
            # rows in this phase, so the proof is the daemon-side request plus the chip.
            screen = _b_wait(writer, _b_chip_only, "attached second request waits as a chip")
            assert any("permission-first-output" in json.dumps(row) for row in stub.rows()), stub.rows()
            _b_capture(evidence, "writer-output", screen)
            assert not any(row.get("desync") for row in stub.rows()), stub.rows()
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_6_B_EVIDENCE={evidence}", flush=True)


# ── Leg C (slice 6-C): AskUserQuestion is the `Decision: Question` frame ──────────────

QUESTION_SCENE = Path(__file__).parent / "fixtures/scenes/decision_queue_question.json"
QUESTION_TEXT = "Which colour do you prefer?"
QUESTION_EDITING = ("[Enter] Send", "[Ctrl+D] Skip question", "[Esc] Back")
QUESTION_IDLE = ("[Enter] Answer", "[Ctrl+D] Skip question", "[Esc] Later")
QUESTION_CRUMB = re.compile(r"Needs you › Question( .*)? \| \d+ pending")
FIELD_ROW = re.compile(r"(?:▶ )?Answer: (.*)")
REPLY_ROW = re.compile(r"(?:^|[ │▶●])Colour acknowledged: blue[ │]*$")
FULL_SHEET_BELOW = 100  # the frame is the full-width sheet below this many columns


def _c_box(screen, title):
    """The content rows (stripped, non-empty) of the bordered `Decision: <title>` box, or None
    while it is torn: every border painted, the box inside the screen, and below 100 columns
    exactly the full-width sheet. Rows outside the box (toasts, transcript) are ignored."""
    rows = screen.split("\n")
    opener = f"┌ Decision: {title} "
    for top, row in enumerate(rows):
        left = row.find(opener)
        if left < 0:
            continue
        right = row.rfind("┐")
        if right <= left + len(opener) or set(row[left + len(opener):right]) - {"─"}:
            return None
        if len(row) < FULL_SHEET_BELOW and (left != 0 or right != len(row) - 1):
            return None
        bottom = next((i for i in range(top + 1, len(rows))
                       if rows[i][left:left + 1] == "└"), None)
        if bottom is None:
            return None
        closing = rows[bottom]
        if len(closing) <= right or closing[right] != "┘" or set(closing[left + 1:right]) != {"─"}:
            return None
        content = []
        for inner in rows[top + 1:bottom]:
            if len(inner) <= right or inner[left] != "│" or inner[right] != "│":
                return None
            text = inner[left + 1:right].strip()
            if text:
                content.append(text)
        return content or None
    return None


def question_frame(screen, answer=None, editing=True):
    """A complete Question sheet: breadcrumb, the question, the `Answer:` field row and the whole
    generated rail under the body (it may wrap by whole pairs onto two rows). `answer` pins the
    field text; the caret `▋` is painted only while editing. Returns the body rows or None."""
    content = _c_box(screen, "Question")
    if not content or not QUESTION_CRUMB.fullmatch(content[0]):
        return None
    body = content[1:]
    rail = QUESTION_EDITING if editing else QUESTION_IDLE
    if len(body) < 3 or not all(item in " ".join(body[-2:]) for item in rail):
        return None
    if not body[-1].endswith(rail[-1]) or not any(QUESTION_TEXT in row for row in body[:-1]):
        return None
    fields = [m.group(1) for m in map(FIELD_ROW.fullmatch, body[:-1]) if m]
    if len(fields) != 1:
        return None
    if answer is not None:
        if editing and fields[0] != answer + "▋":
            return None
        if not editing and fields[0].rstrip("▋") != answer:
            return None
    return tuple(body)


def _c_deferred(screen):
    """Chip `Needs you (1) ^X q` while the turn still waits on its tool (the real status row reads
    `Executing tools...`; `Streaming` is also a live turn); no frame and no list painted."""
    status = harness.status_row(screen)
    live = "Executing tools" in status or "Streaming" in status
    return ("Needs you (1) ^X q" in status and live
            and "Decision:" not in screen and "┌ Needs you " not in screen)


def _c_listed(screen):
    rows = screen.split("\n")
    return (any(row.startswith("┌ Needs you ") for row in rows)
            and any("▸ Question" in row for row in rows)
            and "[Enter] Open  [Esc] Back  [Up/Down] Select" in screen)


def _c_reply(screen):
    return "Decision:" not in screen and any(REPLY_ROW.search(row) for row in screen.split("\n"))


def _c_sheet(width, height, answer="blue", editing=True, tear=None, rail_rows=None, marker=False):
    """An offline synthetic Question screen: the sheet (full width below 100 columns, a centred
    100-column box above) over a three-row composer and a status row."""
    box = width if width < FULL_SHEET_BELOW else 100
    left = (width - box) // 2
    if tear == "shifted":
        box, left = width - 1, 1
    region, inner = height - 4, box - 2
    field = ("▶ " if marker else "") + f"Answer: {answer}" + ("▋" if editing and tear != "no-caret" else "")
    rail = "  ".join(QUESTION_EDITING if editing else QUESTION_IDLE)
    if tear == "idle-rail":
        rail = "  ".join(QUESTION_IDLE if editing else QUESTION_EDITING)
    body = ["Needs you › Question Which colour… | 1 pending", QUESTION_TEXT, field]
    if tear == "no-crumb":
        body[0] = "no crumb here"
    if tear == "no-field":
        body[2] = ""
    interior = body + [""] * (region - 2 - len(body) - len(rail_rows or [rail]))
    interior += list(rail_rows or [rail])
    if tear == "half-rail":
        interior[-1] = "[Enter] Send"
    box_rows = [("┌ Decision: Question " + "─" * inner)[:inner + 1] + "┐"]
    for index, text in enumerate(interior):
        right = "│"
        if tear == "right-border" and index == 1:
            right = " "
        box_rows.append("│" + text.ljust(inner) + right)
    box_rows.append("└" + "─" * inner + "┘")
    if tear == "no-bottom":
        box_rows[-1] = " " * box
    if tear == "no-top-border":
        box_rows[0] = "┌ Decision: Question " + "x" * (inner - 20) + "┐"
    rows = [" " * left + row + " " * (width - left - box) for row in box_rows]
    rows += ["┌" + "─" * (width - 2) + "┐", "│" + " " * (width - 2) + "│", "└" + "─" * (width - 2) + "┘",
             " anthropic/claude-sonnet-4-6 [F] │ Normal │ Streaming...  Needs you (1) ^X q".ljust(width)[:width]]
    assert len(rows) == height and all(len(row) == width for row in rows), (width, height, len(rows))
    return "\n".join(rows)


SIZES = ((60, 16), (80, 24), (130, 30))


def test_c_question_predicate_accepts_the_whole_sheet_at_every_size():
    for width, height in SIZES:
        whole = _c_sheet(width, height)
        body = question_frame(whole, "blue")
        assert body is not None, whole
        assert body[-1] == "  ".join(QUESTION_EDITING) and "Answer: blue▋" in body
        assert question_frame(whole, None) is not None and question_frame(whole, "") is None
        idle = _c_sheet(width, height, editing=False)
        assert question_frame(idle, "blue", editing=False) is not None
        assert question_frame(idle, "blue", editing=True) is None
        # The rail wrapped by whole pairs onto two rows is still the whole rail.
        wrapped = _c_sheet(width, height, rail_rows=["[Enter] Send  [Ctrl+D] Skip question", "[Esc] Back"])
        assert question_frame(wrapped, "blue") is not None
        # A `▶ ` marker before the field row and toast rows outside the box change nothing.
        marked = _c_sheet(width, height, marker=True)
        assert question_frame(marked, "blue") is not None
        rows = whole.split("\n")
        rows[-1] = "Info  Decision waiting — Ctrl+X q".ljust(width)
        assert question_frame("\n".join(rows), "blue") is not None


def test_c_question_predicate_rejects_torn_frames_at_every_size():
    for width, height in SIZES:
        for tear in ("half-rail", "no-bottom", "no-crumb", "no-field", "no-caret", "idle-rail",
                     "right-border", "no-top-border"):
            torn = _c_sheet(width, height, tear=tear)
            assert question_frame(torn, "blue") is None, (width, height, tear, torn)
        # The title and the first rail pair painted, the rest of the rail not yet.
        half = _c_sheet(width, height, rail_rows=["[Enter] Send"])
        assert "┌ Decision: Question " in half and question_frame(half, "blue") is None, half
        # Wrong answer text and a stale (non-blue) field are never accepted.
        assert question_frame(_c_sheet(width, height, answer="blu"), "blue") is None
        assert question_frame(_c_sheet(width, height, answer="blue "), "blue") is None
        # Below 100 columns only the full-width sheet counts; the field must also be present once.
        if width < FULL_SHEET_BELOW:
            assert question_frame(_c_sheet(width, height, tear="shifted"), "blue") is None
        two = _c_sheet(width, height).split("\n")
        two[2] = two[2].replace(QUESTION_TEXT, "Answer: x".ljust(len(QUESTION_TEXT)))
        assert question_frame("\n".join(two), "blue") is None
        # A different decision's frame is never mistaken for a question.
        assert question_frame(_c_sheet(width, height).replace("Question", "Permission"), "blue") is None


def test_c_deferred_list_and_reply_predicates():
    rows = ["···"] + [""] * 19 + [" anthropic/claud [F] │ Normal │ Executing tools... │ █ coding Needs you (1) ^X q",
                                  "┌ Message ─┐", "│ │", "└──┘"]
    assert _c_deferred("\n".join(rows))
    assert _c_deferred("\n".join(rows).replace("Executing tools...", "Streaming..."))
    assert not _c_deferred("\n".join(rows).replace("Executing tools...", "Ready"))
    assert not _c_deferred("\n".join(rows).replace("(1)", "(2)"))
    assert not _c_deferred("\n".join(["┌ Decision: Question ┐"] + rows))
    assert not _c_deferred("\n".join(["┌ Needs you ──┐"] + rows))
    listed = ["┌ Needs you ─┐", "│ ▸ Question · Which colour… │", "│ [Enter] Open  [Esc] Back  [Up/Down] Select │"]
    assert _c_listed("\n".join(listed))
    assert not _c_listed("\n".join(listed[:2]))
    assert not _c_listed("\n".join(listed).replace("▸ Question", "▸ Permission"))
    assert _c_reply("│ Colour acknowledged: blue")
    assert _c_reply("▶ Colour acknowledged: blue")
    assert not _c_reply("│ Colour acknowledged: blue and more")
    assert not _c_reply("┌ Decision: Question ┐\n│ Colour acknowledged: blue")


@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(400)
def test_leg_c_question_compose_80x24(monkeypatch):
    binary = harness._resolve_binary()[0].resolve()
    if not binary.is_file():
        pytest.skip("no native binary: build `cargo build --bin rustain` and set RUSTAIN_TUI_BINARY")
    assert "debug" in binary.parts and "release" not in binary.parts, binary
    monkeypatch.setattr(harness, "TERM_COLS", WIDTH)
    monkeypatch.setattr(harness, "TERM_ROWS", HEIGHT)
    parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir())).resolve()
    parent.mkdir(parents=True, exist_ok=True)
    evidence = Path(tempfile.mkdtemp(prefix="21-6-c-", dir=parent))
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    manifest = {"binary": str(binary), "sha256": digest, "bytes": binary.stat().st_size,
                "viewport": [WIDTH, HEIGHT], "leg": "C",
                "started_at": datetime.now(timezone.utc).isoformat(), "result": "RUNNING"}
    try:
        with tempfile.TemporaryDirectory(prefix="r216c-", dir="/tmp") as directory, ExitStack() as stack:
            root = Path(directory)
            ws, env, gate, stub = _b_world(stack, root, evidence, "local", QUESTION_SCENE)
            tui = _b_launch(stack, ws, env, evidence, "local", False)
            _b_send(tui, "\x1b[200~ask the colour question\x1b[201~\r")
            screen = _b_wait(tui, lambda s: question_frame(s, ""), "question frame with an empty field")
            _b_capture(evidence, "local-question", screen)
            # Keys (the field included) wait for the frame's inert window; typing starts on a settled frame.
            _b_stable(tui, lambda s: question_frame(s, ""), "question frame beyond its inert window")
            _b_send(tui, "blue")
            screen = _b_wait(tui, lambda s: question_frame(s, "blue") and s21_1._composer(s) == "",
                             "answer typed into the field, composer row empty")
            _b_capture(evidence, "local-typed", screen)
            screen = _b_stable(tui, lambda s: question_frame(s, "blue") and s21_1._composer(s) == "",
                               "typed answer stays in the field")
            # First Esc leaves the field (the frame stays), the second defers the question.
            _b_send(tui, "\x1b")
            screen = _b_wait(tui, lambda s: question_frame(s, "blue", editing=False)
                             and s21_1._composer(s) == "", "first Esc leaves the field")
            _b_capture(evidence, "local-left-field", screen)
            _b_send(tui, "\x1b")
            screen = _b_wait(tui, lambda s: _c_deferred(s) and s21_1._composer(s) == "",
                             "second Esc defers: chip only, the turn keeps streaming")
            _b_capture(evidence, "local-deferred", screen)
            assert not any(REPLY_ROW.search(row) for row in screen.split("\n")), screen
            _b_send(tui, "\x18q")
            _b_wait(tui, _c_listed, "Needs-you list with the question")
            _b_stable(tui, _c_listed, "Needs-you list beyond its inert window")
            _b_send(tui, "\r")
            either = lambda s: question_frame(s, "blue", True) or question_frame(s, "blue", False)
            screen = _b_wait(tui, either, "reopened question with the field still `blue`")
            _b_capture(evidence, "local-reopened", screen)
            assert s21_1._composer(screen) == ""
            screen = _b_stable(tui, either, "reopened question beyond its inert window")
            _b_send(tui, "\r")
            # A reopened, not-editing question may take one more Enter to start editing before it sends.
            screen = _b_wait(tui, lambda s: _c_reply(s) or question_frame(s, "blue", True),
                             "answer sent or field editing again")
            if not _c_reply(screen):
                _b_send(tui, "\r")
                screen = _b_wait(tui, _c_reply, "the scene's reply names the answer")
            _b_capture(evidence, "local-answered", screen)
            assert any("blue" in json.dumps(row) for row in stub.rows() if row.get("tool_results")), stub.rows()
            assert not any(row.get("desync") for row in stub.rows()), stub.rows()
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
        manifest["result"] = "PASS"
    except BaseException:
        manifest["result"] = "FAIL"
        (evidence / "failure.txt").write_text(traceback.format_exc())
        raise
    finally:
        manifest["completed_at"] = datetime.now(timezone.utc).isoformat()
        (evidence / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"NATIVE_21_6_C_EVIDENCE={evidence}", flush=True)
