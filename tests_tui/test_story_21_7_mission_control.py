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

Leg E (slice 7-E, 80x24): two saved sessions; the launchpad key line names `^X h History`;
`Ctrl+X h` paints the `History` frame with both titles and the rail; `j` then `Enter` opens the
second (older) session and its transcript row is on the Stage with the frame gone.

Leg F (slice 7-F, 130x30, declared: the monitor exists only from 120 columns): a saved session with
a plan; `Ctrl+X w` paints the right column titled `Monitor · Tasks` with the task rows beside the
intact main column; a second `Ctrl+X w` takes it away. A second process at 80x24: `Ctrl+X w`
answers the width remedy toast and paints no column.

Leg G (slice 7-G, 80x24 with a 60x16 part): a saved session with a plan; the strip row directly
above the status row reads `Tasks 2/3` (two of the seeded three tasks are done; the A22 grammar
anchored by `harness.strip_row`); `Ctrl+X w` (the width remedy toast) paints the toast on the row
directly above the strip, found by the strip-aware `harness.toast_rows`. A second process at
60x16 with an eight-line draft: the strip yields its row (none painted) and `Ctrl+X h` still paints
the five-row History frame with the status row directly below it.

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
from keys import ALT_ENTER, CTRL_X

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
    tab_row = re.fullmatch(r"\s*│ Agents   Tasks  ▸Messages\b.*│\s*", rows[top + 1]) is not None
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

AGENTS_TITLE = "Mission Control · Agents"
EMPTY_COPY = "No agents active."


def agents_frame_ok(screen):
    """Agents title, the marked Agents tab row directly below it, the empty-state copy inside the
    frame, the rail with the way out last inside it and the status row intact below."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, AGENTS_TITLE)
    if top is None or bottom is None or bottom < top + 3:
        return False
    tab_row = re.fullmatch(r"\s*│▸Agents\b.*│\s*", rows[top + 1]) is not None
    body = any(re.fullmatch(r".*│ *" + re.escape(EMPTY_COPY) + r".*│\s*", row) for row in rows[top + 2:bottom - 1])
    rail = re.fullmatch(r"\s*│.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return tab_row and body and rail and status


def adapters_frame_ok(screen):
    """Adapters title, a port row inside the frame, the rail with the way out last, status below."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, "Adapters")
    if top is None or bottom is None or bottom < top + 3:
        return False
    body = any(re.fullmatch(r".*│.*\bpersona\b.*│\s*", row) for row in rows[top + 1:bottom - 1])
    rail = re.fullmatch(r"\s*│.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return body and rail and status


def torn_agents(screen):
    controls = _torn_frame(screen, AGENTS_TITLE, EMPTY_COPY, "Needs you (1)",
                           {"tab row unmarked": ("▸Agents", " Agents")})
    rows = _rows(screen)
    index = next(i for i, row in enumerate(rows) if EMPTY_COPY in row)
    controls["cut mid-row"] = "\n".join(rows[:index] + [rows[index][: rows[index].index(EMPTY_COPY) + 6]] + rows[index + 1:])
    top, _ = _frame_bounds(rows, AGENTS_TITLE)
    controls["tab row missing"] = "\n".join(rows[:top + 1] + rows[top + 2:])
    return controls


def torn_adapters(screen):
    return _torn_frame(screen, "Adapters", "persona", "Needs you (1)", {})


TASKS_TITLE = "Mission Control · Tasks"
TASK_ROW = "Review result"
TASK_RESULT = "Reviewed OK"


def tasks_frame_ok(screen):
    """Tasks title, the marked Tasks tab row directly below it, the seeded task row inside the
    frame, the rail with the way out last and the status row intact below."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, TASKS_TITLE)
    if top is None or bottom is None or bottom < top + 3:
        return False
    tab_row = re.fullmatch(r"\s*│ Agents  ▸Tasks\b.*│\s*", rows[top + 1]) is not None
    body = any(re.fullmatch(r".*│.*\b2\. .*" + TASK_ROW + r".*│\s*", row) for row in rows[top + 2:bottom - 1])
    rail = re.fullmatch(r"\s*│.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return tab_row and body and rail and status


def task_window_ok(screen):
    """Title `Task 2 · <title>`, the rendered result row inside the frame, the rail with the copy
    hint and the way out, status below; the result is rendered text (no markdown markers)."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, f"Task 2 · {TASK_ROW}")
    if top is None or bottom is None or bottom < top + 2:
        return False
    body = any(re.fullmatch(r".*│ *" + TASK_RESULT + r" *│\s*", row) for row in rows[top + 1:bottom - 1])
    rail = re.fullmatch(r"\s*│.*\[c\] Copy.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    status = any(STATUS in row for row in rows[bottom + 1:])
    return body and rail and status


def torn_tasks(screen):
    controls = _torn_frame(screen, TASKS_TITLE, TASK_ROW, "Needs you (1)",
                           {"tab row unmarked": ("▸Tasks", " Tasks")})
    rows = _rows(screen)
    index = next(i for i, row in enumerate(rows) if TASK_ROW in row)
    controls["cut mid-row"] = "\n".join(rows[:index] + [rows[index][: rows[index].index(TASK_ROW) + 4]] + rows[index + 1:])
    return controls


def torn_task_window(screen):
    return _torn_frame(screen, f"Task 2 · {TASK_ROW}", TASK_RESULT, "Needs you (1)", {})


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


def torn_history(screen):
    return _torn_frame(screen, HISTORY_TITLE, HISTORY_B, "Needs you (1)", {})


def torn_launchpad(screen):
    rows = _rows(screen)
    index = next(i for i, row in enumerate(rows) if "^X h History" in row)
    return {"key line cut": "\n".join(rows[:index] + [rows[index][: rows[index].index("History")]] + rows[index + 1:]),
            "key line missing": "\n".join(rows[:index] + rows[index + 1:])}


def transcript_ok(screen):
    return HISTORY_ROW in screen and _frame_top(_rows(screen), HISTORY_TITLE) is None


MONITOR_TITLE = "Monitor · Tasks"
MONITOR_REMEDY_HEAD = "Info The monitor needs a terminal at least 120"


def _monitor_bounds(rows):
    """(title row, bottom row, left border x) of the monitor column, else (None, None, None): the
    title row ends with `┌Monitor · Tasks──┐` and the bottom border `└──┘` sits at the same x."""
    for top, row in enumerate(rows):
        if re.fullmatch(r".*┌" + re.escape(MONITOR_TITLE) + r"─*┐", row):
            x = row.rindex("┌")
            for bottom in range(top + 1, len(rows)):
                if len(rows[bottom]) > x and re.fullmatch(r"└─*┘", rows[bottom][x:]):
                    return top, bottom, x
            return top, None, x
    return None, None, None


def monitor_column_ok(screen):
    """The column's title row, every row down to its bottom border closed by `│` at the left
    border's x and at the right edge, the seeded task row inside the column, the main column's
    status row beside it, and no Mission Control frame or rail anywhere."""
    rows = _rows(screen)
    top, bottom, x = _monitor_bounds(rows)
    if top is None or bottom is None or bottom < top + 4:
        return False
    inside = rows[top + 1:bottom]
    closed = all(len(row) > x and row[x] == "│" and row.endswith("│") for row in inside)
    task = any(re.fullmatch(r".*│[^│]*\b2\. .*" + TASK_ROW + r".*│", row) for row in inside)
    status = any(STATUS in row[:x] for row in inside)
    clean = "Mission Control" not in screen and "[Esc] Back" not in screen
    return closed and task and status and clean


def monitor_off_ok(screen):
    """No monitor column (no `Monitor ·` title anywhere) with the status row intact."""
    return "Monitor ·" not in screen and any(STATUS in row for row in _rows(screen))


def monitor_remedy_ok(screen):
    """The width remedy toast (cut by the row with `…` at 80 columns) on its own row, the status
    row intact below it, and no column."""
    rows = _rows(screen)
    toast = any(re.fullmatch(r"\s*" + re.escape(MONITOR_REMEDY_HEAD) + r".*", row) for row in rows)
    return toast and monitor_off_ok(screen)


def torn_monitor(screen):
    rows = _rows(screen)
    top, bottom, x = _monitor_bounds(rows)
    task = next(i for i, row in enumerate(rows) if TASK_ROW in row)
    status = next(i for i, row in enumerate(rows) if STATUS in row)
    controls = {
        "title cut mid-row": "\n".join(rows[:top] + [rows[top][: x + 9]] + rows[top + 1:]),
        "title swapped": "\n".join(rows[:top] + [rows[top].replace("Tasks", "Room ")] + rows[top + 1:]),
        "task row missing": "\n".join(rows[:task] + [rows[task].replace(TASK_ROW, " " * len(TASK_ROW))] + rows[task + 1:]),
        "task row cut mid-row": "\n".join(rows[:task] + [rows[task][: rows[task].index(TASK_ROW) + 4]] + rows[task + 1:]),
        "status row missing": "\n".join(rows[:status] + rows[status + 1:]),
        "right border missing": "\n".join(rows[:top + 2] + [rows[top + 2].rstrip("│").rstrip()] + rows[top + 3:]),
        "bottom border missing": "\n".join(rows[:bottom] + rows[bottom + 1:]),
        "hub frame painted over": screen + "\n┌Mission Control · Tasks─┐",
    }
    return controls


def torn_remedy(screen):
    rows = _rows(screen)
    toast = next(i for i, row in enumerate(rows) if MONITOR_REMEDY_HEAD in row)
    status = next(i for i, row in enumerate(rows) if STATUS in row)
    return {
        "toast cut mid-row": "\n".join(rows[:toast] + [rows[toast][: rows[toast].index("The monitor") + 11]] + rows[toast + 1:]),
        "toast missing": "\n".join(rows[:toast] + rows[toast + 1:]),
        "status row missing": "\n".join(rows[:status] + rows[status + 1:]),
        "a column painted": "\n".join(rows + ["    ┌Monitor · Tasks──┐"]),
    }


STRIP_TASKS = "Tasks 2/3"
STRIP_IDLE = "^X s Mission Control"


def strip_row_ok(screen, text):
    """The strip row reads exactly `text` (the A22 grammar, found by the harness helper) directly
    above the status row, which is directly above the composer."""
    rows = _rows(screen)
    top = harness.composer_top(rows)
    return (bool(top) and top >= 2 and rows[top - 2] == text and STATUS in rows[top - 1]
            and harness.strip_row(screen) == text)


def strip_toast_ok(screen):
    """The strip with the width remedy toast on the row directly above it (the strip-aware
    `harness.toast_rows`), the status row intact."""
    toasts = harness.toast_rows(screen)
    return (strip_row_ok(screen, STRIP_TASKS) and len(toasts) == 1
            and re.fullmatch(r"\s*" + re.escape(MONITOR_REMEDY_HEAD) + r".*", toasts[0]) is not None)


def strip_yield_ok(screen):
    """No strip row; the five-row `History` frame whose bottom border is directly above the status
    row, with the rail row (`[Esc] Back`) inside it."""
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, HISTORY_TITLE)
    if top is None or bottom is None or bottom - top != 4 or harness.strip_row(screen) != "":
        return False
    rail = re.fullmatch(r"\s*│.*\[Esc\] Back\s*│\s*", rows[bottom - 1]) is not None
    body = re.fullmatch(r"│.*\S.*│", rows[top + 1]) is not None
    return rail and body and STATUS in rows[bottom + 1]


def torn_strip(screen, text):
    rows = _rows(screen)
    top = harness.composer_top(rows)
    strip, status = top - 2, top - 1
    return {
        "strip cut mid-row": "\n".join(rows[:strip] + [text[: len(text) // 2]] + rows[strip + 1:]),
        "strip missing": "\n".join(rows[:strip] + [""] + rows[strip + 1:]),
        "status row missing": "\n".join(rows[:status] + rows[status + 1:]),
        "strip and status swapped": "\n".join(rows[:strip] + [rows[status], rows[strip]] + rows[status + 1:]),
        "strip text changed": "\n".join(rows[:strip] + [text.replace("2/3", "9/9").replace("Mission", "Mess")] + rows[strip + 1:]),
    }


def torn_strip_toast(screen):
    rows = _rows(screen)
    top = harness.composer_top(rows)
    strip, toast = top - 2, top - 3
    return {
        "toast over the strip": "\n".join(rows[:strip] + [rows[toast]] + rows[strip + 1:]),
        "toast under the strip": "\n".join(rows[:toast] + [rows[strip], rows[toast]] + rows[strip + 1:]),
        "toast missing": "\n".join(rows[:toast] + [""] + rows[toast + 1:]),
        "toast cut mid-row": "\n".join(rows[:toast] + [rows[toast][: rows[toast].index("The monitor") + 11]] + rows[toast + 1:]),
    }


def torn_strip_yield(screen):
    rows = _rows(screen)
    top, bottom = _frame_bounds(rows, HISTORY_TITLE)
    status = bottom + 1
    return {
        "a strip row painted over the frame bottom": "\n".join(rows[:bottom] + [STRIP_TASKS] + rows[bottom + 1:]),
        "frame cut mid-row": "\n".join(rows[:top + 1] + [rows[top + 1][: len(rows[top + 1]) // 2]] + rows[top + 2:]),
        "status row missing": "\n".join(rows[:status] + rows[status + 1:]),
        "rail half painted": "\n".join(row.replace("[Esc] Back", "[Esc") for row in rows),
    }


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
    for width, height in SIZES:
        name = f"{width}x{height}"
        agents = (frames / f"agents-open-{name}.txt").read_text(encoding="utf-8")
        adapters = (frames / f"adapters-open-{name}.txt").read_text(encoding="utf-8")
        before = (frames / f"stage-before-{name}.txt").read_text(encoding="utf-8")
        room = (frames / f"room-open-{name}.txt").read_text(encoding="utf-8")
        assert agents_frame_ok(agents), f"{name}: the Agents tab must pass\n{agents}"
        assert adapters_frame_ok(adapters), f"{name}: the Adapters window must pass\n{adapters}"
        for label, frame in (("the Stage", before), ("the Room frame", room)):
            assert not agents_frame_ok(frame) and not adapters_frame_ok(frame), f"{name}: absent from {label}"
        assert not agents_frame_ok(adapters) and not adapters_frame_ok(agents), name
        for label, torn in torn_agents(agents).items():
            assert not agents_frame_ok(torn), f"{name}: torn Agents control `{label}` must fail:\n{torn}"
        for label, torn in torn_adapters(adapters).items():
            assert not adapters_frame_ok(torn), f"{name}: torn Adapters control `{label}` must fail:\n{torn}"
    for width, height in SIZES:
        name = f"{width}x{height}"
        tasks = (frames / f"tasks-open-{name}.txt").read_text(encoding="utf-8")
        window = (frames / f"task-open-{name}.txt").read_text(encoding="utf-8")
        before = (frames / f"stage-before-{name}.txt").read_text(encoding="utf-8")
        room = (frames / f"room-open-{name}.txt").read_text(encoding="utf-8")
        assert tasks_frame_ok(tasks), f"{name}: the Tasks tab must pass\n{tasks}"
        assert task_window_ok(window), f"{name}: the task window must pass\n{window}"
        for label, frame in (("the Stage", before), ("the Room frame", room)):
            assert not tasks_frame_ok(frame) and not task_window_ok(frame), f"{name}: absent from {label}"
        assert not tasks_frame_ok(window) and not task_window_ok(tasks), name
        for label, torn in torn_tasks(tasks).items():
            assert not tasks_frame_ok(torn), f"{name}: torn Tasks control `{label}` must fail:\n{torn}"
        for label, torn in torn_task_window(window).items():
            assert not task_window_ok(torn), f"{name}: torn window control `{label}` must fail:\n{torn}"
    for width, height in SIZES:
        name = f"{width}x{height}"
        launch = (frames / f"launchpad-{name}.txt").read_text(encoding="utf-8")
        history = (frames / f"history-open-{name}.txt").read_text(encoding="utf-8")
        before = (frames / f"stage-before-{name}.txt").read_text(encoding="utf-8")
        assert launchpad_key_ok(launch), f"{name}: the launchpad key line must name History\n{launch}"
        assert history_frame_ok(history), f"{name}: the History frame must pass\n{history}"
        for label, frame in (("the Stage", before), ("the launchpad", launch)):
            assert not history_frame_ok(frame), f"{name}: History is absent from {label}"
        assert not launchpad_key_ok(history) and not launchpad_key_ok(before), name
        for label, torn in torn_history(history).items():
            assert not history_frame_ok(torn), f"{name}: torn History control `{label}` must fail:\n{torn}"
        for label, torn in torn_launchpad(launch).items():
            assert not launchpad_key_ok(torn), f"{name}: torn launchpad control `{label}` must fail:\n{torn}"
    for width, height in SIZES:
        name = f"{width}x{height}"
        off = (frames / f"monitor-off-{name}.txt").read_text(encoding="utf-8")
        on = (frames / f"monitor-on-{name}.txt").read_text(encoding="utf-8")
        history = (frames / f"history-open-{name}.txt").read_text(encoding="utf-8")
        assert monitor_off_ok(off), f"{name}: the monitor-off frame must pass\n{off}"
        assert not monitor_column_ok(off) and not monitor_remedy_ok(off), name
        if width >= 120:
            toggled = (frames / f"monitor-toggled-off-{name}.txt").read_text(encoding="utf-8")
            assert monitor_column_ok(on), f"{name}: the monitor column must pass\n{on}"
            assert not monitor_off_ok(on) and not monitor_remedy_ok(on), name
            assert monitor_off_ok(toggled) and not monitor_column_ok(toggled), name
            for label, torn in torn_monitor(on).items():
                assert not monitor_column_ok(torn), f"{name}: torn monitor control `{label}` must fail:\n{torn}"
        else:
            assert monitor_remedy_ok(on), f"{name}: the width remedy must pass\n{on}"
            assert monitor_off_ok(on) and not monitor_column_ok(on), name
            for label, torn in torn_remedy(on).items():
                assert not monitor_remedy_ok(torn), f"{name}: torn remedy control `{label}` must fail:\n{torn}"
        assert not monitor_column_ok(history) and not monitor_remedy_ok(history), name
    for width, height in SIZES:
        name = f"{width}x{height}"
        idle = (frames / f"strip-idle-{name}.txt").read_text(encoding="utf-8")
        plan = (frames / f"strip-plan-{name}.txt").read_text(encoding="utf-8")
        assert strip_row_ok(idle, STRIP_IDLE), f"{name}: the idle hint must pass\n{idle}"
        assert strip_row_ok(plan, STRIP_TASKS), f"{name}: the plan strip must pass\n{plan}"
        assert not strip_row_ok(idle, STRIP_TASKS) and not strip_row_ok(plan, STRIP_IDLE), name
        for text, frame in ((STRIP_IDLE, idle), (STRIP_TASKS, plan)):
            for label, torn in torn_strip(frame, text).items():
                assert not strip_row_ok(torn, text), f"{name}: torn strip control `{label}` must fail:\n{torn}"
        if width < 120:
            toast = (frames / f"strip-toast-{name}.txt").read_text(encoding="utf-8")
            assert strip_toast_ok(toast), f"{name}: the toast above the strip must pass\n{toast}"
            assert not strip_toast_ok(plan), f"{name}: no toast on the plain frame"
            for label, torn in torn_strip_toast(toast).items():
                assert not strip_toast_ok(torn), f"{name}: torn toast control `{label}` must fail:\n{torn}"
    yielded = (frames / "strip-yield-60x16.txt").read_text(encoding="utf-8")
    assert strip_yield_ok(yielded), f"60x16: the yielded strip with the five-row frame must pass\n{yielded}"
    assert not strip_yield_ok((frames / "strip-plan-60x16.txt").read_text(encoding="utf-8"))
    for label, torn in torn_strip_yield(yielded).items():
        assert not strip_yield_ok(torn), f"60x16: torn yield control `{label}` must fail:\n{torn}"
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


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_c_agents_tab_and_adapters_window_at_80x24(monkeypatch):
    """Leg C (7-C): `Ctrl+X s` -> the Agents tab with its empty-state copy (no API, no agents);
    `Esc`; `Ctrl+X a` -> the Adapters window with the port rows. Hermetic, no sleeps."""
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    with tempfile.TemporaryDirectory(prefix="21-7-c-") as root:
        root = Path(root)
        workspace = root / "workspace"
        (workspace / ".rustain").mkdir(parents=True)
        home = root / "home"
        home.mkdir()
        data = root / "data"
        data.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file",
               "RUSTAIN_DATA_DIR": str(data)}
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-c"
        evidence.mkdir(parents=True, exist_ok=True)
        binary = str(harness._resolve_binary()[0])
        with RustainTUI(fresh=True, build=False, workspace=workspace, env_overrides=env) as tui, \
                (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            before = tui.get_screen_text()
            assert not agents_frame_ok(before) and not adapters_frame_ok(before)
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "result": "PENDING"}) + "\n")
            tui.send(CTRL_X + "s")
            agents = tui.wait_until(agents_frame_ok, "the Agents tab with its empty-state copy", timeout=15)
            assert not any("Needs you (" in row for row in _rows(agents))
            tui.send("\x1b")
            tui.wait_until(lambda s: _frame_top(_rows(s), AGENTS_TITLE) is None, "the hub closed", timeout=10)
            tui.send(CTRL_X + "a")
            adapters = tui.wait_until(adapters_frame_ok, "the Adapters window with the port rows", timeout=15)
            assert not agents_frame_ok(adapters)
            tui.send("\x1b")
            after = tui.wait_until(lambda s: _frame_top(_rows(s), "Adapters") is None, "the window closed", timeout=10)
            for name, frame in (("stage-before", before), ("agents-open", agents),
                                ("adapters-open", adapters), ("stage-after", after)):
                (evidence / f"{name}.txt").write_text(frame, encoding="utf-8")
            assert stage_rows(after)[:3] == stage_rows(before)[:3], f"Esc must restore the Stage:\n{before}\n{after}"
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "result": "PASS"}) + "\n")


def _plan_session(session_id: str) -> dict:
    """The saved session leg D opens (the fixture generator): the 21-9 stage-polish session's
    envelope with one finished plan of three tasks (the second has a markdown-free result)."""
    base = json.loads((Path(__file__).parent / "fixtures" / "sessions" / "story21_9_stage_polish.json").read_text())
    task = {"description": "", "dependsOn": [], "toolCalls": [], "waitingOn": [], "subTasks": []}
    plan = {
        "id": "p1", "title": "Release", "status": "completed", "createdAt": 1700000,
        "tasks": [
            {**task, "number": 1, "title": "Draft notes", "status": "completed",
             "result": {"text": "Notes **drafted**", "toolCallCount": 1}},
            {**task, "number": 2, "title": TASK_ROW, "status": "completed",
             "result": {"text": TASK_RESULT, "toolCallCount": 1}},
            {**task, "number": 3, "title": "Publish", "status": "pending"},
        ],
    }
    return {**base, "id": session_id, "title": "Plan smoke", "messages": [], "turns": [], "plans": {"p1": plan}}


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_d_tasks_tab_and_task_window_at_80x24(monkeypatch):
    """Leg D (7-D): a saved session with a plan; `Ctrl+X t` -> the Tasks tab with the task rows;
    select task 2, `Enter` -> `Task 2 · <title>` with the rendered result row; `c` copies the
    result to the clipboard file; `Esc` twice closes the window and the hub. Hermetic."""
    monkeypatch.setattr(harness, "TERM_COLS", 80)
    monkeypatch.setattr(harness, "TERM_ROWS", 24)
    session_id = "story21-7-plan"
    with tempfile.TemporaryDirectory(prefix="21-7-d-") as root:
        root = Path(root)
        workspace = root / "workspace"
        sessions = workspace / ".claude" / "sessions"
        sessions.mkdir(parents=True)
        (sessions / f"{session_id}.meta.json").write_text(json.dumps(_plan_session(session_id)))
        (workspace / ".env").write_text("")
        home = root / "home"
        home.mkdir()
        data = root / "data"
        data.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file",
               "RUSTAIN_DATA_DIR": str(data)}
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-d"
        evidence.mkdir(parents=True, exist_ok=True)
        clipboard = data / "clipboard.txt"
        binary = str(harness._resolve_binary()[0])
        with RustainTUI(fresh=False, build=False, workspace=workspace, env_overrides=env,
                        extra_args=["--session", session_id]) as tui, \
                (evidence / "raw.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            before = tui.get_screen_text()
            assert not tasks_frame_ok(before) and not task_window_ok(before)
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "result": "PENDING"}) + "\n")
            tui.send(CTRL_X + "t")
            tasks = tui.wait_until(tasks_frame_ok, "the Tasks tab with the seeded rows", timeout=15)
            assert not any("Needs you (" in row for row in _rows(tasks))
            # The first row is selected on open: one `j` selects task 2 (structural wait on the
            # marker row, no sleeps).
            assert any(re.search(r"▸ .*1\. ", row) for row in _rows(tasks)), tasks
            tui.send("j")
            tui.wait_until(lambda s: any(re.search(r"▸ .*2\. .*" + TASK_ROW, row) for row in _rows(s)),
                           "task 2 selected", timeout=10)
            tui.send("\r")
            window = tui.wait_until(task_window_ok, "the task window with the rendered result", timeout=15)
            tui.send("c")
            deadline = time.monotonic() + 10
            while not clipboard.exists() and time.monotonic() < deadline:
                tui.wait_until(lambda s: True, "idle", timeout=1)
            assert clipboard.exists(), f"`c` must write the clipboard file:\n{tui.get_screen_text()}"
            assert clipboard.read_bytes() == TASK_RESULT.encode("utf-8")
            tui.send("\x1b")
            tui.wait_until(lambda s: _frame_top(_rows(s), f"Task 2 · {TASK_ROW}") is None, "the window closed", timeout=10)
            tui.send("\x1b")
            after = tui.wait_until(lambda s: _frame_top(_rows(s), TASKS_TITLE) is None, "the hub closed", timeout=10)
            for name, frame in (("stage-before", before), ("tasks-open", tasks),
                                ("task-open", window), ("stage-after", after)):
                (evidence / f"{name}.txt").write_text(frame, encoding="utf-8")
            assert stage_rows(after)[:3] == stage_rows(before)[:3], f"Esc must restore the Stage:\n{before}\n{after}"
            (evidence / "run.json").write_text(json.dumps(
                {"viewport": [80, 24], "binary": binary, "result": "PASS"}) + "\n")

def _history_session(session_id: str, title: str, updated: int, with_turns: bool) -> dict:
    base = json.loads((Path(__file__).parent / "fixtures" / "sessions" / "story21_9_stage_polish.json").read_text())
    session = {**base, "id": session_id, "title": title, "updatedAt": updated, "createdAt": updated}
    if not with_turns:
        session = {**session, "messages": [], "turns": []}
    return session


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
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


def _start_leg_f(monkeypatch, workspace, env, cols, rows, session_id):
    monkeypatch.setattr(harness, "TERM_COLS", cols)
    monkeypatch.setattr(harness, "TERM_ROWS", rows)
    return RustainTUI(fresh=False, build=False, workspace=workspace, env_overrides=env,
                      extra_args=["--session", session_id])


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_f_monitor_toggle_at_130x30_and_remedy_at_80x24(monkeypatch):
    """Leg F (7-F, declared 130x30: the monitor exists only from 120 columns): a saved session
    with a plan; `Ctrl+X w` -> the right column titled `Monitor · Tasks` with the task rows
    beside the intact main column; `Ctrl+X w` again -> the column is gone. A second process at
    80x24: `Ctrl+X w` -> the width remedy toast and no column. Hermetic."""
    session_id = "story21-7-plan"
    with tempfile.TemporaryDirectory(prefix="21-7-f-") as root:
        root = Path(root)
        workspace = root / "workspace"
        sessions = workspace / ".claude" / "sessions"
        sessions.mkdir(parents=True)
        (sessions / f"{session_id}.meta.json").write_text(json.dumps(_plan_session(session_id)))
        (workspace / ".env").write_text("")
        home = root / "home"
        home.mkdir()
        data = root / "data"
        data.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file",
               "RUSTAIN_DATA_DIR": str(data)}
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-f"
        evidence.mkdir(parents=True, exist_ok=True)
        binary = str(harness._resolve_binary()[0])
        with _start_leg_f(monkeypatch, workspace, env, 130, 30, session_id) as tui, \
                (evidence / "raw-130x30.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            off = tui.wait_until(monitor_off_ok, "no monitor column by default", timeout=15)
            tui.send(CTRL_X + "w")
            on = tui.wait_until(monitor_column_ok, "the Monitor · Tasks column beside the main column", timeout=15)
            tui.send(CTRL_X + "w")
            gone = tui.wait_until(monitor_off_ok, "the column is gone after the second toggle", timeout=15)
            for name, frame in (("monitor-off", off), ("monitor-on", on), ("monitor-toggled-off", gone)):
                (evidence / f"{name}-130x30.txt").write_text(frame, encoding="utf-8")
        with _start_leg_f(monkeypatch, workspace, env, 80, 24, session_id) as tui, \
                (evidence / "raw-80x24.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            tui.send(CTRL_X + "w")
            remedy = tui.wait_until(monitor_remedy_ok, "the width remedy toast and no column", timeout=15)
            (evidence / "monitor-remedy-80x24.txt").write_text(remedy, encoding="utf-8")
        (evidence / "run.json").write_text(json.dumps(
            {"viewports": [[130, 30], [80, 24]], "binary": binary, "result": "PASS"}) + "\n")


@pytest.mark.skipif(DESKCHECK, reason="live run is skipped during the offline desk-check")
@pytest.mark.flaky(reruns=0)
@pytest.mark.timeout(240)
def test_leg_g_mission_strip_at_80x24_and_the_yield_at_60x16(monkeypatch):
    """Leg G (7-G): a saved session with a plan; the strip row above the status row reads
    `Tasks 2/3`; `Ctrl+X w` raises the width remedy toast on the row directly above the strip.
    A second process at 60x16: an eight-line draft makes the strip yield and `Ctrl+X h` still
    paints the five-row History frame. Hermetic."""
    session_id = "story21-7-plan"
    with tempfile.TemporaryDirectory(prefix="21-7-g-") as root:
        root = Path(root)
        workspace = root / "workspace"
        sessions = workspace / ".claude" / "sessions"
        sessions.mkdir(parents=True)
        (sessions / f"{session_id}.meta.json").write_text(json.dumps(_plan_session(session_id)))
        (workspace / ".env").write_text("")
        home = root / "home"
        home.mkdir()
        data = root / "data"
        data.mkdir()
        env = {"HOME": str(home), "RUSTAIN_PROFILE": "", "RUSTAIN_CLIPBOARD": "file",
               "RUSTAIN_DATA_DIR": str(data)}
        evidence_parent = Path(os.environ.get("RUSTAIN_CAPTURE_EVIDENCE_DIR", tempfile.gettempdir()))
        evidence = evidence_parent / "21-7-leg-g"
        evidence.mkdir(parents=True, exist_ok=True)
        binary = str(harness._resolve_binary()[0])
        with _start_leg_f(monkeypatch, workspace, env, 80, 24, session_id) as tui, \
                (evidence / "raw-80x24.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            strip = tui.wait_until(lambda s: strip_row_ok(s, STRIP_TASKS), "the Tasks strip above the status row", timeout=15)
            tui.send(CTRL_X + "w")
            toast = tui.wait_until(strip_toast_ok, "the remedy toast directly above the strip", timeout=15)
            for name, frame in (("strip", strip), ("strip-toast", toast)):
                (evidence / f"{name}-80x24.txt").write_text(frame, encoding="utf-8")
        with _start_leg_f(monkeypatch, workspace, env, 60, 16, session_id) as tui, \
                (evidence / "raw-60x16.pty").open("w", encoding="utf-8", newline="") as raw:
            tui.child.logfile_read = raw
            assert tui.wait_for_screen("Ready", timeout=15), tui.get_screen_text()
            tui.wait_until(lambda s: strip_row_ok(s, STRIP_TASKS), "the strip with an empty composer", timeout=15)
            for number in range(8):
                if number:
                    tui.send(ALT_ENTER)
                tui.send(f"draft {number}")
            tui.wait_until(lambda s: harness.strip_row(s) == "" and "draft 7" in s,
                           "the strip yielded to the eight-line draft", timeout=15)
            tui.send(CTRL_X + "h")
            yielded = tui.wait_until(strip_yield_ok, "the five-row History frame without a strip", timeout=15)
            (evidence / "strip-yield-60x16.txt").write_text(yielded, encoding="utf-8")
        (evidence / "run.json").write_text(json.dumps(
            {"viewports": [[80, 24], [60, 16]], "binary": binary, "result": "PASS"}) + "\n")
